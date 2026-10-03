use crate::{Error, Result};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Run a helper with bounded time and output. Both pipes are drained while the
/// child runs so pipe backpressure cannot prevent an otherwise successful
/// helper from exiting.
pub(crate) fn output(
    binary: &Path,
    args: &[&str],
    timeout: Duration,
    operation: &str,
) -> Result<Output> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Backend(format!("pay.sh: {e}")))?;

    let oversized = Arc::new(AtomicBool::new(false));
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_limit = Arc::clone(&oversized);
    let stderr_limit = Arc::clone(&oversized);
    let stdout_reader = std::thread::spawn(move || drain(stdout, stdout_limit));
    let stderr_reader = std::thread::spawn(move || drain(stderr, stderr_limit));

    let started = Instant::now();
    let status = loop {
        if oversized.load(Ordering::Relaxed) {
            break Err(Error::Backend(format!(
                "{operation} exceeded the {MAX_OUTPUT_BYTES}-byte output limit"
            )));
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() >= timeout => {
                break Err(Error::Backend(format!(
                    "{operation} timed out after {} seconds; the local approval prompt may not be visible",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => break Err(Error::Backend(format!("pay.sh: {e}"))),
        }
    };

    if status.is_err() {
        if let Err(e) = child.kill() {
            // The helper may have exited between try_wait and kill.
            if child.try_wait().ok().flatten().is_none() {
                return Err(Error::Backend(format!(
                    "{operation}: failed to stop pay.sh: {e}"
                )));
            }
        }
        child
            .wait()
            .map_err(|e| Error::Backend(format!("Failed to reap pay.sh: {e}")))?;
    }

    let stdout = join_reader(stdout_reader)?;
    let stderr = join_reader(stderr_reader)?;
    let status = status?;
    if oversized.load(Ordering::Relaxed) {
        return Err(Error::Backend(format!(
            "{operation} exceeded the {MAX_OUTPUT_BYTES}-byte output limit"
        )));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn drain(mut pipe: impl Read, oversized: Arc<AtomicBool>) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = pipe.read(&mut chunk)?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > MAX_OUTPUT_BYTES {
            oversized.store(true, Ordering::Relaxed);
            return Ok(output);
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

fn join_reader(reader: std::thread::JoinHandle<io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| Error::Backend("pay.sh output reader panicked".to_string()))?
        .map_err(|e| Error::Backend(format!("Failed to read pay.sh output: {e}")))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn executable_script(dir: &Path, name: &str, source: &str) -> std::path::PathBuf {
        let script = dir.join(name);
        fs::write(&script, source).expect("write test helper");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .expect("set helper permissions");
        script
    }

    #[test]
    fn preserves_success_and_failure_status() {
        let result = output(
            Path::new("/bin/echo"),
            &["yes"],
            Duration::from_secs(2),
            "test helper",
        )
        .expect("helper should finish");
        assert!(result.status.success());
        assert_eq!(result.stdout, b"yes\n");

        let result = output(
            Path::new("/usr/bin/false"),
            &[],
            Duration::from_secs(2),
            "test helper",
        )
        .expect("failed helper should return its status");
        assert!(!result.status.success());
    }

    #[test]
    fn drains_large_stdout_and_stderr() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let data = dir.path().join("data");
        fs::write(&data, vec![b'x'; 256 * 1024]).expect("write test output");
        let script = executable_script(
            dir.path(),
            "output.sh",
            &format!(
                "#!/bin/sh\n/bin/cat '{}'\n/bin/cat '{}' >&2\n",
                data.display(),
                data.display()
            ),
        );

        let result = output(&script, &[], Duration::from_secs(10), "test helper")
            .expect("finite output larger than pipe capacity should complete");
        assert!(result.status.success());
        assert_eq!(result.stdout.len(), 256 * 1024);
        assert_eq!(result.stderr.len(), 256 * 1024);
    }

    #[test]
    fn rejects_output_over_limit_and_reaps_child() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let data = dir.path().join("data");
        let pid_file = dir.path().join("pid");
        fs::write(&data, vec![b'x'; MAX_OUTPUT_BYTES + 1]).expect("write test output");
        let script = executable_script(
            dir.path(),
            "output.sh",
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nexec /bin/cat '{}'\n",
                pid_file.display(),
                data.display()
            ),
        );

        let error = output(&script, &[], Duration::from_secs(10), "test helper")
            .expect_err("oversized helper output must be rejected");
        assert!(error.to_string().contains("output limit"));
        assert_child_reaped(&pid_file, "oversized helper");
    }

    #[test]
    fn times_out_and_reaps_stuck_child() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let pid_file = dir.path().join("pid");
        let script = executable_script(
            dir.path(),
            "hang.sh",
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 60\n",
                pid_file.display()
            ),
        );

        let started = Instant::now();
        let error = output(
            &script,
            &[],
            Duration::from_secs(1),
            "Touch ID authentication",
        )
        .expect_err("stuck helper must time out");
        assert!(
            error
                .to_string()
                .contains("Touch ID authentication timed out")
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_child_reaped(&pid_file, "timed-out helper");
    }

    fn assert_child_reaped(pid_file: &Path, description: &str) {
        let pid = fs::read_to_string(pid_file).expect("helper started");
        let still_running = Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .expect("probe helper PID")
            .success();
        assert!(!still_running, "{description} must be reaped");
    }
}
