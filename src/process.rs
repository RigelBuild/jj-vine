//! Shared bounded-subprocess helpers.
//!
//! `output_with_timeout` runs a non-interactive child process to completion
//! under a wall-clock deadline and captures stdout/stderr into capped buffers,
//! so a helper that fills a pipe cannot deadlock. The child gets null stdin.
//!
//! Containment: the child runs in a new session and process group on Unix, or
//! in a job object on Windows. When the child exits (with any status),
//! overruns, or the call is cancelled, every process still in that group or
//! job is killed. A helper therefore cannot leave a background agent or daemon
//! running.
//!
//! Interactivity: on Unix the new session has no controlling terminal, so a
//! helper that opens `/dev/tty` to prompt gets an error at once and fails. It
//! is not stopped by job control and then reported as a timeout.
//!
//! Cancellation: on Unix, SIGINT, SIGTERM, and SIGHUP are caught while a child
//! runs, because the child's own session does not get the terminal's signals.
//! A signal is caught only if its disposition is the default on entry. The
//! handler kills every running tree; after the last run ends, the default
//! disposition is restored and the signal is raised again, so the process ends
//! as it would have with no child running. The default is restored only where
//! this module's handler is still installed: a handler other code installed
//! during the run is kept, and a caught signal is then raised to it. If that
//! handler returns, the signal is consumed and later runs are not cancelled.
//! A caught signal is classified when this module's handler first observes
//! the shared cancellation state: it cancels only the active period it
//! observed, and a call that observed no active period records nothing. A
//! chained call from another handler is classified when it reaches this
//! module's handler, not when the signal arrived.
//! On Windows, a console Ctrl-C reaches the child directly, and the job's
//! kill-on-close limit kills the tree when this process exits.
//!
//! Limits: a Unix descendant that leaves the process group on purpose
//! (`setsid`/`setpgid`) is not killed, but cannot keep a pipe reader blocked:
//! output is drained for at most [`DRAIN_GRACE`] and then the pipe is closed.
//! On Windows, a pipe read that cannot be cancelled within [`DRAIN_GRACE`] is
//! left to a detached reader thread, so the call itself stays bounded.

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
/// if the child overran the deadline. The clock is read after each exit
/// check, so an exit first seen at or after the deadline is an overrun and a
/// late child cannot return output.
///
/// The child and descendants still in its process group or job are killed when
/// the child exits or overruns, and the child is reaped. See the module docs
/// for the containment, interactivity, and cancellation policy.
///
/// On Unix the child starts a new session (via `pre_exec`), so a caller must
/// not set `process_group` on `command`: spawning would then fail. On Windows
/// the child is created suspended inside a job object (via `creation_flags`),
/// which overrides a caller-set `creation_flags`.
///
/// # Errors
///
/// Returns an error if the child cannot be spawned, contained, or reaped. On
/// Unix, a run cut short by a caught cancellation signal returns an
/// [`std::io::ErrorKind::Interrupted`] error; the signal ends the process when
/// the last concurrent run returns.
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

/// The result of one exit check, judged against the deadline.
enum Check<T> {
    /// The child exited before the deadline.
    Exited(T),
    /// The child is running; this much time is left.
    Running(Duration),
    /// The deadline passed, whether or not the child has exited.
    Overrun,
}

/// Run the exit check `observe`, then read the clock. Reading it after the
/// check makes an exit seen at or after the deadline an overrun.
fn check_exit<T>(
    start: std::time::Instant,
    timeout: Duration,
    observe: impl FnOnce() -> std::io::Result<Option<T>>,
) -> std::io::Result<Check<T>> {
    let exited = observe()?;
    let remaining = timeout.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return Ok(Check::Overrun);
    }
    Ok(exited.map_or(Check::Running(remaining), Check::Exited))
}

