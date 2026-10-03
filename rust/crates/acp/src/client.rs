//! An ACP client: pay drives an agent instead of sitting between one and its
//! editor.
//!
//! The agent is a child process speaking NDJSON JSON-RPC on stdio, the same
//! adapters `pay acp` launches. The client owns the stdio pair, answers the
//! agent's own requests (permissions), and hands each prompt turn back as a
//! stream of [`TurnEvent`]s so a caller can relay text as it arrives.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// ACP protocol version this client speaks.
pub const PROTOCOL_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("agent i/o: {0}")]
    Io(String),
    #[error("agent returned error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("agent disconnected")]
    Disconnected,
    #[error("agent protocol: {0}")]
    Protocol(String),
    #[error("timed out waiting for the agent")]
    Timeout,
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// How the agent's `session/request_permission` calls are answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Pick the broadest allow option offered.
    AllowAll,
    /// Pick a reject option, or cancel when none is offered.
    RejectAll,
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
    Other(String),
}

impl StopReason {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("end_turn") => Self::EndTurn,
            Some("max_tokens") => Self::MaxTokens,
            Some("max_turn_requests") => Self::MaxTurnRequests,
            Some("refusal") => Self::Refusal,
            Some("cancelled") => Self::Cancelled,
            Some(other) => Self::Other(other.to_string()),
            None => Self::Other(String::new()),
        }
    }
}

/// One thing that happened during a prompt turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// A piece of the assistant's reply.
    Text(String),
    /// A piece of the agent's visible reasoning.
    Thought(String),
    /// The agent started or updated a tool call.
    ToolCall {
        id: String,
        title: Option<String>,
        status: Option<String>,
    },
    /// The turn finished; no more events follow.
    Done(StopReason),
    /// The turn failed; no more events follow.
    Failed(Error),
}

impl TurnEvent {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Failed(_))
    }
}

