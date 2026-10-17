//! External approver — an approval gate that asks another program.
//!
//! A headless agent (a CI job, a cron task, a server process) has no screen
//! for Touch ID, Windows Hello or polkit. Instead of failing every payment,
//! the account owner can name a program that decides: it might page a phone,
//! post to a chat, or check a dashboard. pay writes the request to the
//! program's stdin as JSON and reads one answer: exit status `0` approves,
//! anything else denies.
//!
//! The gate fails closed. A program that cannot start, exits non-zero, is
//! killed by a signal or does not answer within the timeout is a denial. The
//! timeout covers delivering the request as well as the answer, and a
//! timed-out program is killed and reaped, along with any helpers it started
//! on Unix, so it cannot keep a request open. On Unix the approver runs in
//! its own process group, and once it answers, anything it left running in
//! that group is killed too.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::{AuthGate, AuthIntent, Error, Result};

/// How long an approver may take when the account does not say.
pub const DEFAULT_APPROVER_TIMEOUT: Duration = Duration::from_secs(120);

/// Version of the JSON request written to the approver's stdin.
pub const APPROVER_REQUEST_VERSION: u32 = 1;

const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// An [`AuthGate`] that delegates the decision to an external program.
#[derive(Debug, Clone)]
pub struct ExternalApprover {
    program: PathBuf,
    args: Vec<String>,
    timeout: Duration,
}

