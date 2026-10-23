//! The agent's `buzz messages send`, without the Buzz signing key.
//!
//! A Buzz-managed agent is asked to answer with `buzz messages send`, but the
//! ACP adapter and the agent it runs never hold `BUZZ_PRIVATE_KEY` (a
//! compromised agent could carry it off). Instead the parent puts a `buzz`
//! shim first on the adapter's `PATH`. The shim runs `pay __acp-buzz-relay`,
//! which hands its arguments to a loopback listener in the parent, together
//! with a token minted for this launch. The parent runs the real `buzz` with
//! its own credentials, but only for `messages send` to the channel of a turn
//! the agent is answering, and returns its output and exit code.
//!
//! What the agent holds is the token: it can post replies while this launch
//! runs, where it is already asked to post. It cannot sign anything else,
//! read messages, or use the key after the launch ends.

use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use pay_acp::BuzzDeliveryTracker;
use serde::{Deserialize, Serialize};

/// First argument that turns `pay` into the shim's relay client.
pub(crate) const RELAY_ARG: &str = "__acp-buzz-relay";
const ADDR_ENV: &str = "PAY_ACP_BUZZ_RELAY";
const TOKEN_ENV: &str = "PAY_ACP_BUZZ_RELAY_TOKEN";
/// A message, its attachments excluded, fits well within this.
const MAX_REQUEST_BYTES: u64 = 4 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// The shim sends its whole request at once, so a few at a time suffice.
/// Excess connections are closed before any thread is started for them.
const MAX_CONNECTIONS: usize = 4;
/// Reading one request may take this long in total, however slowly the
/// bytes arrive, so a peer without the token cannot hold a slot.
const REQUEST_DEADLINE: Duration = Duration::from_secs(5);
/// Longest one `buzz messages send` may run while it holds a slot; past it
/// the send is killed and reported as failed.
const SEND_DEADLINE: Duration = Duration::from_secs(60);
/// How long the shim waits for the relay's answer: past both of the relay's
/// own deadlines, so the shim never gives up on a send the relay may still
/// post (the agent's retry would post it twice).
const ANSWER_TIMEOUT: Duration =
    Duration::from_secs(REQUEST_DEADLINE.as_secs() + SEND_DEADLINE.as_secs() + 10);
/// Over-cap connections waiting for their "busy" answer. Beyond this they
/// are closed unanswered, so a flood cannot grow the queue.
const BUSY_QUEUE: usize = 16;
/// How long the busy responder waits for an over-cap peer's request before
/// closing, so the close does not reset the answer it just sent.
const BUSY_DRAIN: Duration = Duration::from_millis(500);
/// The `messages send` options an agent may pass. Attachments read local
/// files, `--broadcast` leaves the relay, and global options would pick
/// another identity or relay.
const SEND_OPTIONS: &[&str] = &["channel", "content", "reply-to", "kind", "mention"];

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    args: Vec<String>,
    /// Base64 of the shim's stdin, read only for `--content -`.
    stdin: String,
}

#[derive(Serialize, Deserialize)]
struct Response {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Response {
    /// Shaped like the buzz CLI's own errors: JSON on stderr, exit code 1.
    fn refused(message: &str) -> Self {
        Self::error("bad_input", 1, message)
    }

    /// The relay, not the request, failed: `other`, exit code 4, as when the
    /// shim cannot reach the relay at all.
    fn failed(message: &str) -> Self {
        Self::error("other", 4, message)
    }

