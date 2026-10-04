//! Shared bounded-subprocess helpers.
//!
//! `output_with_timeout` runs a child process to completion under a wall-clock
//! deadline, draining stdout/stderr on reader threads into capped buffers so a
//! helper that fills a pipe cannot deadlock, and killing-and-reaping the child
//! on overrun. These details stay shared by subprocess callers.

/// Run `command` to completion with a wall-clock `timeout`. Returns `Ok(None)`
/// if the child overran the deadline (it is then killed and reaped). Std-only:
/// stdout/stderr are drained on reader threads so a helper that fills a pipe
/// buffer cannot deadlock, and the child is polled against the deadline.
pub(crate) fn output_with_timeout(
    mut command: std::process::Command,
    timeout: core::time::Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::process::Stdio;

    const MAX_STREAM_BYTES: usize = 1 << 20;
    // A reader can outlive the child if a descendant inherited the pipe and
    // keeps it open. Bound the drain wait and return the bytes read so far.
    let drain_grace = core::time::Duration::from_millis(500);

    // Keep buffers shared so bytes read before EOF stalls remain available.
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");
    // Read each stream on its own thread into a shared buffer, appending as
    // bytes arrive. This lets us recover an already-read token when a lingering
    // descendant holds the pipe open and read never sees EOF.
    let (stdout_buf, stdout_reader) = spawn_capped_reader(stdout_pipe, MAX_STREAM_BYTES);
    let (stderr_buf, stderr_reader) = spawn_capped_reader(stderr_pipe, MAX_STREAM_BYTES);

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                if let Err(cleanup_error) = stop_child(&mut child) {
                    return Err(std::io::Error::other(format!(
                        "polling child failed: {error}; stopping and reaping it failed: {cleanup_error}"
                    )));
                }
                return Err(error);
            }
        }
        if start.elapsed() >= timeout {
            stop_child(&mut child)?;
            return Ok(None);
        }
        std::thread::sleep(core::time::Duration::from_millis(10));
    };

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
/// Stop and reap the child, tolerating a race where it already exited.
fn stop_child(child: &mut std::process::Child) -> std::io::Result<()> {
    match child.kill() {
        Ok(()) => child.wait().map(|_| ()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
            ) =>
        {
            child.wait().map(|_| ())
        }
        Err(error) => Err(error),
    }
}
/// Read a child pipe into a capped buffer while continuing to drain it.
fn spawn_capped_reader<R>(
    mut reader: R,
    cap: usize,
) -> (
    std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    std::thread::JoinHandle<()>,
)
where
    R: std::io::Read + Send + 'static,
{
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&shared);
    let handle = std::thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        let mut total: usize = 0;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let take = n.min(cap.saturating_sub(total));
                    if take > 0 {
                        let mut buf = sink
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        buf.extend_from_slice(&chunk[..take]);
                        total = total.saturating_add(take);
                    }
                    // Stop buffering at the cap but keep draining the pipe.
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    (shared, handle)
}

/// Wait for a reader thread to finish, giving up after `grace`. It can outlive
/// the child only when a descendant inherited the pipe fd and holds it open.
fn wait_bounded(handle: &std::thread::JoinHandle<()>, grace: core::time::Duration) {
    let start = std::time::Instant::now();
    while !handle.is_finished() {
        if start.elapsed() >= grace {
            return;
        }
        std::thread::sleep(core::time::Duration::from_millis(5));
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
}