impl ExternalApprover {
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            args,
            timeout,
        }
    }

    /// The JSON document the approver receives on stdin.
    pub fn request_json(intent: &AuthIntent) -> String {
        serde_json::json!({
            "version": APPROVER_REQUEST_VERSION,
            "kind": intent_kind(intent),
            "message": intent.message(),
            "amount_minor_units": intent.amount_minor_units(),
            "moves_money": intent.moves_money(),
            "recurring": intent.is_recurring(),
        })
        .to_string()
    }

    fn name(&self) -> String {
        self.program.display().to_string()
    }

    /// Run the approver on `request` and decide from its exit status.
    fn run(&self, request: Vec<u8>) -> Result<()> {
        // A bare or relative name would be resolved through the caller's
        // `PATH` or working directory, which the caller controls.
        if !self.program.is_absolute() {
            return Err(Error::AuthDenied(format!(
                "approver `{}` must be an absolute path",
                self.name()
            )));
        }
        // Checked before spawning, so a timeout too large to represent never
        // starts a program it could not bound.
        let deadline = Instant::now().checked_add(self.timeout).ok_or_else(|| {
            Error::AuthDenied(format!(
                "approver `{}` timeout of {}s is too large",
                self.name(),
                self.timeout.as_secs()
            ))
        })?;

        let mut command = Command::new(&self.program);
        // None of the caller's environment (`LD_PRELOAD`, `PATH`, secrets)
        // and none of its working directory reach the approver.
        command
            .args(&self.args)
            .env_clear()
            .envs(approver_env())
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // Its own process group, so a timeout can kill helpers it started too.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn().map_err(|e| {
            Error::AuthDenied(format!("approver `{}` could not start: {e}", self.name()))
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            // Written on its own thread so the deadline also covers delivery:
            // an approver that holds stdin open without reading cannot stall
            // us on a full pipe. Killing it closes the pipe and ends the
            // thread. An approver may decide without reading the request; a
            // closed pipe is not an error, its exit status still decides.
            let writer = std::thread::Builder::new()
                .name("pay-approver-stdin".into())
                .spawn(move || {
                    let _ = stdin.write_all(&request);
                });
            if let Err(e) = writer {
                kill_tree(&mut child);
                return Err(Error::AuthDenied(format!(
                    "approver `{}` could not be sent the request: {e}",
                    self.name()
                )));
            }
        }

        let status = self.wait_until(&mut child, deadline)?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::AuthDenied(format!(
                "approver `{}` declined ({status})",
                self.name()
            )))
        }
    }

    fn wait_until(&self, child: &mut Child, deadline: Instant) -> Result<ExitStatus> {
        loop {
            if exited(child)? {
                // The approver has answered. Stop what it left behind in its
                // group, which may still hold the request pipe open and keep
                // the writer thread blocked, then reap it.
                kill_group(child);
                return Ok(child.wait()?);
            }
            if Instant::now() >= deadline {
                kill_tree(child);
                return Err(Error::AuthDenied(format!(
                    "approver `{}` did not answer within {}s",
                    self.name(),
                    self.timeout.as_secs()
                )));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

/// `PATH` for the approver: fixed, never the caller's.
#[cfg(unix)]
pub const APPROVER_PATH: &str = "/usr/bin:/bin";

/// The whole environment the approver starts with. Nothing comes from the
/// caller unchecked: `PATH` is fixed, `HOME` comes from the user database
/// rather than the caller's `HOME`, and `LANG` passes only as a plain locale
/// name, since glibc reads a name with a `/` as a path to locale data.
fn approver_env() -> Vec<(&'static str, OsString)> {
    let mut env = Vec::new();
    #[cfg(unix)]
    {
        env.push(("PATH", OsString::from(APPROVER_PATH)));
        if let Some(home) = user_home() {
            env.push(("HOME", home));
        }
    }
    // Windows programs commonly fail to start without it.
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        env.push(("SystemRoot", root));
    }
    if let Some(lang) = std::env::var("LANG").ok().filter(|l| is_plain_locale(l)) {
        env.push(("LANG", OsString::from(lang)));
    }
    env
}

/// A locale name such as `en_US.UTF-8` or `C.UTF-8`: no path separators,
/// nothing that could name a file.
fn is_plain_locale(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

/// The effective user's home directory from the user database.
#[cfg(unix)]
fn user_home() -> Option<OsString> {
    use std::os::unix::ffi::OsStrExt;

    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: passwd is plain data; all zeroes is a valid value.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call, and `buf.len()` is the
        // size of the buffer `getpwuid_r` may write strings into.
        let rc = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        // SAFETY: on success `pw_dir` points at a NUL-terminated string in `buf`.
        let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
        return Some(std::ffi::OsStr::from_bytes(dir.to_bytes()).to_owned());
    }
}

/// Whether the approver has exited. On Unix this leaves it unreaped, so its
/// pid, and with it the id of the process group it leads, stays reserved
/// until [`kill_group`] has run and we reap it.
#[cfg(unix)]
fn exited(child: &mut Child) -> std::io::Result<bool> {
    let pid = libc::id_t::try_from(child.id()).map_err(std::io::Error::other)?;
    // SAFETY: siginfo_t is plain data; all zeroes is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, writable siginfo_t for the whole call.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc == -1 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(err);
    }
    // With WNOHANG and nothing to report, si_pid keeps the zero we wrote.
    // SAFETY: si_pid is either that zero or set by waitid for an exited child.
    Ok(unsafe { info.si_pid() } != 0)
}

#[cfg(not(unix))]
fn exited(child: &mut Child) -> std::io::Result<bool> {
    Ok(child.try_wait()?.is_some())
}

/// Kill every process in the approver's group (Unix only).
///
/// Only called while the approver is unreaped: an exited child stays a zombie
/// until it is waited for, and a zombie keeps its pid, so the group id it
/// leads cannot be handed to an unrelated process before this signal lands.
/// A group whose leader we have already reaped is never signalled.
fn kill_group(child: &Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: kill(2) takes no pointers. See above for why the group id
        // cannot have been reused.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// Kill the approver and every process in its group, then reap it.
fn kill_tree(child: &mut Child) {
    kill_group(child);
    let _ = child.kill();
    let _ = child.wait();
}

impl AuthGate for ExternalApprover {
    fn authenticate(&self, intent: &AuthIntent) -> Result<()> {
        self.run(Self::request_json(intent).into_bytes())
    }

    fn is_available(&self) -> bool {
        self.program.is_absolute() && self.program.is_file()
    }
}

fn intent_kind(intent: &AuthIntent) -> &'static str {
    match intent {
        AuthIntent::AuthorizePayment { .. } => "authorize_payment",
        AuthIntent::AuthorizeBatch { .. } => "authorize_batch",
        AuthIntent::CreateAccount(_) => "create_account",
        AuthIntent::ImportAccount(_) => "import_account",
        AuthIntent::ExportAccount(_) => "export_account",
        AuthIntent::DeleteAccount(_) => "delete_account",
        AuthIntent::OpenSession(_) => "open_session",
        AuthIntent::UseGatewayFeePayer(_) => "use_gateway_fee_payer",
        AuthIntent::UseAccount(_) => "use_account",
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn payment() -> AuthIntent {
        AuthIntent::authorize_payment_details("$0.05", "accessing API", "api.example.com")
    }

    fn gate(program: PathBuf, timeout: Duration) -> ExternalApprover {
        ExternalApprover::new(program, Vec::new(), timeout)
    }

    #[test]
    fn exit_zero_approves() {
        let dir = tempfile::tempdir().unwrap();
        let approver = gate(
            script(&dir, "yes.sh", "cat >/dev/null; exit 0"),
            DEFAULT_APPROVER_TIMEOUT,
        );
        assert!(approver.authenticate(&payment()).is_ok());
    }

    #[test]
    fn non_zero_exit_denies() {
        let dir = tempfile::tempdir().unwrap();
        let approver = gate(script(&dir, "no.sh", "exit 1"), DEFAULT_APPROVER_TIMEOUT);
        let err = approver.authenticate(&payment()).unwrap_err();
        assert!(matches!(err, Error::AuthDenied(m) if m.contains("declined")));
    }

    #[test]
    fn killed_approver_denies() {
        let dir = tempfile::tempdir().unwrap();
        let approver = gate(
            script(&dir, "crash.sh", "kill -9 $$"),
            DEFAULT_APPROVER_TIMEOUT,
        );
        assert!(matches!(
            approver.authenticate(&payment()),
            Err(Error::AuthDenied(_))
        ));
    }

    #[test]
    fn timeout_denies_and_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let body = format!("echo $$ > {}; exec sleep 30", pid_file.display());
        let approver = gate(script(&dir, "slow.sh", &body), Duration::from_millis(300));

        let started = Instant::now();
        let err = approver.authenticate(&payment()).unwrap_err();
        assert!(matches!(err, Error::AuthDenied(m) if m.contains("did not answer")));
        assert!(started.elapsed() < Duration::from_secs(5));

        let pid = std::fs::read_to_string(&pid_file).unwrap();
        assert!(gone(pid.trim()), "timed-out approver must be killed");
    }

    #[test]
    fn timeout_kills_helpers_the_approver_started() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("helper-pid");
        // No `exec`: the shell waits on a separate helper process.
        let body = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
        let approver = gate(script(&dir, "helper.sh", &body), Duration::from_millis(300));

        let err = approver.authenticate(&payment()).unwrap_err();
        assert!(matches!(err, Error::AuthDenied(m) if m.contains("did not answer")));

        let pid = std::fs::read_to_string(&pid_file).unwrap();
        assert!(
            gone(pid.trim()),
            "helper of a timed-out approver must be killed"
        );
    }

    #[test]
    fn answered_approver_leaves_no_helper_holding_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("helper-pid");
        // The helper keeps the request pipe open without reading it (a
        // background job's stdin is /dev/null, hence fd 3), and the approver
        // approves at once, leaving the writer blocked on a full pipe.
        let body = format!(
            "exec 3<&0; sleep 30 <&3 3<&- & echo $! > {}; exit 0",
            pid_file.display()
        );
        let approver = gate(script(&dir, "leaves.sh", &body), DEFAULT_APPROVER_TIMEOUT);

        approver.run(vec![b'x'; 4 << 20]).unwrap();

        let pid = std::fs::read_to_string(&pid_file).unwrap();
        assert!(
            gone(pid.trim()),
            "helper left by an approver must be killed"
        );
    }

    #[test]
    fn timeout_covers_a_request_the_approver_never_reads() {
        let dir = tempfile::tempdir().unwrap();
        // Holds stdin open and never reads it, so a large request fills the pipe.
        let approver = gate(
            script(&dir, "deaf.sh", "exec sleep 30"),
            Duration::from_millis(300),
        );

        let started = Instant::now();
        let err = approver.run(vec![b'x'; 4 << 20]).unwrap_err();
        assert!(matches!(err, Error::AuthDenied(m) if m.contains("did not answer")));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn oversized_timeout_denies_without_starting_the_approver() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let body = format!("touch {}; exit 0", marker.display());
        let approver = gate(script(&dir, "never.sh", &body), Duration::MAX);

        let err = approver.authenticate(&payment()).unwrap_err();
        assert!(matches!(err, Error::AuthDenied(m) if m.contains("too large")));
        assert!(!marker.exists(), "approver must not start");
    }

    /// Whether `pid` has stopped within a couple of seconds. A killed helper
    /// is reparented to init, which may reap it late or, in a container with
    /// no reaping init, never; a zombie counts as stopped.
    fn gone(pid: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !running(pid) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(target_os = "linux")]
    fn running(pid: &str) -> bool {
        // The state follows the command name, which ends at the last `)`.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let (_, rest) = stat.rsplit_once(')')?;
                rest.split_whitespace().next().map(|state| state != "Z")
            })
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    fn running(pid: &str) -> bool {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    #[test]
    fn missing_program_denies_and_is_unavailable() {
        let approver = gate(
            PathBuf::from("/nonexistent/pay-approver"),
            DEFAULT_APPROVER_TIMEOUT,
        );
        assert!(!approver.is_available());
        let err = approver.authenticate(&payment()).unwrap_err();
        assert!(matches!(err, Error::AuthDenied(m) if m.contains("could not start")));
    }

    #[test]
    fn bare_or_relative_command_is_refused() {
        for program in ["sh", "pay-approver", "./approve", "bin/approve"] {
            let approver = gate(PathBuf::from(program), DEFAULT_APPROVER_TIMEOUT);
            assert!(!approver.is_available(), "{program} must be unavailable");
            let err = approver.authenticate(&payment()).unwrap_err();
            assert!(
                matches!(&err, Error::AuthDenied(m) if m.contains("must be an absolute path")),
                "{program}: {err}"
            );
        }
    }

    #[test]
    fn plain_locale_names_pass_and_paths_do_not() {
        for ok in [
            "C",
            "C.UTF-8",
            "en_US.UTF-8",
            "de_DE@euro",
            "sr_RS.UTF-8@latin",
        ] {
            assert!(is_plain_locale(ok), "{ok}");
        }
        for bad in [
            "",
            "/tmp/evil",
            "../../tmp/evil",
            "..",
            ".hidden",
            "en US",
            "a/b",
        ] {
            assert!(!is_plain_locale(bad), "{bad:?}");
        }
    }

    /// Set by [`caller_environment_never_reaches_the_approver`] when it
    /// re-runs this test binary with a hostile environment.
    const PROBE_OUT: &str = "PAY_APPROVER_TEST_PROBE_OUT";

    #[test]
    fn caller_environment_never_reaches_the_approver() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("env.txt");
        let planted = dir.path().join("planted");
        std::fs::create_dir(&planted).unwrap();

        // Run the probe in a fresh process, so the hostile environment never
        // touches this one (setting variables in a threaded test is unsound).
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "external::tests::env_probe",
                "--exact",
                "--ignored",
                "--test-threads=1",
            ])
            .env(PROBE_OUT, &out)
            // glibc only warns about a missing LD_PRELOAD library and runs
            // the probe anyway. DYLD_INSERT_LIBRARIES is left out: macOS
            // aborts a process whose inserted library is missing, so the
            // probe would never run. env_clear() drops it like any other
            // variable, which the rest of this test covers.
            .env("LD_PRELOAD", "/nonexistent/evil.so")
            .env("PATH", format!("{}:/usr/bin:/bin", planted.display()))
            .env("HOME", &planted)
            .env("LANG", "/tmp/evil-locale")
            .env("PAY_TEST_REMOTE_WALLET_SECRET", "hunter2")
            .current_dir(&planted)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "probe failed");

        let seen = std::fs::read_to_string(&out).unwrap();
        let var = |name: &str| {
            seen.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
                .map(str::to_owned)
        };
        for name in ["LD_PRELOAD", "PAY_TEST_REMOTE_WALLET_SECRET"] {
            assert_eq!(var(name), None, "{name} reached the approver");
        }
        assert!(
            !seen.contains("hunter2"),
            "secret value reached the approver"
        );
        assert_eq!(var("PATH").as_deref(), Some(APPROVER_PATH));
        assert_eq!(var("LANG"), None, "a path-like LANG must not pass");
        assert_ne!(
            var("HOME").as_deref(),
            Some(planted.to_str().unwrap()),
            "HOME must come from the user database, not the caller"
        );
        assert_eq!(var("CWD").as_deref(), Some("/"), "approver must run from /");
    }

    /// The approver half of [`caller_environment_never_reaches_the_approver`];
    /// does nothing when run on its own.
    #[test]
    #[ignore = "run by caller_environment_never_reaches_the_approver"]
    fn env_probe() {
        let Some(out) = std::env::var_os(PROBE_OUT) else {
            return;
        };
        let out = PathBuf::from(out);
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            "env > '{0}'; echo \"CWD=$(pwd)\" >> '{0}'; exit 0",
            out.display()
        );
        let approver = gate(script(&dir, "probe.sh", &body), DEFAULT_APPROVER_TIMEOUT);
        approver.authenticate(&payment()).unwrap();
    }

    #[test]
    fn existing_program_is_available() {
        let dir = tempfile::tempdir().unwrap();
        let approver = gate(script(&dir, "ok.sh", "exit 0"), DEFAULT_APPROVER_TIMEOUT);
        assert!(approver.is_available());
    }

    #[test]
    fn approver_receives_the_request_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("request.json");
        let body = format!("cat > {}; exit 0", out.display());
        let approver = gate(script(&dir, "record.sh", &body), DEFAULT_APPROVER_TIMEOUT);
        approver.authenticate(&payment()).unwrap();

        let request: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(request["version"], APPROVER_REQUEST_VERSION);
        assert_eq!(request["kind"], "authorize_payment");
        assert_eq!(request["amount_minor_units"], 500);
        assert_eq!(request["moves_money"], true);
        assert_eq!(request["recurring"], false);
        assert!(
            request["message"]
                .as_str()
                .unwrap()
                .contains("api.example.com")
        );
    }

    #[test]
    fn non_payment_intents_carry_no_amount() {
        let request: serde_json::Value = serde_json::from_str(&ExternalApprover::request_json(
            &AuthIntent::export_account("main"),
        ))
        .unwrap();
        assert_eq!(request["kind"], "export_account");
        assert!(request["amount_minor_units"].is_null());
        assert_eq!(request["moves_money"], false);
    }
}