    fn error(kind: &str, code: i32, message: &str) -> Self {
        let error = serde_json::json!({"error": kind, "message": message});
        Self {
            code,
            stdout: String::new(),
            stderr: BASE64.encode(format!("{error}\n")),
        }
    }
}

/// A running relay. The shim directory lives as long as this value.
pub(super) struct BuzzRelay {
    addr: SocketAddr,
    token: String,
    shim_dir: tempfile::TempDir,
    /// Connections being served; at most [`Limits::max_connections`].
    #[cfg(test)]
    active: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
struct Limits {
    max_connections: usize,
    request_deadline: Duration,
    send_deadline: Duration,
}

impl Limits {
    const DEFAULT: Self = Self {
        max_connections: MAX_CONNECTIONS,
        request_deadline: REQUEST_DEADLINE,
        send_deadline: SEND_DEADLINE,
    };
}

/// One of the relay's bounded connection slots, released on every exit path
/// of the connection's worker, including a worker that never started.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn acquire(active: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        let mut n = active.load(Ordering::Acquire);
        while n < max {
            match active.compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(Self(Arc::clone(active))),
                Err(current) => n = current,
            }
        }
        None
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl BuzzRelay {
    /// Listen on loopback and write a `buzz` shim that runs `client_exe`.
    /// Sends go through `buzz` with this process's environment, to channels
    /// `tracker` reports live.
    pub(super) fn start(
        tracker: Arc<Mutex<BuzzDeliveryTracker>>,
        buzz: PathBuf,
        client_exe: &Path,
    ) -> io::Result<Self> {
        Self::start_with(tracker, buzz, client_exe, Limits::DEFAULT)
    }

    fn start_with(
        tracker: Arc<Mutex<BuzzDeliveryTracker>>,
        buzz: PathBuf,
        client_exe: &Path,
        limits: Limits,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let token = hex_token();
        let shim_dir = tempfile::Builder::new().prefix("pay-acp-buzz-").tempdir()?;
        write_shim(shim_dir.path(), client_exe)?;

        let expected = token.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let slots = Arc::clone(&active);
        // One thread answers every over-cap connection, so a flood costs a
        // bounded queue and no worker.
        let (busy, over_cap) = std::sync::mpsc::sync_channel::<TcpStream>(BUSY_QUEUE);
        thread::Builder::new()
            .name("pay-acp-buzz-busy".into())
            .spawn(move || {
                for stream in over_cap {
                    answer_busy(stream);
                }
            })?;
        thread::Builder::new()
            .name("pay-acp-buzz-relay".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    // Over the cap: no worker; told to retry, or closed when
                    // even the busy queue is full.
                    let Some(slot) = Slot::acquire(&slots, limits.max_connections) else {
                        let _ = busy.try_send(stream);
                        continue;
                    };
                    let (tracker, buzz, expected) =
                        (Arc::clone(&tracker), buzz.clone(), expected.clone());
                    // A worker that fails to start drops its closure, and
                    // with it the slot.
                    let _ = thread::Builder::new()
                        .name("pay-acp-buzz-send".into())
                        .spawn(move || {
                            let _slot = slot;
                            serve(stream, &expected, &tracker, &buzz, limits);
                        });
                }
            })?;
        Ok(Self {
            addr,
            token,
            shim_dir,
            #[cfg(test)]
            active,
        })
    }

    /// What the adapter needs to reach the relay: the shim first on `PATH`,
    /// the listener's address and the launch token.
    pub(super) fn child_env(&self) -> io::Result<Vec<(OsString, OsString)>> {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.shim_dir.path().to_path_buf())
                .chain(std::env::split_paths(&inherited)),
        )
        .map_err(io::Error::other)?;
        Ok(vec![
            ("PATH".into(), path),
            (ADDR_ENV.into(), self.addr.to_string().into()),
            (TOKEN_ENV.into(), self.token.clone().into()),
        ])
    }
}

fn hex_token() -> String {
    use std::fmt::Write as _;
    rand::random::<[u8; 32]>()
        .iter()
        .fold(String::with_capacity(64), |mut token, byte| {
            let _ = write!(token, "{byte:02x}");
            token
        })
}

#[cfg(unix)]
fn write_shim(dir: &Path, client_exe: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let exe = client_exe.to_string_lossy().replace('\'', r"'\''");
    let path = dir.join("buzz");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nexec '{exe}' {RELAY_ARG} \"$@\"\n"),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(windows)]
