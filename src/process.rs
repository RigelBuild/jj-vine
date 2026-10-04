//! Shared bounded-subprocess helpers.
//!
//! `output_with_timeout` runs a child process to completion under a wall-clock
//! deadline and captures stdout/stderr into capped buffers, so a helper that
//! fills a pipe cannot deadlock. The child runs in its own process group on
//! Unix or job object on Windows. When the child exits or overruns, remaining
//! members of that group or job are killed. A Unix descendant that leaves its
//! process group can outlive the call, but cannot keep a pipe reader blocked:
//! output is drained for at most [`DRAIN_GRACE`] and then the pipe is closed.

use core::time::Duration;
use std::process::{Command, Stdio};

/// Capture cap per stream. Bytes past the cap are read and dropped so a
/// chatty child cannot fill the pipe and stall.
const MAX_STREAM_BYTES: usize = 1 << 20;
/// After the tree is killed, how long to keep reading bytes still queued in
/// the pipes. Only a Unix descendant that left the process group on purpose
/// (`setsid`/`setpgid`) can hold a pipe open past this. Its pipe is then
/// closed on our side.
const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// How often the child is checked for exit.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Run `command` to completion with a wall-clock `timeout`. Returns `Ok(None)`
/// if the child overran the deadline.
///
/// The child and descendants still in its process group or job are killed when
/// the child exits or overruns, and the child is reaped. A Unix descendant can
/// leave its process group; its lifetime is not controlled, but inherited
/// output pipes are drained for at most [`DRAIN_GRACE`] and then closed. No
/// thread or pipe handle outlives the call.
///
/// The child gets its own process group on Unix (via `process_group`) and is
/// created suspended inside a job object on Windows (via `creation_flags`).
/// A caller-set `process_group` or `creation_flags` on `command` is
/// overridden.
pub(crate) fn output_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    platform::run(command, timeout)
}

/// Append `bytes` to a capture buffer, up to [`MAX_STREAM_BYTES`].
fn append_capped(buf: &mut Vec<u8>, bytes: &[u8]) {
    let take = bytes.len().min(MAX_STREAM_BYTES.saturating_sub(buf.len()));
    if let Some(head) = bytes.get(..take) {
        buf.extend_from_slice(head);
    }
}