/// What the agent said about itself in `initialize`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Initialized {
    #[serde(default)]
    pub protocol_version: i64,
    #[serde(default)]
    pub agent_capabilities: Value,
    #[serde(default)]
    pub agent_info: Option<Implementation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Implementation {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

enum Pending {
    /// A plain request; the response goes to whoever is waiting.
    Reply(Sender<Result<Value, Error>>),
    /// A `session/prompt`; the response ends the turn for this session.
    Turn(String),
}

#[derive(Default)]
struct Shared {
    pending: Mutex<HashMap<i64, Pending>>,
    turns: Mutex<HashMap<String, Sender<TurnEvent>>>,
    disconnected: AtomicBool,
}

impl Shared {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn turns(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sender<TurnEvent>>> {
        self.turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn disconnect(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
        // Dropping the senders wakes every waiter with `Disconnected`.
        let pending = std::mem::take(&mut *self.pending());
        for (_, entry) in pending {
            if let Pending::Turn(session) = entry
                && let Some(turn) = self.turns().remove(&session)
            {
                let _ = turn.send(TurnEvent::Failed(Error::Disconnected));
            }
        }
        self.turns().clear();
    }
}

/// The agent's stdin. `None` once closed: dropping the handle is how the
/// agent learns the client is gone, so it must not wait on the reader
/// thread, which holds its own clone.
type Writer = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// A connected agent. Dropping it closes stdin and kills a spawned child.
pub struct AgentClient {
    writer: Writer,
    next_id: AtomicI64,
    shared: Arc<Shared>,
    child: Option<Child>,
}

impl AgentClient {
    /// Spawn `command` as the agent, taking over its stdin and stdout.
    /// stderr is inherited so the adapter's own diagnostics stay visible.
    pub fn spawn(mut command: Command, permissions: PermissionPolicy) -> Result<Self, Error> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Io("agent stdin was not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Io("agent stdout was not piped".into()))?;
        let mut client = Self::connect(stdout, stdin, permissions);
        client.child = Some(child);
        Ok(client)
    }

    /// Drive an agent over an arbitrary stdio pair (tests, or an adapter
    /// launched elsewhere).
    pub fn connect<R, W>(reader: R, writer: W, permissions: PermissionPolicy) -> Self
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let writer: Writer = Arc::new(Mutex::new(Some(Box::new(writer))));
        let shared = Arc::new(Shared::default());
        let reader_shared = Arc::clone(&shared);
        let reader_writer = Arc::clone(&writer);
        thread::Builder::new()
            .name("pay-acp-reader".into())
            .spawn(move || {
                read_loop(reader, &reader_writer, &reader_shared, permissions);
                reader_shared.disconnect();
            })
            .expect("spawn acp reader thread");
        Self {
            writer,
            next_id: AtomicI64::new(1),
            shared,
            child: None,
        }
    }

    /// `initialize`: declare a client with no filesystem or terminal
    /// capabilities, so the agent never asks us for either.
    pub fn initialize(&self) -> Result<Initialized, Error> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                },
                "clientInfo": { "name": "pay", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        serde_json::from_value(result).map_err(|e| Error::Protocol(format!("initialize: {e}")))
    }

    /// `session/new` rooted at `cwd`; returns the session id.
    pub fn new_session(&self, cwd: &Path) -> Result<String, Error> {
        let result = self.request(
            "session/new",
            json!({ "cwd": cwd.to_string_lossy(), "mcpServers": [] }),
        )?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Protocol("session/new returned no sessionId".into()))
    }

    /// `session/prompt` with one text block. Returns immediately; the
    /// [`Turn`] yields events until the agent ends the turn.
    pub fn prompt(&self, session_id: &str, text: &str) -> Result<Turn, Error> {
        self.prompt_blocks(session_id, vec![json!({ "type": "text", "text": text })])
    }

    /// `session/prompt` with arbitrary ACP content blocks.
    pub fn prompt_blocks(&self, session_id: &str, prompt: Vec<Value>) -> Result<Turn, Error> {
        if self.shared.disconnected.load(Ordering::SeqCst) {
            return Err(Error::Disconnected);
        }
        let (tx, rx) = mpsc::channel();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        {
            let mut turns = self.shared.turns();
            if turns.contains_key(session_id) {
                return Err(Error::Protocol(format!(
                    "session {session_id} already has a turn in flight"
                )));
            }
            turns.insert(session_id.to_string(), tx);
        }
        self.shared
            .pending()
            .insert(id, Pending::Turn(session_id.to_string()));
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": prompt },
        });
        if let Err(error) = write_frame(&self.writer, &frame) {
            self.shared.pending().remove(&id);
            self.shared.turns().remove(session_id);
            return Err(error);
        }
        Ok(Turn {
            events: rx,
            done: false,
        })
    }

    /// `session/cancel`: ask the agent to stop the turn in flight. The turn
    /// then ends with [`StopReason::Cancelled`].
    pub fn cancel(&self, session_id: &str) -> Result<(), Error> {
        write_frame(
            &self.writer,
            &json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": session_id },
            }),
        )
    }

    /// Send a request and wait for its response.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.request_with(method, params, None)
    }

    /// Send a request and wait at most `timeout` for its response.
    pub fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, Error> {
        self.request_with(method, params, Some(timeout))
    }

    fn request_with(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, Error> {
        if self.shared.disconnected.load(Ordering::SeqCst) {
            return Err(Error::Disconnected);
        }
        let (tx, rx) = mpsc::channel();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.shared.pending().insert(id, Pending::Reply(tx));
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(error) = write_frame(&self.writer, &frame) {
            self.shared.pending().remove(&id);
            return Err(error);
        }
        let outcome = match timeout {
            None => rx.recv().map_err(|_| Error::Disconnected),
            Some(timeout) => rx.recv_timeout(timeout).map_err(|e| match e {
                RecvTimeoutError::Timeout => Error::Timeout,
                RecvTimeoutError::Disconnected => Error::Disconnected,
            }),
        };
        if matches!(outcome, Err(Error::Timeout)) {
            self.shared.pending().remove(&id);
        }
        outcome?
    }

    /// Whether the agent's stdout has closed.
    pub fn is_disconnected(&self) -> bool {
        self.shared.disconnected.load(Ordering::SeqCst)
    }
}