fn write_shim(dir: &Path, client_exe: &Path) -> io::Result<()> {
    std::fs::write(
        dir.join("buzz.cmd"),
        format!(
            "@echo off\r\n\"{}\" {RELAY_ARG} %*\r\n",
            client_exe.display()
        ),
    )
}

/// Tell an over-cap peer to retry: write the answer, then read what it sent
/// for a moment, so closing with its request unread does not reset the
/// connection before the answer is read.
fn answer_busy(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(BUSY_DRAIN));
    let _ = stream.set_read_timeout(Some(BUSY_DRAIN));
    if let Ok(mut body) = serde_json::to_vec(&Response::failed(
        "the relay is busy with other sends; retry",
    )) {
        body.push(b'\n');
        let _ = stream.write_all(&body);
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let deadline = Instant::now() + BUSY_DRAIN;
    let mut sink = [0u8; 4096];
    while Instant::now() < deadline {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

fn serve(
    stream: TcpStream,
    expected_token: &str,
    tracker: &Mutex<BuzzDeliveryTracker>,
    buzz: &Path,
    limits: Limits,
) {
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    // A peer that cannot finish its request in time gets no answer.
    let Ok(line) = read_request(&stream, Instant::now() + limits.request_deadline) else {
        return;
    };
    let response = match serde_json::from_slice::<Request>(&line).ok() {
        None => Response::refused("malformed relay request"),
        Some(request) if !constant_time_eq(&request.token, expected_token) => {
            Response::refused("this relay belongs to another launch")
        }
        Some(request) => {
            let live = |channel: &str| {
                tracker
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .is_live_destination(channel)
            };
            match (
                send_args(&request.args, live),
                BASE64.decode(&request.stdin),
            ) {
                (Ok(args), Ok(stdin)) => run_buzz(buzz, &args, &stdin, limits.send_deadline),
                (Err(message), _) => Response::refused(&message),
                (_, Err(_)) => Response::refused("malformed relay request"),
            }
        }
    };
    let mut stream = stream;
    if let Ok(mut body) = serde_json::to_vec(&response) {
        body.push(b'\n');
        let _ = stream.write_all(&body);
    }
}

/// One newline-terminated request of at most [`MAX_REQUEST_BYTES`], read
/// before `deadline` in total: each read waits only for what remains.
fn read_request(mut stream: &TcpStream, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(remaining))?;
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(request);
        }
        request.extend_from_slice(&chunk[..read]);
        if let Some(end) = request.iter().position(|byte| *byte == b'\n') {
            request.truncate(end);
            return Ok(request);
        }
        if request.len() as u64 > MAX_REQUEST_BYTES {
            return Err(io::Error::other("relay request too large"));
        }
    }
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// The `buzz` arguments for an agent's request, or why it is refused. Only
/// `messages send` with [`SEND_OPTIONS`], to one live channel. Options are
/// re-emitted as `--name=value`, so a value can never be read as a flag.
fn send_args(args: &[String], live: impl Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let [messages, send, options @ ..] = args else {
        return Err("only `buzz messages send` is available to this agent".into());
    };
    if messages != "messages" || send != "send" {
        return Err("only `buzz messages send` is available to this agent".into());
    }
    let mut out = vec!["messages".to_string(), "send".to_string()];
    let mut channel = None;
    let mut options = options.iter();
    while let Some(option) = options.next() {
        let Some(option) = option.strip_prefix("--") else {
            return Err(format!("unexpected argument `{option}`"));
        };
        let (name, value) = match option.split_once('=') {
            Some((name, value)) => (name, value.to_string()),
            None => (
                option,
                options
                    .next()
                    .ok_or_else(|| format!("`--{option}` needs a value"))?
                    .clone(),
            ),
        };
        if !SEND_OPTIONS.contains(&name) {
            return Err(format!("`--{name}` is not available to this agent"));
        }
        if name == "channel" && channel.replace(value.clone()).is_some() {
            return Err("send to one channel at a time".into());
        }
        out.push(format!("--{name}={value}"));
    }
    let channel = channel.ok_or("`--channel` is required")?;
    if !live(&channel) {
        return Err(format!(
            "channel {channel} is not a conversation this agent is answering"
        ));
    }
    Ok(out)
}