#[cfg(unix)]
mod platform {
    use core::time::Duration;
    use std::{
        io,
        os::{fd::AsRawFd, unix::process::CommandExt as _},
        process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Output},
        time::Instant,
    };

    use super::{DRAIN_GRACE, POLL_INTERVAL, append_capped};

    pub(super) fn run(mut command: Command, timeout: Duration) -> io::Result<Option<Output>> {
        // A new process group: its id is the child's pid, and every
        // descendant stays in it unless it calls setsid/setpgid on purpose.
        command.process_group(0);
        let mut child = command.spawn()?;
        let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
            child.kill().ok();
            child.wait().ok();
            return Err(io::Error::other("child pid does not fit pid_t"));
        };
        let mut streams = Streams {
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            out: Vec::new(),
            err: Vec::new(),
        };
        let mut tree = Tree {
            child,
            pgid,
            reaped: false,
        };

        let start = Instant::now();
        let status = loop {
            if tree.has_exited()? {
                break tree.kill_and_reap()?;
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                tree.kill_and_reap()?;
                return Ok(None);
            }
            streams.pump(POLL_INTERVAL.min(timeout.saturating_sub(elapsed)))?;
        };

        // The tree is dead, so its pipe ends are closed and EOF follows the
        // bytes still queued. Only an escaped descendant can delay EOF.
        let drain_start = Instant::now();
        while streams.is_open() {
            let elapsed = drain_start.elapsed();
            if elapsed >= DRAIN_GRACE {
                break;
            }
            streams.pump(DRAIN_GRACE.saturating_sub(elapsed))?;
        }
        Ok(Some(Output {
            status,
            stdout: streams.out,
            stderr: streams.err,
        }))
    }

    /// The child and its process group. Dropping an unreaped tree kills the
    /// group and reaps the child, so every early return cleans up.
    struct Tree {
        child: Child,
        pgid: libc::pid_t,
        reaped: bool,
    }

    impl Tree {
        /// Whether the child has exited, without reaping it. The unreaped
        /// zombie keeps its pid, so the group id cannot be reused before
        /// `kill_and_reap` signals the group.
        fn has_exited(&self) -> io::Result<bool> {
            // SAFETY: an all-zero `siginfo_t` is a valid value; it is plain
            // data.
            let mut info: libc::siginfo_t = unsafe { core::mem::zeroed() };
            // SAFETY: `info` is a valid, writable `siginfo_t`. WNOWAIT leaves
            // the child waitable for the later `Child::wait`.
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if rc == -1_i32 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(false);
                }
                return Err(error);
            }
            // SAFETY: waitid filled `info` for a child state change, or left
            // it zeroed when no child was waitable.
            let pid = unsafe { info.si_pid() };
            // Accept only termination codes: some platforms report a stopped
            // child here even with WEXITED alone.
            Ok(pid != 0
                && matches!(
                    info.si_code,
                    libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
                ))
        }

        /// Kill every process left in the group, then reap the child.
        fn kill_and_reap(&mut self) -> io::Result<ExitStatus> {
            // SAFETY: killpg has no memory-safety preconditions. The group id
            // is the unreaped child's pid, so it names our group only. ESRCH
            // (group already empty) is expected and ignored.
            unsafe { libc::killpg(self.pgid, libc::SIGKILL) };
            // A child that left the group (setsid) is not reached by killpg.
            match self.child.kill() {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
                Err(error) => return Err(error),
            }
            let status = self.child.wait()?;
            self.reaped = true;
            Ok(status)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            if !self.reaped {
                // SAFETY: as in `kill_and_reap`; the child is not reaped yet.
                unsafe { libc::killpg(self.pgid, libc::SIGKILL) };
                self.child.kill().ok();
                self.child.wait().ok();
            }
        }
    }

    /// Both output pipes, read on the calling thread with `poll`. A pipe is
    /// set to `None` (closed) at EOF or on a read error.
    struct Streams {
        stdout: Option<ChildStdout>,
        stderr: Option<ChildStderr>,
        out: Vec<u8>,
        err: Vec<u8>,
    }

    impl Streams {
        fn is_open(&self) -> bool {
            self.stdout.is_some() || self.stderr.is_some()
        }

        /// Wait up to `wait` for either pipe to be readable, then read once
        /// from each readable pipe.
        fn pump(&mut self, wait: Duration) -> io::Result<()> {
            if !self.is_open() {
                std::thread::sleep(wait);
                return Ok(());
            }
            // poll ignores entries with a negative fd.
            let mut fds = [
                pollfd(self.stdout.as_ref().map(AsRawFd::as_raw_fd)),
                pollfd(self.stderr.as_ref().map(AsRawFd::as_raw_fd)),
            ];
            // Round up so a sub-millisecond wait does not busy-loop.
            let millis = wait.as_millis().max(1);
            let millis = libc::c_int::try_from(millis).unwrap_or(libc::c_int::MAX);
            // SAFETY: `fds` is a valid array of `fds.len()` pollfd entries.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, millis) };
            if rc == -1_i32 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(());
                }
                return Err(error);
            }
            if fds[0].revents != 0 {
                read_ready(&mut self.stdout, &mut self.out);
            }
            if fds[1].revents != 0 {
                read_ready(&mut self.stderr, &mut self.err);
            }
            Ok(())
        }
    }

    fn pollfd(fd: Option<libc::c_int>) -> libc::pollfd {
        libc::pollfd {
            fd: fd.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        }
    }

    /// Read once from a pipe that `poll` reported ready. A ready pipe does not
    /// block on read.
    fn read_ready<R>(slot: &mut Option<R>, buf: &mut Vec<u8>)
    where
        R: io::Read,
    {
        let Some(pipe) = slot.as_mut() else {
            return;
        };
        let mut chunk = [0_u8; 8192];
        match pipe.read(&mut chunk) {
            Ok(0) => *slot = None,
            Ok(n) => append_capped(buf, chunk.get(..n).unwrap_or_default()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => *slot = None,
        }
    }
}