#[cfg(unix)]
mod platform {
    use core::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };
    use std::{
        io,
        os::{fd::AsRawFd, unix::process::CommandExt as _},
        process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Output},
        sync::{Mutex, PoisonError},
        time::Instant,
    };

    use super::{Check, DRAIN_GRACE, POLL_INTERVAL, append_capped, check_exit};

    pub(super) fn run(mut command: Command, timeout: Duration) -> io::Result<Option<Output>> {
        // A new session: the child leads a new process group, whose id is its
        // pid, and has no controlling terminal. Every descendant stays in the
        // group unless it calls setsid/setpgid on purpose. std applies a
        // caller-set `process_group` before this hook, which would make the
        // child a group leader and setsid fail, so callers must not set one.
        // SAFETY: the hook runs in the forked child before exec and calls only
        // setsid, which is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1_i32 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })
        };
        // Declared before `tree`, so it drops after the tree is reaped.
        let _cancel = CancelGuard::enter();
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
            let check = check_exit(start, timeout, || Ok(tree.has_exited()?.then_some(())))?;
            if matches!(check, Check::Exited(())) {
                break tree.kill_and_reap()?;
            }
            if pending_signal(CANCEL_WORD.load(Ordering::Acquire)) != 0_i32 {
                tree.kill_and_reap()?;
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "cancelled by signal",
                ));
            }
            let Check::Running(remaining) = check else {
                tree.kill_and_reap()?;
                return Ok(None);
            };
            streams.pump(POLL_INTERVAL.min(remaining))?;
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

    /// Signals that cancel a run. The child's new session does not get them
    /// from the terminal, so this process must stop the tree itself.
    const CANCEL_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    /// The cancellation state in one lock-free atomic word, so a handler call
    /// and an active-period transition are totally ordered. Bits 0..8 hold
    /// the first signal caught in the current active period, or 0; bit 8 is
    /// set while a period is active; bits 9..32 count handler calls in
    /// flight; bits 32..64 hold the generation of the latest period.
    ///
    /// A handler call classifies its signal by the word it observes when it
    /// enters: a signal is recorded only if a period was active then, and it
    /// is recorded against that period. The last run waits for handler calls
    /// in flight before it ends the period, so a call that observed the
    /// period cannot record after the period's signal was taken. A call that
    /// observed no active period records nothing, even if a new period
    /// starts before it finishes.
    static CANCEL_WORD: AtomicU64 = AtomicU64::new(0);

    const SIGNAL_MASK: u64 = 0xff;
    const ACTIVE_BIT: u64 = 1 << 8;
    const IN_FLIGHT_ONE: u64 = 1 << 9;
    const IN_FLIGHT_MASK: u64 = ((1 << 23) - 1) << 9;
    const GENERATION_SHIFT: u32 = 32;

    /// The pending signal held in `word`, or 0.
    fn pending_signal(word: u64) -> libc::c_int {
        libc::c_int::try_from(word & SIGNAL_MASK).unwrap_or(0_i32)
    }

    /// The generation held in `word`.
    const fn generation(word: u64) -> u64 {
        word >> GENERATION_SHIFT
    }

    /// Active runs and the signals whose handler this module installed.
    static CANCEL_STATE: Mutex<CancelState> = Mutex::new(CancelState {
        active: 0,
        installed: [false; 3],
    });

    struct CancelState {
        active: usize,
        installed: [bool; 3],
    }

    /// Records the signal only; the run loop does the cleanup. It uses only
    /// lock-free atomic operations, which are async-signal-safe.
    extern "C" fn on_cancel_signal(signal: libc::c_int) {
        HandlerCall::observe().record(signal);
    }

    /// One handler call, from its observation of the cancellation word to
    /// its record. Split in two so tests can order a transition between them.
    pub(super) struct HandlerCall {
        observed: u64,
    }

    impl HandlerCall {
        /// Enter the handler: count this call in flight and observe whether
        /// a period is active. The observation classifies the signal.
        pub(super) fn observe() -> Self {
            Self {
                observed: CANCEL_WORD.fetch_add(IN_FLIGHT_ONE, Ordering::AcqRel),
            }
        }

        /// Record `signal` against the observed period if one was active and
        /// no signal is pending yet, then leave the handler.
        pub(super) fn record(self, signal: libc::c_int) {
            let value = u64::try_from(signal)
                .ok()
                .filter(|value| *value != 0 && *value <= SIGNAL_MASK);
            if let Some(value) = value
                && self.observed & ACTIVE_BIT != 0
            {
                // The period this call observed stays active until the call
                // leaves, because `end_period` waits for it; the generation
                // check keeps the record tied to that period regardless.
                let period = generation(self.observed);
                let mut word = CANCEL_WORD.load(Ordering::Acquire);
                while word & ACTIVE_BIT != 0
                    && generation(word) == period
                    && word & SIGNAL_MASK == 0
                {
                    match CANCEL_WORD.compare_exchange_weak(
                        word,
                        word | value,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => break,
                        Err(current) => word = current,
                    }
                }
            }
            CANCEL_WORD.fetch_sub(IN_FLIGHT_ONE, Ordering::Release);
        }
    }

    /// Start a new active period: advance the generation and clear any
    /// signal. Handler calls in flight observed no active period, so they
    /// record nothing in this one.
    fn begin_period() {
        let mut word = CANCEL_WORD.load(Ordering::Acquire);
        loop {
            let next = generation(word).wrapping_add(1) << GENERATION_SHIFT;
            let started = next | ACTIVE_BIT | (word & IN_FLIGHT_MASK);
            match CANCEL_WORD.compare_exchange_weak(
                word,
                started,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => word = current,
            }
        }
    }

    /// End the active period and take its signal, or 0. Waits for handler
    /// calls in flight, so none that observed this period records after it.
    /// A handler call on this thread runs to completion before this resumes,
    /// so the wait cannot deadlock on it.
    fn end_period() -> libc::c_int {
        let mut word = CANCEL_WORD.load(Ordering::Acquire);
        loop {
            if word & IN_FLIGHT_MASK != 0 {
                std::thread::yield_now();
                word = CANCEL_WORD.load(Ordering::Acquire);
                continue;
            }
            let ended = word & !(ACTIVE_BIT | SIGNAL_MASK);
            match CANCEL_WORD.compare_exchange_weak(
                word,
                ended,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return pending_signal(word),
                Err(current) => word = current,
            }
        }
    }

    /// Catches the cancellation signals while at least one run is active.
    /// Only a signal whose disposition is the default on entry is caught, so
    /// an ignored signal or a handler installed by other code is left alone.
    pub(super) struct CancelGuard;

    impl CancelGuard {
        pub(super) fn enter() -> Self {
            let mut state = CANCEL_STATE.lock().unwrap_or_else(PoisonError::into_inner);
            if state.active == 0 {
                // Start the period before the handlers are installed, so a
                // signal caught after an install is recorded.
                begin_period();
                for (signal, installed) in CANCEL_SIGNALS.iter().zip(state.installed.iter_mut()) {
                    *installed = install_if_default(*signal);
                }
            }
            state.active = state.active.saturating_add(1);
            Self
        }
    }

    impl Drop for CancelGuard {
        fn drop(&mut self) {
            let mut state = CANCEL_STATE.lock().unwrap_or_else(PoisonError::into_inner);
            state.active = state.active.saturating_sub(1);
            if state.active != 0 {
                return;
            }
            for (signal, installed) in CANCEL_SIGNALS.iter().zip(state.installed.iter_mut()) {
                if core::mem::take(installed) {
                    restore_default_if_ours(*signal);
                }
            }
            // Take the signal, so a handler that returns does not cancel
            // later runs.
            let pending = end_period();
            if pending != 0_i32 {
                // Only a signal this module caught is recorded. Its
                // disposition is the default again, so this ends the process
                // as the original signal would have, unless other code
                // installed a handler during the run: that handler gets it.
                // SAFETY: raise has no memory-safety preconditions.
                unsafe { libc::raise(pending) };
            }
        }
    }

    /// `on_cancel_signal` as a disposition value.
    fn cancel_handler() -> libc::sighandler_t {
        let handler: extern "C" fn(libc::c_int) = on_cancel_signal;
        handler as libc::sighandler_t
    }

    /// Install `on_cancel_signal` for `signal` if its disposition is the
    /// default. Returns whether it was installed.
    fn install_if_default(signal: libc::c_int) -> bool {
        if current_disposition(signal) != Some(libc::SIG_DFL) {
            return false;
        }
        set_disposition(signal, cancel_handler())
    }

    /// Restore the default disposition for `signal` only if
    /// `on_cancel_signal` is still its handler. A disposition other code set
    /// during the run is left in place. POSIX has no compare-and-swap for a
    /// disposition, so a handler set between the query and the reset is still
    /// replaced; that window is two system calls wide.
    fn restore_default_if_ours(signal: libc::c_int) {
        if current_disposition(signal) == Some(cancel_handler()) {
            set_disposition(signal, libc::SIG_DFL);
        }
    }

    /// The current handler of `signal`, or `None` if it cannot be queried.
    pub(super) fn current_disposition(signal: libc::c_int) -> Option<libc::sighandler_t> {
        // SAFETY: an all-zero `sigaction` is a valid value; it is plain data.
        let mut old: libc::sigaction = unsafe { core::mem::zeroed() };
        // SAFETY: a null new action only queries; `old` is writable.
        if unsafe { libc::sigaction(signal, core::ptr::null(), &raw mut old) } == -1_i32 {
            return None;
        }
        Some(old.sa_sigaction)
    }

    /// Set `signal`'s handler with an empty mask and `SA_RESTART`. Returns
    /// whether the call succeeded.
    pub(super) fn set_disposition(signal: libc::c_int, handler: libc::sighandler_t) -> bool {
        // SAFETY: an all-zero `sigaction` is a valid value; it is plain data.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        action.sa_sigaction = handler;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: `action.sa_mask` is a writable sigset_t.
        unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
        // SAFETY: `action` is a valid sigaction; a null old action is allowed.
        unsafe { libc::sigaction(signal, &raw const action, core::ptr::null_mut()) == 0_i32 }
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

    use super::{Check, DRAIN_GRACE, POLL_INTERVAL, append_capped, check_exit};

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
            let check = match check_exit(start, timeout, || child.try_wait()) {
                Ok(check) => check,
                Err(error) => {
                    terminate(&job, &mut child).ok();
                    finish(stdout, stderr, Duration::ZERO);
                    return Err(error);
                }
            };
            match check {
                Check::Exited(status) => break status,
                Check::Running(remaining) => std::thread::sleep(POLL_INTERVAL.min(remaining)),
                Check::Overrun => {
                    let killed = terminate(&job, &mut child);
                    finish(stdout, stderr, Duration::ZERO);
                    killed?;
                    return Ok(None);
                }
            }
        };

        // The child has exited; its descendants are killed even though the
        // child succeeded, so no helper process outlives the call.
        let killed = terminate(&job, &mut child);
        let (stdout, stderr) = finish(stdout, stderr, DRAIN_GRACE);
        killed?;
        Ok(Some(Output {
            status,
            stdout,
            stderr,
        }))
    }

    /// Kill every process still in the job, then wait for the child. If the
    /// job cannot be terminated, the direct child is killed instead. The
    /// child is waited for only once a kill succeeded, so the wait cannot
    /// block on a live child. On error, the remaining processes die when
    /// `job` drops, by its `KILL_ON_JOB_CLOSE` limit.
    fn terminate(job: &OwnedHandle, child: &mut Child) -> io::Result<()> {
        // SAFETY: `job` is a live job handle.
        if unsafe { TerminateJobObject(job.as_raw_handle(), 1) } == 0_i32 {
            let error = io::Error::last_os_error();
            // `Child::kill` succeeds for a child that has already exited.
            child.kill()?;
            child.wait()?;
            return Err(error);
        }
        child.wait()?;
        Ok(())
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

    /// A pipe drained on its own thread. The thread owns the pipe until it
    /// ends, so `raw` stays a live handle while the thread runs. `raw` is used
    /// only before the thread is joined or detached.
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

        /// Ask the thread to finish by `deadline`. After `stop` is set, the
        /// thread can start at most one more read, and `CancelIoEx` is
        /// repeated until that read is cancelled or the deadline passes.
        fn stop(&self, deadline: Instant) {
            self.stop.store(true, Ordering::Release);
            while !self.thread.is_finished() && Instant::now() < deadline {
                // SAFETY: the thread is still running, so it still owns the
                // pipe and `raw` is live. A null OVERLAPPED cancels all its
                // I/O.
                unsafe { CancelIoEx(self.raw, core::ptr::null()) };
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// Join a finished thread and return its bytes. A thread still
        /// running is detached and its bytes are dropped: it owns and closes
        /// the pipe when its read ends, and `raw` is not used again.
        fn join(self) -> Vec<u8> {
            if !self.thread.is_finished() {
                return Vec::new();
            }
            self.thread
                .join()
                .map(|(_pipe, buf)| buf)
                .unwrap_or_default()
        }
    }

    /// Wait up to `grace` for both readers to reach EOF, then cancel any
    /// still blocked for at most another [`DRAIN_GRACE`], and join both.
    /// Returns (stdout, stderr). Bounded: never waits past both limits.
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
        let deadline = Instant::now() + DRAIN_GRACE;
        stdout.stop(deadline);
        stderr.stop(deadline);
        (stdout.join(), stderr.join())
    }
}

