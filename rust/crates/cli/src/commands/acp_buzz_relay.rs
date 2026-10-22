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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
        let error = serde_json::json!({"error": "bad_input", "message": message});
        Self {
            code: 1,
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
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        let token = hex_token();
        let shim_dir = tempfile::Builder::new().prefix("pay-acp-buzz-").tempdir()?;
        write_shim(shim_dir.path(), client_exe)?;

        let expected = token.clone();
        thread::Builder::new()
            .name("pay-acp-buzz-relay".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (tracker, buzz, expected) =
                        (Arc::clone(&tracker), buzz.clone(), expected.clone());
                    let _ = thread::Builder::new()
                        .name("pay-acp-buzz-send".into())
                        .spawn(move || serve(stream, &expected, &tracker, &buzz));
                }
            })?;
        Ok(Self {
            addr,
            token,
            shim_dir,
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

fn serve(
    stream: TcpStream,
    expected_token: &str,
    tracker: &Mutex<BuzzDeliveryTracker>,
    buzz: &Path,
) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = String::new();
    let read = BufReader::new((&stream).take(MAX_REQUEST_BYTES)).read_line(&mut line);
    let response = match read
        .ok()
        .and_then(|_| serde_json::from_str::<Request>(&line).ok())
    {
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
                (Ok(args), Ok(stdin)) => run_buzz(buzz, &args, &stdin),
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

fn run_buzz(buzz: &Path, args: &[String], stdin: &[u8]) -> Response {
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
            return Response::refused("the relay could not run buzz");
        }
    };
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(stdin);
    }
    match child.wait_with_output() {
        Ok(output) => Response {
            code: output.status.code().unwrap_or(4),
            stdout: BASE64.encode(output.stdout),
            stderr: BASE64.encode(output.stderr),
        },
        Err(error) => {
            tracing::warn!(%error, "Pay ACP relay could not wait for `buzz messages send`");
            Response::refused("the relay could not run buzz")
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
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
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