/// Run `buzz` for one send, killing it if it outlives `deadline`: it runs
/// while its connection holds one of the relay's few slots.
fn run_buzz(buzz: &Path, args: &[String], stdin: &[u8], deadline: Duration) -> Response {
    let child = Command::new(buzz)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "Pay ACP relay could not launch `buzz messages send`");
            return Response::failed("the relay could not run buzz");
        }
    };
    // Feed stdin and drain both pipes on their own threads, so a buzz that
    // stops reading its input or fills a pipe cannot block the relay past
    // the deadline; killing the child ends all three.
    let input = child.stdin.take().map(|mut input| {
        let stdin = stdin.to_vec();
        thread::spawn(move || {
            let _ = input.write_all(&stdin);
        })
    });
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(pipe) = pipe {
                let _ = pipe.take(MAX_REQUEST_BYTES).read_to_end(&mut out);
            }
            out
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let until = Instant::now() + deadline;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("buzz did not finish the send in time");
            }
            Err(error) => {
                tracing::warn!(%error, "Pay ACP relay could not wait for `buzz messages send`");
                let _ = child.kill();
                let _ = child.wait();
                break Err("the relay could not run buzz");
            }
        }
    };
    if let Some(input) = input {
        let _ = input.join();
    }
    let (stdout, stderr) = (
        stdout.join().unwrap_or_default(),
        stderr.join().unwrap_or_default(),
    );
    match status {
        Ok(status) => Response {
            code: status.code().unwrap_or(4),
            stdout: BASE64.encode(stdout),
            stderr: BASE64.encode(stderr),
        },
        Err(message) => {
            tracing::warn!(?deadline, "Pay ACP relay send failed: {message}");
            Response::failed(message)
        }
    }
}

/// `pay __acp-buzz-relay <buzz args>`, run by the shim inside the adapter.
/// Returns the exit code of the parent's `buzz`.
pub(crate) fn run_client() -> i32 {
    let args: Vec<String> = std::env::args().skip(2).collect();
    match relay(&args) {
        Ok(response) => {
            let _ = io::stdout().write_all(&BASE64.decode(response.stdout).unwrap_or_default());
            let _ = io::stderr().write_all(&BASE64.decode(response.stderr).unwrap_or_default());
            response.code
        }
        Err(error) => {
            let error = serde_json::json!({"error": "other", "message": error.to_string()});
            eprintln!("{error}");
            4
        }
    }
}

fn relay(args: &[String]) -> io::Result<Response> {
    let (Some(addr), Some(token)) = (std::env::var_os(ADDR_ENV), std::env::var_os(TOKEN_ENV))
    else {
        return Err(io::Error::other(
            "buzz is relayed by `pay acp`; this process was not started by it",
        ));
    };
    let addr: SocketAddr = addr
        .to_string_lossy()
        .parse()
        .map_err(|_| io::Error::other(format!("invalid {ADDR_ENV}")))?;
    let reads_stdin = args
        .windows(2)
        .any(|pair| pair[0] == "--content" && pair[1] == "-")
        || args.iter().any(|arg| arg == "--content=-");
    let mut stdin = Vec::new();
    if reads_stdin {
        io::stdin()
            .take(MAX_REQUEST_BYTES / 2)
            .read_to_end(&mut stdin)?;
    }
    send_request(
        addr,
        &Request {
            token: token.to_string_lossy().into_owned(),
            args: args.to_vec(),
            stdin: BASE64.encode(stdin),
        },
    )
}