#[cfg(windows)]
mod platform {
    use core::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };
    use std::{
        io,
        os::windows::{
            io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle, RawHandle},
            process::CommandExt as _,
        },
        process::{Child, Command, Output},
        sync::Arc,
        thread::JoinHandle,
        time::Instant,
    };

    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot,
                TH32CS_SNAPTHREAD,
                THREADENTRY32,
                Thread32First,
                Thread32Next,
            },
            IO::CancelIoEx,
            JobObjects::{
                AssignProcessToJobObject,
                CreateJobObjectW,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation,
                SetInformationJobObject,
                TerminateJobObject,
            },
            Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    use super::{DRAIN_GRACE, POLL_INTERVAL, append_capped};

    pub(super) fn run(mut command: Command, timeout: Duration) -> io::Result<Option<Output>> {
        // Start suspended so the child cannot create a process before it is
        // in the job; every process it creates later joins the job too.
        command.creation_flags(CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            // The suspended child has created no process yet.
            child.kill().ok();
            child.wait().ok();
            return Err(io::Error::other("child output was not piped"));
        };
        // Dropping `job` (KILL_ON_JOB_CLOSE) kills every process in it, so
        // every return below kills the tree.
        let job = match contain(&child) {
            Ok(job) => job,
            Err(error) => {
                child.kill().ok();
                child.wait().ok();
                return Err(error);
            }
        };
        let stdout = Reader::spawn(stdout);
        let stderr = Reader::spawn(stderr);

        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    terminate(&job);
                    child.wait().ok();
                    finish(stdout, stderr, Duration::ZERO);
                    return Err(error);
                }
            }
            if start.elapsed() >= timeout {
                terminate(&job);
                let reaped = child.wait();
                finish(stdout, stderr, Duration::ZERO);
                reaped?;
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL);
        };

        terminate(&job);
        let (stdout, stderr) = finish(stdout, stderr, DRAIN_GRACE);
        Ok(Some(Output {
            status,
            stdout,
            stderr,
        }))
    }

    /// Kill every process still in the job. On failure the job handle's
    /// `KILL_ON_JOB_CLOSE` kills them when `job` drops.
    fn terminate(job: &OwnedHandle) {
        // SAFETY: `job` is a live job handle.
        unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    }

    /// Put the suspended child in a new kill-on-close job, then resume it.
    /// The job does not allow breakaway, so no descendant can leave it.
    fn contain(child: &Child) -> io::Result<OwnedHandle> {
        // SAFETY: null security attributes and a null name are allowed.
        let raw = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a new job handle that nothing else owns.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(io::Error::other)?;
        // SAFETY: the pointer and size describe `limits`, the struct this
        // information class expects.
        let set = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size,
            )
        };
        if set == 0_i32 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both handles are live for this call.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0_i32
        {
            return Err(io::Error::last_os_error());
        }
        resume_main_thread(child.id())?;
        Ok(job)
    }

    /// Resume the only thread of a process created suspended. Std does not
    /// expose the main thread handle on stable, so find it by snapshot.
    fn resume_main_thread(pid: u32) -> io::Result<()> {
        // SAFETY: no pointer arguments; the result is checked.
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a new snapshot handle that nothing else owns.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut entry = THREADENTRY32 {
            dwSize: u32::try_from(size_of::<THREADENTRY32>()).map_err(io::Error::other)?,
            ..THREADENTRY32::default()
        };
        // SAFETY: `entry` is a writable THREADENTRY32 with dwSize set.
        let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) } != 0_i32;
        while found {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: no pointer arguments; the result is checked.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: `thread` is a new thread handle that nothing else owns.
                let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
                // SAFETY: `thread` is a live handle with THREAD_SUSPEND_RESUME.
                if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            // SAFETY: as for Thread32First.
            found = unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) } != 0_i32;
        }
        Err(io::Error::other("suspended child has no thread to resume"))
    }

    /// A pipe drained on its own thread. The thread hands the pipe back on
    /// join, so `raw` stays a live handle until `finish` joins the thread.
    struct Reader<R> {
        raw: RawHandle,
        stop: Arc<AtomicBool>,
        thread: JoinHandle<(R, Vec<u8>)>,
    }

    impl<R> Reader<R>
    where
        R: io::Read + std::os::windows::io::AsRawHandle + Send + 'static,
    {
        fn spawn(mut pipe: R) -> Self {
            let raw = pipe.as_raw_handle();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0_u8; 8192];
                while !stop_flag.load(Ordering::Acquire) {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => append_capped(&mut buf, chunk.get(..n).unwrap_or_default()),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                (pipe, buf)
            });
            Self { raw, stop, thread }
        }

        /// Make the thread finish. After `stop` is set, the thread can start
        /// at most one more read, and `CancelIoEx` is repeated until that read
        /// is cancelled, so this loop ends.
        fn stop(&self) {
            self.stop.store(true, Ordering::Release);
            while !self.thread.is_finished() {
                // SAFETY: the thread owns the pipe until it is joined, so
                // `raw` is live. A null OVERLAPPED cancels all its I/O.
                unsafe { CancelIoEx(self.raw, core::ptr::null()) };
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn join(self) -> Vec<u8> {
            self.thread
                .join()
                .map(|(_pipe, buf)| buf)
                .unwrap_or_default()
        }
    }

    /// Wait up to `grace` for both readers to reach EOF, cancel any still
    /// blocked, and join both. Returns (stdout, stderr).
    fn finish<O, E>(stdout: Reader<O>, stderr: Reader<E>, grace: Duration) -> (Vec<u8>, Vec<u8>)
    where
        O: io::Read + std::os::windows::io::AsRawHandle + Send + 'static,
        E: io::Read + std::os::windows::io::AsRawHandle + Send + 'static,
    {
        let start = Instant::now();
        while !(stdout.thread.is_finished() && stderr.thread.is_finished())
            && start.elapsed() < grace
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        stdout.stop();
        stderr.stop();
        (stdout.join(), stderr.join())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_with_timeout_kills_overrunning_child() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let start = std::time::Instant::now();
        let result = output_with_timeout(command, core::time::Duration::from_millis(200))
            .expect("spawn/poll must not error");
        assert!(result.is_none(), "an overrunning child must report None");
        assert!(
            start.elapsed() < core::time::Duration::from_secs(5),
            "must return promptly after the timeout, not wait out the sleep"
        );
    }

    #[test]
    fn output_with_timeout_returns_fast_command_output() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf fast-tok"]);
        let output = output_with_timeout(command, core::time::Duration::from_secs(10))
            .expect("spawn/poll must not error")
            .expect("a fast command must return output, not time out");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"fast-tok");
    }

    #[test]
    fn output_with_timeout_returns_despite_descendant_holding_pipe() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 30 & exit 0"]);
        let start = std::time::Instant::now();
        let output = output_with_timeout(command, core::time::Duration::from_secs(10))
            .expect("spawn/poll must not error")
            .expect("the child exited 0, so this is the normal path, not a timeout");
        assert!(output.status.success());
        assert!(
            start.elapsed() < core::time::Duration::from_secs(5),
            "must return within the drain grace, not block on the descendant's inherited fd"
        );
    }

    #[test]
    fn output_with_timeout_preserves_token_when_descendant_holds_pipe() {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "printf sekret-tok; sleep 30 & exit 0"]);
        let start = std::time::Instant::now();
        let output = output_with_timeout(command, core::time::Duration::from_secs(10))
            .expect("spawn/poll must not error")
            .expect("the child exited 0, so this is the normal path, not a timeout");
        assert!(output.status.success());
        assert_eq!(
            output.stdout, b"sekret-tok",
            "a token read before the descendant blocked EOF must not be discarded"
        );
        assert!(
            start.elapsed() < core::time::Duration::from_secs(5),
            "must return within the drain grace, not block on the descendant's inherited fd"
        );
    }

    /// Poll `kill -0 <pid>` until the process is gone or `within` elapses.
    #[cfg(unix)]
    fn process_gone_within(pid: &str, within: core::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < within {
            let alive = std::process::Command::new("kill")
                .args(["-0", pid])
                .stderr(std::process::Stdio::null())
                .status()
                .expect("kill -0 must run")
                .success();
            if !alive {
                return true;
            }
            std::thread::sleep(core::time::Duration::from_millis(20));
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_terminates_descendant_holding_pipe_after_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("descendant.pid");
        let mut command = std::process::Command::new("sh");
        // The descendant inherits stdout/stderr, records its pid, then keeps the
        // pipes open long past the child's exit.
        command
            .args([
                "-c",
                "printf sekret-tok; sh -c 'echo $$ > \"$1\"; exec sleep 30' _ \"$1\" & \
                 while [ ! -s \"$1\" ]; do sleep 0.01; done; exit 0",
                "_",
            ])
            .arg(&pid_file);
        let start = std::time::Instant::now();
        let output = output_with_timeout(command, core::time::Duration::from_secs(10))
            .expect("spawn/poll must not error")
            .expect("the child exited 0, so this is the normal path, not a timeout");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"sekret-tok");
        assert!(
            start.elapsed() < core::time::Duration::from_secs(5),
            "the helper must not wait for the descendant to exit naturally"
        );
        let pid = std::fs::read_to_string(&pid_file).expect("descendant wrote its pid");
        assert!(
            process_gone_within(pid.trim(), core::time::Duration::from_secs(5)),
            "a descendant holding the output pipe must not outlive the bounded call"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_terminates_descendant_on_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("descendant.pid");
        let mut command = std::process::Command::new("sh");
        command
            .args([
                "-c",
                "sh -c 'echo $$ > \"$1\"; exec sleep 30' _ \"$1\" & sleep 30",
                "_",
            ])
            .arg(&pid_file);
        let start = std::time::Instant::now();
        let result = output_with_timeout(command, core::time::Duration::from_millis(500))
            .expect("spawn/poll must not error");
        assert!(result.is_none(), "an overrunning child must report None");
        assert!(start.elapsed() < core::time::Duration::from_secs(5));
        let pid = std::fs::read_to_string(&pid_file).expect("descendant wrote its pid");
        assert!(
            process_gone_within(pid.trim(), core::time::Duration::from_secs(5)),
            "a timed-out child's descendants must be terminated too"
        );
    }
}