impl Drop for AgentClient {
    fn drop(&mut self) {
        // Close stdin first: a well-behaved agent exits on EOF, which also
        // ends the reader thread.
        self.writer.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The events of one `session/prompt`, ending with `Done` or `Failed`.
#[derive(Debug)]
pub struct Turn {
    events: Receiver<TurnEvent>,
    done: bool,
}

impl Turn {
    /// Next event, waiting at most `timeout`. `Err(Timeout)` leaves the turn
    /// open; `Err(Disconnected)` means the agent went away without ending it.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<TurnEvent, Error> {
        if self.done {
            return Err(Error::Disconnected);
        }
        let event = self.events.recv_timeout(timeout).map_err(|e| match e {
            RecvTimeoutError::Timeout => Error::Timeout,
            RecvTimeoutError::Disconnected => Error::Disconnected,
        })?;
        self.done = event.is_terminal();
        Ok(event)
    }

    /// Drain the turn, concatenating the assistant text.
    pub fn collect_text(self) -> Result<(String, StopReason), Error> {
        let mut text = String::new();
        for event in self {
            match event {
                TurnEvent::Text(chunk) => text.push_str(&chunk),
                TurnEvent::Done(reason) => return Ok((text, reason)),
                TurnEvent::Failed(error) => return Err(error),
                TurnEvent::Thought(_) | TurnEvent::ToolCall { .. } => {}
            }
        }
        Err(Error::Disconnected)
    }
}

impl Iterator for Turn {
    type Item = TurnEvent;

    fn next(&mut self) -> Option<TurnEvent> {
        if self.done {
            return None;
        }
        match self.events.recv() {
            Ok(event) => {
                self.done = event.is_terminal();
                Some(event)
            }
            Err(_) => {
                self.done = true;
                Some(TurnEvent::Failed(Error::Disconnected))
            }
        }
    }
}

fn write_frame(writer: &Writer, frame: &Value) -> Result<(), Error> {
    let mut bytes = serde_json::to_vec(frame).map_err(|e| Error::Protocol(e.to_string()))?;
    bytes.push(b'\n');
    let mut guard = writer.lock().unwrap_or_else(|p| p.into_inner());
    let writer = guard.as_mut().ok_or(Error::Disconnected)?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_loop<R: Read>(reader: R, writer: &Writer, shared: &Shared, permissions: PermissionPolicy) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").filter(|id| !id.is_null());
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let response = answer_agent_request(method, message.get("params"), permissions);
                let frame = match response {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err((code, text)) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": { "code": code, "message": text },
                    }),
                };
                if write_frame(writer, &frame).is_err() {
                    return;
                }
            }
            (Some("session/update"), None) => handle_update(shared, message.get("params")),
            (Some(_), None) => {}
            (None, Some(id)) => handle_response(shared, id, &message),
            (None, None) => {}
        }
    }
}