fn send_request(addr: SocketAddr, request: &Request) -> io::Result<Response> {
    let mut stream = TcpStream::connect_timeout(&addr, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    stream.write_all(&body)?;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REQUEST_BYTES)).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL: &str = "82f078b8-29e4-40b8-9daf-79e7f79c482d";
    const OTHER: &str = "00000000-0000-0000-0000-000000000001";

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| arg.to_string()).collect()
    }

    fn live(channel: &str) -> bool {
        channel == CHANNEL
    }

    #[test]
    fn a_send_to_the_turns_channel_is_relayed_with_unambiguous_options() {
        assert_eq!(
            send_args(
                &args(&[
                    "messages",
                    "send",
                    "--channel",
                    CHANNEL,
                    "--reply-to=abc",
                    "--content",
                    "--file=/etc/passwd",
                ]),
                live
            )
            .unwrap(),
            args(&[
                "messages",
                "send",
                &format!("--channel={CHANNEL}"),
                "--reply-to=abc",
                // A value is never read back as a flag.
                "--content=--file=/etc/passwd",
            ])
        );
    }

    #[test]
    fn anything_but_a_send_to_a_live_channel_is_refused() {
        for (request, reason) in [
            (vec!["messages", "get", "--channel", CHANNEL], "only"),
            (vec!["channels", "list"], "only"),
            (vec!["--private-key", "x", "messages", "send"], "only"),
            (vec!["messages", "send", "--content", "hi"], "--channel"),
            (
                vec!["messages", "send", "--channel", OTHER, "--content", "hi"],
                "not a conversation",
            ),
            (
                vec![
                    "messages",
                    "send",
                    "--channel",
                    CHANNEL,
                    "--file",
                    "/etc/passwd",
                ],
                "--file",
            ),
            (
                vec!["messages", "send", "--channel", CHANNEL, "--broadcast"],
                "--broadcast",
            ),
            (
                vec![
                    "messages",
                    "send",
                    "--channel",
                    CHANNEL,
                    "--relay-url=http://evil",
                ],
                "--relay-url",
            ),
            (
                vec![
                    "messages",
                    "send",
                    "--channel",
                    CHANNEL,
                    "--channel",
                    CHANNEL,
                ],
                "one channel",
            ),
            (
                vec!["messages", "send", "--channel", CHANNEL, "stray"],
                "stray",
            ),
        ] {
            let error = send_args(&args(&request), live).unwrap_err();
            assert!(error.contains(reason), "{request:?}: {error}");
        }
    }

    fn live_tracker() -> Arc<Mutex<BuzzDeliveryTracker>> {
        let mut tracker = BuzzDeliveryTracker::default();
        let prompt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {
                "sessionId": "s",
                "prompt": [{"type": "text", "text": format!("[Context]\nChannel: {CHANNEL}\n")}]
            }
        });
        tracker.observe_client_frame(prompt.to_string().as_bytes());
        Arc::new(Mutex::new(tracker))
    }

    /// The agent's send reaches `buzz` with the parent's key; the agent only
    /// ever held the launch token.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn the_agents_send_is_signed_by_the_parent_without_exposing_the_key() {
        use std::os::unix::fs::PermissionsExt as _;
        let fake = tempfile::tempdir().unwrap();
        let buzz = fake.path().join("buzz");
        // Stands in for the real CLI: reports the key it was given, its
        // arguments and its stdin.
        std::fs::write(
            &buzz,
            "#!/bin/sh\nprintf 'key=%s args=%s stdin=' \"${BUZZ_PRIVATE_KEY-unset}\" \"$*\"\ncat\n",
        )
        .unwrap();
        std::fs::set_permissions(&buzz, std::fs::Permissions::from_mode(0o755)).unwrap();

        let relay = BuzzRelay::start(live_tracker(), buzz, Path::new("/nonexistent/pay")).unwrap();
        let env = relay.child_env().unwrap();
        assert!(relay.shim_dir.path().join("buzz").exists());
        assert!(env.iter().any(|(name, value)| name == "PATH"
            && std::env::split_paths(value).next().as_deref() == Some(relay.shim_dir.path())));

        // The parent holds the key; `serve` runs `buzz` in its environment.
        // SAFETY: serial; no other test reads this variable from the process.
        unsafe { std::env::set_var("BUZZ_PRIVATE_KEY", "parent-key") };
        let request = |token: &str, channel: &str| Request {
            token: token.to_string(),
            args: args(&["messages", "send", "--channel", channel, "--content", "-"]),
            stdin: BASE64.encode("hello"),
        };
        let sent = send_request(relay.addr, &request(&relay.token, CHANNEL)).unwrap();
        let forged = send_request(relay.addr, &request(&"0".repeat(64), CHANNEL)).unwrap();
        let elsewhere = send_request(relay.addr, &request(&relay.token, OTHER)).unwrap();
        unsafe { std::env::remove_var("BUZZ_PRIVATE_KEY") };

        assert_eq!(sent.code, 0);
        assert_eq!(
            String::from_utf8(BASE64.decode(sent.stdout).unwrap()).unwrap(),
            format!(
                "key=parent-key args=messages send --channel={CHANNEL} --content=- stdin=hello"
            )
        );
        for refused in [forged, elsewhere] {
            assert_eq!(refused.code, 1);
            assert!(refused.stdout.is_empty());
        }
        // Nothing the adapter receives carries the key.
        assert!(env.iter().all(|(_, value)| value != "parent-key"));
    }
}

