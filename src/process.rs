//! Shared bounded-subprocess helpers.
//!
//! `output_with_timeout` runs a child process to completion under a wall-clock
//! deadline, draining stdout/stderr on reader threads into capped buffers so a
//! helper that fills a pipe cannot deadlock, and killing-and-reaping the child
//! on overrun. Both the `tokenCommand` credential path (`config.rs`) and the
//! `gh-stack link` hook (`submit/stack_link.rs`) go through it, so the drain,
//! deadline-polling, and cap logic live in one place.

/// Run `command` to completion with a wall-clock `timeout`. Returns `Ok(None)`
/// if the child overran the deadline (it is then killed and reaped). Std-only:
/// stdout/stderr are drained on reader threads so a helper that fills a pipe
/// buffer cannot deadlock, and the child is polled against the deadline.
pub(crate) fn output_with_timeout(
    mut command: std::process::Command,
    timeout: core::time::Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::process::Stdio;

    // Cap each stream so a runaway helper cannot balloon memory during the
    // window; a token is well under this. Reading is capped, not the pipe, so
    // the child still runs - we simply stop buffering past the cap.
    const MAX_STREAM_BYTES: usize = 1 << 20; // 1 MiB
    // How long to wait for the reader threads to drain AFTER the child has
    // exited (normal path) or been killed. A reader can only outlive this if a
    // descendant inherited the pipe fd and is holding it open; we must not
    // block on that, so we abandon (detach) the readers past this grace and
    // return what we have. This is what keeps the timeout honest.
    let drain_grace = core::time::Duration::from_millis(500);

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;

    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");
    // Read each stream on its own thread into a SHARED buffer, appending as
    // bytes arrive. Sharing - rather than returning a thread-local buffer only
    // when the thread finishes - is what lets us recover an already-read token
    // when a lingering descendant holds the pipe open and read never sees EOF.
    let (stdout_buf, stdout_reader) = spawn_capped_reader(stdout_pipe, MAX_STREAM_BYTES);
    let (stderr_buf, stderr_reader) = spawn_capped_reader(stderr_pipe, MAX_STREAM_BYTES);

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                // The one loop exit that must still reap the child; the reader
                // threads are detached (their bytes, if any, stay in the shared
                // buffers - we just discard the Output on this error path).
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            // Do NOT wait on the readers here: the Output is discarded, and a
            // descendant that inherited the pipe fd would keep the reader
            // blocked past EOF, voiding the very timeout this branch enforces.
            // Detach them (a bounded thread leak until the fd closes is
            // strictly better than an unbounded hang).
            return Ok(None);
        }
        std::thread::sleep(core::time::Duration::from_millis(10));
    };

    // Child has exited. Give the readers a bounded grace to hit EOF, then take
    // whatever they have buffered - finished or not. A descendant that
    // inherited the pipe fd keeps read from ever seeing EOF, but the helper has
    // already exited, so the token it printed is sitting in the shared buffer:
    // detach the still-blocked reader yet keep its bytes. `stderr` is captured
    // but resolved_token never reads it.
    wait_bounded(&stdout_reader, drain_grace);
    wait_bounded(&stderr_reader, drain_grace);
    let stdout = core::mem::take(
        &mut *stdout_buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let stderr = core::mem::take(
        &mut *stderr_buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    Ok(Some(std::process::Output {
        status,
        stdout,
        stderr,
    }))
}

/// Spawn a thread draining `reader` into a shared buffer, appending each chunk
/// as it arrives and stopping once `cap` bytes are buffered (the child keeps
/// running; we simply stop buffering past the cap). Returns the shared buffer
/// and the thread handle. Appending into a *shared* buffer - not a thread-local
/// one returned only on completion - is what lets the caller recover bytes
/// already read when the thread is later detached.
pub(crate) fn spawn_capped_reader<R: std::io::Read + Send + 'static>(
    mut reader: R,
    cap: usize,
) -> (
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    std::thread::JoinHandle<()>,
) {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&shared);
    let handle = std::thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        let mut total: usize = 0;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let take = n.min(cap.saturating_sub(total));
                    if take > 0 {
                        let mut buf = sink
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        buf.extend_from_slice(&chunk[..take]);
                        total = total.saturating_add(take);
                    }
                    if total >= cap {
                        break; // hit the cap; stop buffering
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    (shared, handle)
}

/// Wait for a reader thread to finish, giving up after `grace`. A reader only
/// overruns if a descendant of the child inherited the pipe fd and holds it
/// open; we detach it in that case. Detaching loses nothing: the bytes read so
/// far already live in the shared buffer.
fn wait_bounded(handle: &std::thread::JoinHandle<()>, grace: core::time::Duration) {
    let start = std::time::Instant::now();
    while !handle.is_finished() {
        if start.elapsed() >= grace {
            return; // detach; bytes already read are in the shared buffer
        }
        std::thread::sleep(core::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_with_timeout_kills_overrunning_child() {
        // A helper that would block far longer than the timeout is killed and
        // reported as an overrun (None), fast - it does not wait out the sleep.
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
        // A command that finishes within the deadline yields its real output.
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
        // The child exits fast but leaves a backgrounded descendant that
        // inherited the stdout fd and holds it for 30s. read_to_end would never
        // see EOF, so an unbounded join would hang forever - the bounded drain
        // must still return within the grace, well under the descendant's life.
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
        // The exact credential-discard case: the child PRINTS a token and then
        // leaves a backgrounded descendant holding the stdout fd open. The
        // reader never sees EOF and is detached at the grace - but the token was
        // already read, so it must survive in the shared buffer rather than be
        // dropped as empty output.
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
}