// These tests drive `sh` and POSIX utilities, so they run on Unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn output_with_timeout_runs_child_in_new_session() {
        let mut command = std::process::Command::new("sh");
        // Field 6 of /proc/<pid>/stat is the session id; `cut` inherits it.
        command.args(["-c", "echo $$; cut -d' ' -f6 /proc/self/stat"]);
        let output = output_with_timeout(command, core::time::Duration::from_secs(10))
            .expect("spawn/poll must not error")
            .expect("a fast command must return output, not time out");
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
        let mut lines = stdout.lines();
        let pid = lines.next().expect("child pid");
        let session = lines.next().expect("session id");
        assert_eq!(
            pid, session,
            "the child must lead a new session with no controlling terminal, so a \
             terminal prompt fails at once instead of stopping until the timeout"
        );
    }

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

    /// Poll until process `pid` is gone or `within` elapses. A zombie counts
    /// as gone: a killed descendant is reparented, and its new parent may
    /// reap it late or never, but a zombie runs no code and holds no pipe.
    /// `kill -0` succeeds on a zombie, so it cannot be the probe.
    fn process_gone_within(pid: &str, within: core::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < within {
            if !is_running(pid) {
                return true;
            }
            std::thread::sleep(core::time::Duration::from_millis(20));
        }
        false
    }

    /// Whether `pid` names a process that exists and is not a zombie. Field 3
    /// of `/proc/<pid>/stat` is the state; it follows the last `)`, because
    /// the command name in field 2 may hold spaces or parentheses.
    #[cfg(target_os = "linux")]
    fn is_running(pid: &str) -> bool {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(error) => panic!("cannot inspect process {pid}: {error}"),
        };
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .expect("process stat must contain a state");
        !matches!(state, 'Z' | 'X')
    }

    /// Whether `pid` names a process that exists and is not a zombie. `ps`
    /// prints nothing for a missing pid, and a state that starts with `Z`
    /// for a zombie.
    #[cfg(not(target_os = "linux"))]
    fn is_running(pid: &str) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .stderr(std::process::Stdio::null())
            .output()
            .expect("ps must run");
        assert!(
            output.status.success() || output.status.code() == Some(1),
            "ps failed to inspect process {pid}: {}",
            output.status
        );
        let state = String::from_utf8_lossy(&output.stdout);
        let state = state.trim();
        assert!(
            output.status.success() || state.is_empty(),
            "unexpected ps output"
        );
        !state.is_empty() && !state.starts_with('Z')
    }

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

    #[test]
    fn output_with_timeout_caps_large_stdout_and_stderr() {
        let size = MAX_STREAM_BYTES.saturating_add(0x1_0000);
        let mut command = std::process::Command::new("sh");
        command
            .args([
                "-c",
                "head -c \"$1\" /dev/zero; head -c \"$1\" /dev/zero >&2; exit 0",
                "_",
            ])
            .arg(size.to_string());
        let output = output_with_timeout(command, core::time::Duration::from_secs(30))
            .expect("spawn/poll must not error")
            .expect("a child that writes past the cap must still complete, not stall");
        assert!(output.status.success(), "the child must exit 0");
        assert_eq!(
            output.stdout.len(),
            MAX_STREAM_BYTES,
            "stdout must be capped, and bytes up to the cap kept"
        );
        assert_eq!(
            output.stderr.len(),
            MAX_STREAM_BYTES,
            "stderr must be capped, and bytes up to the cap kept"
        );
    }

    /// Set in a re-executed test binary to the directory a child-mode test
    /// uses. Child-mode tests do nothing without it.
    const CHILD_DIR_ENV: &str = "JJ_VINE_PROCESS_TEST_CHILD_DIR";

    /// Re-run this test binary with only the child-mode test `name`, so
    /// signals and process-global dispositions do not touch other tests.
    fn spawn_isolated(name: &str, dir: &std::path::Path) -> std::process::Child {
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_crate, rest)| rest);
        std::process::Command::new(std::env::current_exe().expect("test binary path"))
            .args(["--exact", &format!("{module}::{name}")])
            .args(["--ignored", "--test-threads=1"])
            .env(CHILD_DIR_ENV, dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("re-executing the test binary must spawn")
    }

    /// Wait until `path` holds a complete `echo $$` line; return the pid.
    fn wait_for_pid(path: &std::path::Path, within: core::time::Duration) -> Option<String> {
        let start = std::time::Instant::now();
        while start.elapsed() < within {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return Some(text.trim().to_owned());
            }
            std::thread::sleep(core::time::Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn output_with_timeout_cancels_tree_and_reraises_signal() {
        use std::os::unix::process::ExitStatusExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut parent = spawn_isolated("isolated_cancel_child", dir.path());
        let Some(descendant) = wait_for_pid(
            &dir.path().join("descendant.pid"),
            core::time::Duration::from_secs(10),
        ) else {
            parent.kill().ok();
            parent.wait().ok();
            panic!("the helper's descendant never started");
        };
        let helper = wait_for_pid(
            &dir.path().join("helper.pid"),
            core::time::Duration::from_secs(1),
        )
        .expect("the helper writes its pid before starting the descendant");
        let parent_pid = libc::pid_t::try_from(parent.id()).expect("pid fits pid_t");
        // SAFETY: kill has no memory-safety preconditions; the pid is our
        // unreaped child, so it cannot have been reused.
        assert_eq!(
            unsafe { libc::kill(parent_pid, libc::SIGTERM) },
            0_i32,
            "SIGTERM must reach the throwaway parent"
        );
        let status = parent.wait().expect("the throwaway parent must be reaped");
        assert_eq!(
            status.signal(),
            Some(libc::SIGTERM),
            "the parent must end by the original signal, after cleanup, as if no \
             helper had run; got {status:?}"
        );
        assert!(
            !dir.path().join("returned").exists(),
            "the cancelled run must not return normally to its caller"
        );
        assert!(
            process_gone_within(&helper, core::time::Duration::from_secs(5)),
            "the helper must be killed before the signal ends the parent"
        );
        assert!(
            process_gone_within(&descendant, core::time::Duration::from_secs(5)),
            "the helper's process group must be killed before the signal ends the parent"
        );
    }

    /// Child mode for `output_with_timeout_cancels_tree_and_reraises_signal`:
    /// run a helper and a group descendant until the parent test signals us.
    #[test]
    #[ignore = "child mode; run only by output_with_timeout_cancels_tree_and_reraises_signal"]
    fn isolated_cancel_child() {
        let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        assert!(
            platform::set_disposition(libc::SIGTERM, libc::SIG_DFL),
            "SIGTERM must start at the default disposition"
        );
        let mut command = std::process::Command::new("sh");
        command
            .args([
                "-c",
                "echo $$ > \"$1/helper.pid\"; \
                 sh -c 'echo $$ > \"$1/descendant.pid\"; exec sleep 30' _ \"$1\" & sleep 30",
                "_",
            ])
            .arg(&dir);
        let result = output_with_timeout(command, core::time::Duration::from_secs(30));
        // Reached only if the caught signal was not raised again.
        std::fs::write(dir.join("returned"), format!("{result:?}")).ok();
    }

    #[test]
    fn output_with_timeout_keeps_handler_installed_during_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut parent = spawn_isolated("isolated_keep_handler_child", dir.path());
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = parent.try_wait().expect("try_wait must not error") {
                break status;
            }
            if start.elapsed() > core::time::Duration::from_secs(30) {
                parent.kill().ok();
                parent.wait().ok();
                panic!("the child-mode test did not finish");
            }
            std::thread::sleep(core::time::Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "a SIGHUP handler installed while a helper ran must survive the run; \
             child-mode test failed with {status:?}"
        );
    }

    extern "C" fn embedder_handler(_signal: libc::c_int) {}

    /// Child mode for `output_with_timeout_keeps_handler_installed_during_run`:
    /// install a SIGHUP handler while the helper runs, as an embedding
    /// component could, and check the run does not replace it.
    #[test]
    #[ignore = "child mode; run only by output_with_timeout_keeps_handler_installed_during_run"]
    fn isolated_keep_handler_child() {
        let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        // Start from the default, so the run installs its own handler and
        // this test cannot pass by the run leaving SIGHUP alone.
        assert!(
            platform::set_disposition(libc::SIGHUP, libc::SIG_DFL),
            "SIGHUP must start at the default disposition"
        );
        let handler: extern "C" fn(libc::c_int) = embedder_handler;
        let handler = handler as libc::sighandler_t;
        let installer = {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                while !dir.join("ready").exists() {
                    assert!(
                        start.elapsed() < core::time::Duration::from_secs(10),
                        "the helper never started"
                    );
                    std::thread::sleep(core::time::Duration::from_millis(5));
                }
                let during = platform::current_disposition(libc::SIGHUP);
                assert!(
                    platform::set_disposition(libc::SIGHUP, handler),
                    "the embedder handler must install"
                );
                std::fs::write(dir.join("installed"), b"").expect("signal the helper");
                during
            })
        };
        let mut command = std::process::Command::new("sh");
        command
            .args([
                "-c",
                ": > \"$1/ready\"; while [ ! -e \"$1/installed\" ]; do sleep 0.01; done",
                "_",
            ])
            .arg(&dir);
        let output = output_with_timeout(command, core::time::Duration::from_secs(20))
            .expect("spawn/poll must not error")
            .expect("the helper exits once the handler is installed");
        let during = installer.join().expect("installer thread must not panic");
        assert!(output.status.success(), "the helper must exit 0");
        assert_ne!(
            during,
            Some(libc::SIG_DFL),
            "the run must have caught SIGHUP, or this test proves nothing"
        );
        assert_eq!(
            platform::current_disposition(libc::SIGHUP),
            Some(handler),
            "the run must not reset a handler installed while it ran"
        );
    }

    #[test]
    fn check_exit_treats_exit_seen_at_deadline_as_overrun() {
        let start = std::time::Instant::now();
        let timeout = core::time::Duration::from_millis(1);
        let check = check_exit(start, timeout, || {
            std::thread::sleep(core::time::Duration::from_millis(5));
            Ok(Some(()))
        })
        .expect("the exit check must not error");
        assert!(
            matches!(check, Check::Overrun),
            "an exit first observed after the deadline must not return output"
        );
    }

    #[test]
    fn output_with_timeout_consumes_signal_taken_by_returning_handler() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut parent = spawn_isolated("isolated_returning_handler_child", dir.path());
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = parent.try_wait().expect("try_wait must not error") {
                break status;
            }
            if start.elapsed() > core::time::Duration::from_secs(30) {
                parent.kill().ok();
                parent.wait().ok();
                panic!("the child-mode test did not finish");
            }
            std::thread::sleep(core::time::Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "a signal raised to a returning handler must not cancel a later run; \
             child-mode test failed with {status:?}"
        );
    }

    /// Child mode for
    /// `output_with_timeout_consumes_signal_taken_by_returning_handler`: catch
    /// SIGTERM, hand it to a returning embedder handler, then run a helper.
    #[test]
    #[ignore = "child mode; run only by output_with_timeout_consumes_signal_taken_by_returning_handler"]
    fn isolated_returning_handler_child() {
        if std::env::var_os(CHILD_DIR_ENV).is_none() {
            return;
        }
        assert!(
            platform::set_disposition(libc::SIGTERM, libc::SIG_DFL),
            "SIGTERM must start at the default disposition"
        );
        let handler: extern "C" fn(libc::c_int) = embedder_handler;
        let handler = handler as libc::sighandler_t;
        let guard = platform::CancelGuard::enter();
        assert_ne!(
            platform::current_disposition(libc::SIGTERM),
            Some(libc::SIG_DFL),
            "the guard must catch SIGTERM, or this test proves nothing"
        );
        // SAFETY: raise has no memory-safety preconditions. It signals
        // this thread, so the module's handler runs before it returns.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0_i32);
        assert!(
            platform::set_disposition(libc::SIGTERM, handler),
            "the embedder handler must install"
        );
        drop(guard);
        // Dropping the guard raised SIGTERM to the embedder handler, which
        // returned. The helper outlives the first exit check, so a stale
        // signal would cancel it.
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 0.2; printf after-signal"]);
        let output = output_with_timeout(command, core::time::Duration::from_secs(20))
            .expect("a consumed signal must not cancel a later run")
            .expect("the helper exits before the deadline");
        assert_eq!(output.stdout, b"after-signal");
    }

    #[test]
    fn output_with_timeout_ignores_signal_recorded_while_inactive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut parent = spawn_isolated("isolated_inactive_signal_child", dir.path());
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = parent.try_wait().expect("try_wait must not error") {
                break status;
            }
            if start.elapsed() > core::time::Duration::from_secs(30) {
                parent.kill().ok();
                parent.wait().ok();
                panic!("the child-mode test did not finish");
            }
            std::thread::sleep(core::time::Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "a signal recorded while no run was active must not cancel the next \
             run; child-mode test failed with {status:?}"
        );
    }

    /// The handler `chaining_handler` forwards to, as a disposition value.
    static CHAINED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

    /// An embedder handler that keeps the handler it replaced and forwards
    /// every signal to it, as a signal-chaining library does.
    extern "C" fn chaining_handler(signal: libc::c_int) {
        let previous = CHAINED.load(core::sync::atomic::Ordering::Acquire);
        if previous != 0 {
            // SAFETY: `CHAINED` holds only a value read back from
            // `sigaction` while the module's handler was installed, which is
            // an `extern "C" fn(c_int)` cast to `sighandler_t`.
            let previous = unsafe {
                core::mem::transmute::<libc::sighandler_t, extern "C" fn(libc::c_int)>(previous)
            };
            previous(signal);
        }
    }

    /// Child mode for
    /// `output_with_timeout_ignores_signal_recorded_while_inactive`: replace
    /// the module's SIGTERM handler with one that chains to it, end the
    /// active period, take SIGTERM while no run is active, then run a helper.
    #[test]
    #[ignore = "child mode; run only by output_with_timeout_ignores_signal_recorded_while_inactive"]
    fn isolated_inactive_signal_child() {
        if std::env::var_os(CHILD_DIR_ENV).is_none() {
            return;
        }
        assert!(
            platform::set_disposition(libc::SIGTERM, libc::SIG_DFL),
            "SIGTERM must start at the default disposition"
        );
        let guard = platform::CancelGuard::enter();
        let ours = platform::current_disposition(libc::SIGTERM)
            .expect("the SIGTERM disposition must be readable");
        assert_ne!(
            ours,
            libc::SIG_DFL,
            "the guard must catch SIGTERM, or this test proves nothing"
        );
        CHAINED.store(ours, core::sync::atomic::Ordering::Release);
        let handler: extern "C" fn(libc::c_int) = chaining_handler;
        assert!(
            platform::set_disposition(libc::SIGTERM, handler as libc::sighandler_t),
            "the chaining handler must install"
        );
        // The module's handler is no longer installed, so it is kept, and no
        // signal is pending, so nothing is raised.
        drop(guard);
        // No run is active. The chained call observes that and records
        // nothing.
        // SAFETY: raise has no memory-safety preconditions. It signals this
        // thread, so the handler runs before it returns.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0_i32);
        // The helper outlives the first exit check, so a stale signal would
        // cancel it.
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 0.2; printf after-signal"]);
        let output = output_with_timeout(command, core::time::Duration::from_secs(20))
            .expect("a signal recorded while inactive must not cancel a new run")
            .expect("the helper exits before the deadline");
        assert_eq!(output.stdout, b"after-signal");
    }

    #[test]
    fn output_with_timeout_reraises_signal_observed_before_last_drop() {
        use std::os::unix::process::ExitStatusExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let mut parent = spawn_isolated("isolated_last_drop_race_child", dir.path());
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = parent.try_wait().expect("try_wait must not error") {
                break status;
            }
            if start.elapsed() > core::time::Duration::from_secs(30) {
                parent.kill().ok();
                parent.wait().ok();
                panic!("the child-mode test did not finish");
            }
            std::thread::sleep(core::time::Duration::from_millis(20));
        };
        assert_eq!(
            status.signal(),
            Some(libc::SIGTERM),
            "a signal the handler observed while a run was active must end the \
             process when the last run ends; got {status:?}"
        );
    }

    /// Child mode for
    /// `output_with_timeout_reraises_signal_observed_before_last_drop`: a
    /// handler call observes the active period, the last guard drops on
    /// another thread, and only then does the handler call record.
    #[test]
    #[ignore = "child mode; run only by output_with_timeout_reraises_signal_observed_before_last_drop"]
    fn isolated_last_drop_race_child() {
        if std::env::var_os(CHILD_DIR_ENV).is_none() {
            return;
        }
        assert!(
            platform::set_disposition(libc::SIGTERM, libc::SIG_DFL),
            "SIGTERM must start at the default disposition"
        );
        let guard = platform::CancelGuard::enter();
        let call = platform::HandlerCall::observe();
        let ending = std::thread::spawn(move || drop(guard));
        // Time for the last drop to end the period first, if it does not
        // wait for a handler call already in flight.
        std::thread::sleep(core::time::Duration::from_millis(200));
        call.record(libc::SIGTERM);
        ending.join().expect("the dropping thread must not panic");
        // Reached only if the signal was lost: the parent test then fails.
    }

    #[test]
    fn output_with_timeout_ignores_signal_observed_before_period() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut parent = spawn_isolated("isolated_next_enter_race_child", dir.path());
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = parent.try_wait().expect("try_wait must not error") {
                break status;
            }
            if start.elapsed() > core::time::Duration::from_secs(30) {
                parent.kill().ok();
                parent.wait().ok();
                panic!("the child-mode test did not finish");
            }
            std::thread::sleep(core::time::Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "a signal the handler observed while no run was active must not \
             cancel a run that starts before it records; child-mode test failed \
             with {status:?}"
        );
    }

    /// Child mode for `output_with_timeout_ignores_signal_observed_before_period`:
    /// a handler call observes no active run, a new period starts, and only
    /// then does the handler call record.
    #[test]
    #[ignore = "child mode; run only by output_with_timeout_ignores_signal_observed_before_period"]
    fn isolated_next_enter_race_child() {
        if std::env::var_os(CHILD_DIR_ENV).is_none() {
            return;
        }
        assert!(
            platform::set_disposition(libc::SIGTERM, libc::SIG_DFL),
            "SIGTERM must start at the default disposition"
        );
        let call = platform::HandlerCall::observe();
        let guard = platform::CancelGuard::enter();
        call.record(libc::SIGTERM);
        // The helper outlives the first exit check, so a misattributed
        // signal would cancel it.
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "sleep 0.2; printf after-signal"]);
        let output = output_with_timeout(command, core::time::Duration::from_secs(20))
            .expect("a signal observed before the period must not cancel this run")
            .expect("the helper exits before the deadline");
        assert_eq!(output.stdout, b"after-signal");
        // No signal is pending, so this ends the period without raising.
        drop(guard);
    }
}