/// Requests the agent makes of the client. Permissions are decided by
/// policy; everything else is refused, since we declared no capabilities.
fn answer_agent_request(
    method: &str,
    params: Option<&Value>,
    permissions: PermissionPolicy,
) -> Result<Value, (i64, String)> {
    match method {
        "session/request_permission" => {
            let options = params
                .and_then(|p| p.get("options"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(match choose_permission(&options, permissions) {
                Some(option_id) => {
                    json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
                }
                None => json!({ "outcome": { "outcome": "cancelled" } }),
            })
        }
        _ => Err((-32601, format!("method not supported by pay: {method}"))),
    }
}

fn choose_permission(options: &[Value], permissions: PermissionPolicy) -> Option<String> {
    let ranked: &[&str] = match permissions {
        PermissionPolicy::AllowAll => &["allow_always", "allow_once"],
        PermissionPolicy::RejectAll => &["reject_once", "reject_always"],
    };
    ranked.iter().find_map(|kind| {
        options
            .iter()
            .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
            .and_then(|o| o.get("optionId").and_then(Value::as_str))
            .map(str::to_string)
    })
}

fn handle_update(shared: &Shared, params: Option<&Value>) {
    let Some(params) = params else { return };
    let Some(session) = params.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let Some(update) = params.get("update") else {
        return;
    };
    let event = match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => text_of(update).map(TurnEvent::Text),
        Some("agent_thought_chunk") => text_of(update).map(TurnEvent::Thought),
        Some("tool_call") | Some("tool_call_update") => update
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(|id| TurnEvent::ToolCall {
                id: id.to_string(),
                title: update
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                status: update
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
        _ => None,
    };
    if let Some(event) = event
        && let Some(turn) = shared.turns().get(session)
    {
        let _ = turn.send(event);
    }
}

fn text_of(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    (content.get("type").and_then(Value::as_str) == Some("text"))
        .then(|| content.get("text").and_then(Value::as_str))
        .flatten()
        .map(str::to_string)
}

fn handle_response(shared: &Shared, id: &Value, message: &Value) {
    let Some(id) = id.as_i64() else { return };
    let Some(pending) = shared.pending().remove(&id) else {
        return;
    };
    let outcome = match message.get("error") {
        Some(error) => Err(Error::Rpc {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
        }),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    };
    match pending {
        Pending::Reply(tx) => {
            let _ = tx.send(outcome);
        }
        Pending::Turn(session) => {
            if let Some(turn) = shared.turns().remove(&session) {
                let event = match outcome {
                    Ok(result) => TurnEvent::Done(StopReason::parse(
                        result.get("stopReason").and_then(Value::as_str),
                    )),
                    Err(error) => TurnEvent::Failed(error),
                };
                let _ = turn.send(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{PipeReader, PipeWriter};

    /// A scripted agent on the far end of two pipes. `handle` is called per
    /// client frame with a writer for the agent's replies.
    fn fake_agent<F>(handle: F) -> (AgentClient, thread::JoinHandle<Vec<Value>>)
    where
        F: FnMut(&Value, &mut PipeWriter) -> bool + Send + 'static,
    {
        let (agent_in, client_out) = io::pipe().unwrap();
        let (client_in, agent_out) = io::pipe().unwrap();
        let handle = thread::spawn(move || run_fake_agent(agent_in, agent_out, handle));
        let client = AgentClient::connect(client_in, client_out, PermissionPolicy::AllowAll);
        (client, handle)
    }

    /// Returns every frame the client sent. `handle` returning false ends
    /// the agent, which closes its stdout.
    fn run_fake_agent<F>(input: PipeReader, mut output: PipeWriter, mut handle: F) -> Vec<Value>
    where
        F: FnMut(&Value, &mut PipeWriter) -> bool,
    {
        let mut seen = Vec::new();
        let reader = BufReader::new(input);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let frame: Value = serde_json::from_str(&line).unwrap();
            seen.push(frame.clone());
            if !handle(&frame, &mut output) {
                break;
            }
        }
        seen
    }

    fn send(out: &mut PipeWriter, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).unwrap();
        bytes.push(b'\n');
        out.write_all(&bytes).unwrap();
        out.flush().unwrap();
    }

    fn reply(out: &mut PipeWriter, id: &Value, result: Value) {
        send(out, json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn update(out: &mut PipeWriter, session: &str, update: Value) {
        send(
            out,
            json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": { "sessionId": session, "update": update },
            }),
        );
    }

    /// The happy path: initialize, new session, a turn with a permission
    /// request, a thought, a tool call and two text chunks.
    fn scripted_agent(frame: &Value, out: &mut PipeWriter) -> bool {
        let id = frame.get("id");
        match frame["method"].as_str() {
            Some("initialize") => reply(
                out,
                id.unwrap(),
                json!({
                    "protocolVersion": 1,
                    "agentCapabilities": { "loadSession": false },
                    "agentInfo": { "name": "fake", "version": "0.1" },
                }),
            ),
            Some("session/new") => {
                assert_eq!(frame["params"]["cwd"], "/tmp/work");
                reply(out, id.unwrap(), json!({ "sessionId": "s1" }));
            }
            Some("session/prompt") => {
                assert_eq!(frame["params"]["sessionId"], "s1");
                assert_eq!(frame["params"]["prompt"][0]["text"], "Say hello");
                update(
                    out,
                    "s1",
                    json!({ "sessionUpdate": "agent_thought_chunk",
                            "content": { "type": "text", "text": "thinking" } }),
                );
                send(
                    out,
                    json!({
                        "jsonrpc": "2.0", "id": 900, "method": "session/request_permission",
                        "params": {
                            "sessionId": "s1",
                            "toolCall": { "toolCallId": "t1", "title": "run tests" },
                            "options": [
                                { "optionId": "no", "name": "No", "kind": "reject_once" },
                                { "optionId": "once", "name": "Once", "kind": "allow_once" },
                                { "optionId": "always", "name": "Always", "kind": "allow_always" },
                            ],
                        },
                    }),
                );
                // The permission answer arrives as the next client frame.
            }
            None if id == Some(&json!(900)) => {
                assert_eq!(frame["result"]["outcome"]["optionId"], "always");
                update(
                    out,
                    "s1",
                    json!({ "sessionUpdate": "tool_call", "toolCallId": "t1",
                            "title": "run tests", "status": "in_progress" }),
                );
                update(
                    out,
                    "s1",
                    json!({ "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "Hello, " } }),
                );
                update(
                    out,
                    "s1",
                    json!({ "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "world" } }),
                );
                // A chunk for another session must not leak into this turn.
                update(
                    out,
                    "other",
                    json!({ "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "noise" } }),
                );
                reply(out, &json!(3), json!({ "stopReason": "end_turn" }));
            }
            _ => {}
        }
        true
    }

    #[test]
    fn drives_a_full_turn_and_answers_permissions() {
        let (client, agent) = fake_agent(scripted_agent);

        let init = client.initialize().unwrap();
        assert_eq!(init.protocol_version, 1);
        assert_eq!(init.agent_info.unwrap().name, "fake");

        let session = client.new_session(Path::new("/tmp/work")).unwrap();
        assert_eq!(session, "s1");

        let turn = client.prompt(&session, "Say hello").unwrap();
        let events: Vec<TurnEvent> = turn.collect();
        assert_eq!(
            events,
            vec![
                TurnEvent::Thought("thinking".into()),
                TurnEvent::ToolCall {
                    id: "t1".into(),
                    title: Some("run tests".into()),
                    status: Some("in_progress".into()),
                },
                TurnEvent::Text("Hello, ".into()),
                TurnEvent::Text("world".into()),
                TurnEvent::Done(StopReason::EndTurn),
            ]
        );

        drop(client);
        let seen = agent.join().unwrap();
        assert_eq!(seen[0]["method"], "initialize");
        assert_eq!(seen[0]["params"]["protocolVersion"], 1);
        assert_eq!(seen[0]["params"]["clientCapabilities"]["terminal"], false);
        assert_eq!(seen[1]["method"], "session/new");
        assert_eq!(seen[2]["method"], "session/prompt");
        assert_eq!(seen[2]["id"], 3);
    }

    #[test]
    fn collect_text_joins_chunks_and_reports_the_stop_reason() {
        let (client, _agent) = fake_agent(scripted_agent);
        client.initialize().unwrap();
        let session = client.new_session(Path::new("/tmp/work")).unwrap();
        let (text, reason) = client
            .prompt(&session, "Say hello")
            .unwrap()
            .collect_text()
            .unwrap();
        assert_eq!(text, "Hello, world");
        assert_eq!(reason, StopReason::EndTurn);
    }

    #[test]
    fn reject_policy_picks_a_reject_option_and_unknown_requests_are_refused() {
        let options = vec![
            json!({ "optionId": "once", "kind": "allow_once" }),
            json!({ "optionId": "never", "kind": "reject_always" }),
        ];
        assert_eq!(
            choose_permission(&options, PermissionPolicy::RejectAll),
            Some("never".to_string())
        );
        assert_eq!(
            choose_permission(&options, PermissionPolicy::AllowAll),
            Some("once".to_string())
        );
        assert_eq!(choose_permission(&[], PermissionPolicy::AllowAll), None);

        let refused = answer_agent_request("fs/read_text_file", None, PermissionPolicy::AllowAll);
        assert_eq!(refused.unwrap_err().0, -32601);
        let cancelled = answer_agent_request(
            "session/request_permission",
            None,
            PermissionPolicy::AllowAll,
        )
        .unwrap();
        assert_eq!(cancelled["outcome"]["outcome"], "cancelled");
    }

    #[test]
    fn rpc_errors_surface_on_the_request() {
        let (client, _agent) = fake_agent(|frame, out| {
            if frame["method"] == "session/new" {
                send(
                    out,
                    json!({ "jsonrpc": "2.0", "id": frame["id"],
                            "error": { "code": -32602, "message": "cwd must be absolute" } }),
                );
            }
            true
        });
        let error = client.new_session(Path::new("relative")).unwrap_err();
        assert!(
            matches!(error, Error::Rpc { code: -32602, ref message } if message.contains("absolute")),
            "{error:?}"
        );
    }

    #[test]
    fn a_dying_agent_fails_the_turn_in_flight_and_later_requests() {
        let (client, _agent) = fake_agent(|frame, out| match frame["method"].as_str() {
            Some("session/new") => {
                reply(out, &frame["id"], json!({ "sessionId": "s1" }));
                true
            }
            Some("session/prompt") => {
                update(
                    out,
                    "s1",
                    json!({ "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": "partial" } }),
                );
                false // exit: stdout closes mid-turn
            }
            _ => true,
        });
        let session = client.new_session(Path::new("/tmp/work")).unwrap();
        let mut turn = client.prompt(&session, "go").unwrap();
        assert_eq!(
            turn.recv_timeout(Duration::from_secs(5)).unwrap(),
            TurnEvent::Text("partial".into())
        );
        assert_eq!(
            turn.recv_timeout(Duration::from_secs(5)).unwrap(),
            TurnEvent::Failed(Error::Disconnected)
        );
        assert!(turn.next().is_none(), "a finished turn yields nothing more");

        // The reader thread has marked the client disconnected by now, or
        // the write itself fails; either way no request can succeed.
        let error = client
            .request_timeout("session/new", json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert!(
            matches!(error, Error::Disconnected | Error::Io(_)),
            "{error:?}"
        );
        assert!(client.is_disconnected());
    }

    #[test]
    fn request_timeout_gives_up_and_forgets_the_request() {
        let (client, _agent) = fake_agent(|_, _| true);
        let error = client
            .request_timeout("initialize", json!({}), Duration::from_millis(50))
            .unwrap_err();
        assert!(matches!(error, Error::Timeout));
        assert!(client.shared.pending().is_empty());
    }

    #[test]
    fn one_turn_per_session_at_a_time() {
        let (client, _agent) = fake_agent(|_, _| true);
        let _turn = client.prompt("s1", "first").unwrap();
        let error = client.prompt("s1", "second").unwrap_err();
        assert!(matches!(error, Error::Protocol(_)));
    }

    #[test]
    fn cancel_is_a_notification() {
        let (client, agent) = fake_agent(|frame, _| frame["method"] != "session/cancel");
        client.cancel("s1").unwrap();
        drop(client);
        let seen = agent.join().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["method"], "session/cancel");
        assert!(seen[0].get("id").is_none());
        assert_eq!(seen[0]["params"]["sessionId"], "s1");
    }
}