#[cfg(all(test, unix))]
mod limit_tests {
    use super::*;

    const CHANNEL: &str = "82f078b8-29e4-40b8-9daf-79e7f79c482d";

    /// A relay over a stand-in `buzz` that echoes its arguments.
    fn relay(limits: Limits) -> (BuzzRelay, tempfile::TempDir) {
        relay_with(limits, "#!/bin/sh\nprintf 'sent %s' \"$*\"\n")
    }

    /// A relay over a stand-in `buzz` running `script`.
    fn relay_with(limits: Limits, script: &str) -> (BuzzRelay, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt as _;
        let fake = tempfile::tempdir().unwrap();
        let buzz = fake.path().join("buzz");
        std::fs::write(&buzz, script).unwrap();
        std::fs::set_permissions(&buzz, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut tracker = BuzzDeliveryTracker::default();
        let prompt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/prompt",
            "params": {
                "sessionId": "s",
                "prompt": [{"type": "text", "text": format!("[Context]\nChannel: {CHANNEL}\n")}]
            }
        });
        tracker.observe_client_frame(prompt.to_string().as_bytes());
        let relay = BuzzRelay::start_with(
            Arc::new(Mutex::new(tracker)),
            buzz,
            Path::new("/nonexistent/pay"),
            limits,
        )
        .unwrap();
        (relay, fake)
    }

    fn wait_for(active: &AtomicUsize, expected: usize) {
        let until = Instant::now() + Duration::from_secs(5);
        while active.load(Ordering::Acquire) != expected {
            assert!(
                Instant::now() < until,
                "active connections never reached {expected}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn send(relay: &BuzzRelay) -> io::Result<Response> {
        send_request(
            relay.addr,
            &Request {
                token: relay.token.clone(),
                args: ["messages", "send", "--channel", CHANNEL, "--content", "hi"]
                    .map(String::from)
                    .to_vec(),
                stdin: String::new(),
            },
        )
    }

    /// The peer's connection was closed without an answer.
    fn closed_unanswered(mut stream: TcpStream) -> bool {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut answer = Vec::new();
        match stream.read_to_end(&mut answer) {
            Ok(_) => answer.is_empty(),
            Err(error) => {
                error.kind() != io::ErrorKind::WouldBlock && error.kind() != io::ErrorKind::TimedOut
            }
        }
    }

    fn stderr_of(response: &Response) -> String {
        String::from_utf8(BASE64.decode(&response.stderr).unwrap()).unwrap()
    }

    #[test]
    fn connections_beyond_the_cap_are_told_to_retry_until_a_slot_frees() {
        let (relay, _fake) = relay(Limits {
            max_connections: 2,
            request_deadline: Duration::from_secs(30),
            send_deadline: SEND_DEADLINE,
        });
        // Two peers without the token hold every slot.
        let held: Vec<_> = (0..2)
            .map(|_| TcpStream::connect(relay.addr).unwrap())
            .collect();
        wait_for(&relay.active, 2);

        // Past the cap a send gets no worker, but an answer: busy, retry.
        let busy = send(&relay).expect("an over-cap send is answered");
        assert_eq!(busy.code, 4);
        assert!(stderr_of(&busy).contains("busy"), "{}", stderr_of(&busy));
        assert_eq!(relay.active.load(Ordering::Acquire), 2);

        drop(held);
        wait_for(&relay.active, 0);
        let sent = send(&relay).unwrap();
        assert_eq!(sent.code, 0);
        assert_eq!(
            String::from_utf8(BASE64.decode(sent.stdout).unwrap()).unwrap(),
            format!("sent messages send --channel={CHANNEL} --content=hi")
        );
        wait_for(&relay.active, 0);
    }

    #[test]
    fn trickled_input_is_cut_at_the_request_deadline() {
        let deadline = Duration::from_millis(300);
        let (relay, _fake) = relay(Limits {
            max_connections: 1,
            request_deadline: deadline,
            send_deadline: SEND_DEADLINE,
        });
        let mut trickle = TcpStream::connect(relay.addr).unwrap();
        wait_for(&relay.active, 1);
        // One byte every 50 ms never trips an idle timeout, only the total.
        let started = Instant::now();
        while relay.active.load(Ordering::Acquire) == 1 {
            assert!(
                started.elapsed() < deadline * 5,
                "a trickling peer kept its worker past the deadline"
            );
            let _ = trickle.write_all(b"{");
            thread::sleep(Duration::from_millis(50));
        }
        assert!(started.elapsed() >= deadline - Duration::from_millis(50));
        assert!(closed_unanswered(trickle));
        // The freed slot serves an authenticated send.
        assert_eq!(send(&relay).unwrap().code, 0);
    }

    /// A `buzz` that never finishes is killed at the send deadline and
    /// reported as a relay failure, and its slot is freed.
    #[test]
    fn a_send_that_overruns_its_deadline_is_killed() {
        let (relay, _fake) = relay_with(
            Limits {
                max_connections: 1,
                request_deadline: REQUEST_DEADLINE,
                send_deadline: Duration::from_millis(300),
            },
            "#!/bin/sh\nexec sleep 30\n",
        );
        let started = Instant::now();
        let response = send(&relay).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the send was not cut"
        );
        assert_eq!(response.code, 4);
        assert!(
            stderr_of(&response).contains("\"error\":\"other\""),
            "{}",
            stderr_of(&response)
        );
        wait_for(&relay.active, 0);
    }

    /// A `buzz` that never reads the content it is given is still killed at
    /// the deadline: the stdin write cannot block the relay.
    #[test]
    fn a_send_that_stops_reading_stdin_is_still_killed() {
        let started = Instant::now();
        let response = run_buzz(
            Path::new("/bin/sh"),
            &["-c".to_string(), "exec sleep 30".to_string()],
            &vec![b'x'; 4 * 1024 * 1024],
            Duration::from_millis(300),
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the stdin write blocked"
        );
        assert_eq!(response.code, 4);
    }

    #[test]
    fn the_shim_waits_past_the_relays_own_deadlines() {
        assert!(ANSWER_TIMEOUT > REQUEST_DEADLINE + SEND_DEADLINE);
    }

    /// When the relay itself cannot run buzz, the agent sees `other` and
    /// exit 4, not a refusal of its input.
    #[test]
    fn a_relay_failure_is_not_reported_as_bad_input() {
        let response = run_buzz(
            Path::new("/nonexistent/buzz"),
            &[],
            &[],
            Duration::from_secs(1),
        );
        assert_eq!(response.code, 4);
        assert!(stderr_of(&response).contains("\"error\":\"other\""));
    }
}