// Windows-native tests: they drive `powershell`, the job object, and
// `OpenProcess`. They do not cover console Ctrl-C delivery.
#[cfg(all(test, windows))]
mod windows_tests {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    use super::*;

    /// Whether process `pid` is gone, or exits, within `within`.
    fn process_gone_within(pid: u32, within: core::time::Duration) -> bool {
        // SAFETY: no pointer arguments; a null result means no such process.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return true;
        }
        let millis = u32::try_from(within.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `handle` is a live process handle with SYNCHRONIZE.
        let waited = unsafe { WaitForSingleObject(handle, millis) };
        // SAFETY: `handle` is live and not used again.
        unsafe { CloseHandle(handle) };
        waited == WAIT_OBJECT_0
    }

    #[test]
    fn output_with_timeout_kills_job_descendant_after_success() {
        let mut command = std::process::Command::new("powershell");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$p = Start-Process -FilePath ping -ArgumentList '-n','30','127.0.0.1' \
             -WindowStyle Hidden -PassThru; [Console]::Out.Write($p.Id)",
        ]);
        let output = output_with_timeout(command, core::time::Duration::from_secs(60))
            .expect("spawn/contain/wait must not error")
            .expect("powershell exits once the descendant starts");
        assert!(output.status.success(), "powershell must exit 0");
        let pid: u32 = String::from_utf8(output.stdout)
            .expect("utf-8 pid")
            .trim()
            .parse()
            .expect("powershell printed the descendant pid");
        assert!(
            process_gone_within(pid, core::time::Duration::from_secs(5)),
            "a descendant in the job must not outlive the call"
        );
    }

    #[test]
    fn output_with_timeout_kills_overrunning_job() {
        let mut command = std::process::Command::new("ping");
        command.args(["-n", "30", "127.0.0.1"]);
        let start = std::time::Instant::now();
        let result = output_with_timeout(command, core::time::Duration::from_millis(500))
            .expect("spawn/contain/wait must not error");
        assert!(result.is_none(), "an overrunning child must report None");
        assert!(
            start.elapsed() < core::time::Duration::from_secs(5),
            "must return promptly after the timeout, not wait out the ping"
        );
    }

    #[test]
    fn output_with_timeout_caps_large_stdout_and_stderr() {
        let size = MAX_STREAM_BYTES.saturating_add(0x1_0000);
        let mut command = std::process::Command::new("powershell");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "$s = 'x' * {size}; [Console]::Out.Write($s); [Console]::Out.Flush(); \
                 [Console]::Error.Write($s); [Console]::Error.Flush()"
            ),
        ]);
        let output = output_with_timeout(command, core::time::Duration::from_secs(60))
            .expect("spawn/contain/wait must not error")
            .expect("a child that writes past the cap must still complete, not stall");
        assert!(output.status.success(), "powershell must exit 0");
        assert_eq!(
            output.stdout.len(),
            MAX_STREAM_BYTES,
            "stdout must be capped"
        );
        assert_eq!(
            output.stderr.len(),
            MAX_STREAM_BYTES,
            "stderr must be capped"
        );
    }
}
