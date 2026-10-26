//! Exercise HTTP response handling through real MCP initialization and tools/call.

use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use pay_core::accounts::MemoryAccountsStore;
use pay_mcp::{ApprovalPolicy, CallScope, LocalFiles, PayContext, PayMcp};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ErrorCode},
    service::{Peer, RequestContext, RoleServer},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct NoPayment;

impl ApprovalPolicy for NoPayment {
    fn gate(&self, _: Option<&Peer<RoleServer>>) -> pay_core::signer::AuthOverride {
        panic!("ordinary HTTP responses must not request payment approval");
    }
}

struct TestContext {
    hosted: bool,
}

impl PayContext for TestContext {
    fn scope(&self, _: &RequestContext<RoleServer>) -> Result<CallScope, rmcp::ErrorData> {
        Ok(CallScope {
            accounts: Arc::new(MemoryAccountsStore::new()),
            network_override: None,
            account_override: None,
            rpc_url_override: None,
            approval: Arc::new(NoPayment),
            payment_permissions: None,
            local_files: (!self.hosted).then(LocalFiles::on_user_machine),
            outbound_policy: pay_core::client::fetch::OutboundPolicy::Unrestricted,
        })
    }
}

const WORKER_CASE: &str = "PAY_MCP_HTTP_TEST_CASE";
const WORKER_OK: &str = "MCP HTTP worker completed";
const REQUEST_STARTED: &str = "MCP HTTP fault: request received";
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Serialize, Deserialize)]
struct HttpCase {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    fault: Option<String>,
    #[serde(default)]
    hosted: bool,
}

impl HttpCase {
    fn new(status: u16, content_type: &str, body: &[u8]) -> Self {
        Self {
            status,
            content_type: content_type.to_string(),
            body: body.to_vec(),
            fault: None,
            hosted: false,
        }
    }
}

/// Reap the worker even if its supervisor panics. Killing the process also
/// stops any HTTP work already running in Tokio's blocking pool.
struct Worker(Child);

impl Drop for Worker {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct WorkerResult {
    status: ExitStatus,
    timed_out: bool,
    output: String,
    /// Files the worker left in its temporary directory.
    leftover_files: Vec<String>,
}

fn worker_command(case: &HttpCase) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "isolated_http_worker",
            "--ignored",
            "--nocapture",
        ])
        .env(WORKER_CASE, serde_json::to_string(case).unwrap());
    command
}

fn is_proxy_variable(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_string_lossy().to_ascii_lowercase().as_str(),
        "http_proxy" | "https_proxy" | "all_proxy" | "no_proxy" | "pay_debugger_proxy"
    )
}

fn run_worker(mut command: Command, timeout: Duration) -> WorkerResult {
    // Only the child's environment changes. Include explicit Command overrides
    // as well as inherited variables, including unusual casing.
    let proxy_variables: Vec<_> = std::env::vars_os()
        .map(|(name, _)| name)
        .chain(command.get_envs().map(|(name, _)| name.to_os_string()))
        .filter(|name| is_proxy_variable(name))
        .collect();
    for name in proxy_variables {
        command.env_remove(name);
    }

    // The parent owns every generated response file, including files left by a
    // panic or killed worker. Regular log files cannot fill a subprocess pipe.
    let directory = tempfile::tempdir().unwrap();
    let log_path = directory.path().join("worker.log");
    let log = std::fs::File::create(&log_path).unwrap();
    command
        .env("TMPDIR", directory.path())
        .env("TMP", directory.path())
        .env("TEMP", directory.path())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    let mut worker = Worker(command.spawn().unwrap());
    drop(command);
    let started = Instant::now();
    let timed_out = loop {
        if worker.0.try_wait().unwrap().is_some() {
            break false;
        }
        if started.elapsed() >= timeout {
            let _ = worker.0.kill();
            break true;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let status = worker.0.wait().unwrap();
    drop(worker);
    let output = std::fs::read_to_string(&log_path).unwrap();
    let leftover_files = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| *path != log_path)
        .map(|path| path.display().to_string())
        .collect();
    directory
        .close()
        .expect("remove all worker files after reaping");
    WorkerResult {
        status,
        timed_out,
        output,
        leftover_files,
    }
}

fn assert_worker_succeeded(result: WorkerResult) {
    assert!(!result.timed_out, "worker timed out:\n{}", result.output);
    assert!(result.status.success(), "worker failed:\n{}", result.output);
    // A misspelled libtest filter can report success with zero tests run.
    assert!(result.output.contains(WORKER_OK), "{}", result.output);
}

// The nine public test cases run this worker in a fresh process. Keeping the
// runtime here lets the parent enforce a deadline even during runtime shutdown.
#[test]
#[ignore = "subprocess worker; invoked with an explicit serialized HTTP case"]
fn isolated_http_worker() {
    assert!(std::env::vars_os().all(|(name, _)| !is_proxy_variable(&name)));
    let case: HttpCase = serde_json::from_str(&std::env::var(WORKER_CASE).unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(check_http(&case));
    drop(runtime);
    println!("{WORKER_OK}");
}

async fn call_http(case: &HttpCase) -> CallToolResult {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/response", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {} Test\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        case.status,
        case.content_type,
        case.body.len()
    );
    let response = [response.as_bytes(), &case.body].concat();
    let requests = Arc::new(AtomicUsize::new(0));
    let request_count = requests.clone();
    let inject_fault = case.fault.is_some();
    let (arrived, request_received) = tokio::sync::oneshot::channel();
    // JoinSet aborts its listener/connection task if this future unwinds.
    let mut http = tokio::task::JoinSet::<()>::new();
    http.spawn(async move {
        let mut arrived = Some(arrived);
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            request_count.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let byte = stream.read_u8().await.unwrap();
                request.push(byte);
            }
            if let Some(arrived) = arrived.take() {
                let _ = arrived.send(());
            }
            if inject_fault {
                std::future::pending::<()>().await;
            }
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });

    let (server_transport, client_transport) = tokio::io::duplex(65536);
    // Poll both initialization futures together; there is no detached startup task.
    let (server, client) = tokio::join!(
        if case.hosted {
            PayMcp::with_hosted_context(Arc::new(TestContext { hosted: true }))
        } else {
            PayMcp::with_context(Arc::new(TestContext { hosted: false }))
        }
        .serve(server_transport),
        ().serve(client_transport),
    );
    let server = server.unwrap();
    let client = client.unwrap();
    let result = {
        let request = client.call_tool(
            CallToolRequestParams::new("curl")
                .with_arguments(json!({"url": url}).as_object().unwrap().clone()),
        );
        tokio::pin!(request);
        if let Some(fault) = case.fault.as_deref() {
        tokio::select! {
            result = &mut request => panic!("request returned before fault injection: {result:?}"),
            received = request_received => {
                received.unwrap();
                // This marker proves the real blocking HTTP request reached the
                // fixture, rather than merely timing out during process startup.
                println!("{REQUEST_STARTED}");
                std::io::Write::flush(&mut std::io::stdout()).unwrap();
                match fault {
                    "stall" => std::future::pending().await,
                    "panic" => panic!("injected panic with a blocking HTTP request in flight"),
                    _ => panic!("unknown fault: {fault}"),
                }
            }
        }
    } else {
        request.as_mut().await
    }
    .expect("HTTP failures belong in the tool result, not a JSON-RPC error")
    };

    // Dispatching an unknown tool still uses the JSON-RPC error channel.
    let protocol_error = client
        .call_tool(CallToolRequestParams::new("no_such_tool"))
        .await
        .unwrap_err();
    assert!(matches!(
        protocol_error,
        rmcp::ServiceError::McpError(error) if error.code == ErrorCode::INVALID_PARAMS
    ));

    client.cancel().await.unwrap();
    server.cancel().await.unwrap();
    http.abort_all();
    let listener_result = http.join_next().await.unwrap();
    assert!(listener_result.unwrap_err().is_cancelled());
    assert_eq!(requests.load(Ordering::SeqCst), 1, "no HTTP retries");
    result
}

async fn check_http(case: &HttpCase) {
    let result = call_http(case).await;
    let expected_error = case.status >= 400;
    assert_eq!(
        result.is_error,
        Some(expected_error),
        "HTTP {}",
        case.status
    );
    let wire = serde_json::to_value(&result).unwrap();
    assert_eq!(wire["isError"], expected_error);
    let expected_blocks = if case.content_type == "application/pdf" && !case.hosted {
        2
    } else {
        1
    };
    assert_eq!(result.content.len(), expected_blocks);
    let content = &result.content[0];
    if case.content_type.starts_with("image/") {
        let image = content.as_image().unwrap();
        assert_eq!(image.mime_type, case.content_type);
        assert_eq!(STANDARD.decode(&image.data).unwrap(), case.body);
    } else if case.hosted
        && (case.content_type == "application/octet-stream"
            || case.content_type == "application/pdf")
    {
        let text = &content.as_text().unwrap().text;
        assert!(text.contains("omitted"), "{text}");
        assert!(!text.contains(" written to "), "{text}");
    } else if case.content_type == "application/octet-stream"
        || case.content_type == "application/pdf"
    {
        let text = &content.as_text().unwrap().text;
        let path = text.split(" written to ").nth(1).unwrap();
        assert_eq!(
            std::fs::canonicalize(Path::new(path).parent().unwrap()).unwrap(),
            std::fs::canonicalize(std::env::temp_dir()).unwrap(),
            "response files must belong to the supervised worker's temporary directory"
        );
        assert_eq!(std::fs::read(path).unwrap(), case.body);
        assert!(text.contains(&case.content_type));
        if case.content_type == "application/pdf" {
            let link = result.content[1].as_resource_link().unwrap();
            assert_eq!(link.mime_type.as_deref(), Some(case.content_type.as_str()));
            let linked_path = reqwest::Url::parse(&link.uri)
                .unwrap()
                .to_file_path()
                .unwrap();
            assert_eq!(
                std::fs::canonicalize(&linked_path).unwrap(),
                std::fs::canonicalize(path).unwrap()
            );
            assert_eq!(
                link.name,
                Path::new(path).file_name().unwrap().to_str().unwrap()
            );
            assert_eq!(link.size, Some(case.body.len() as u64));
            assert_eq!(
                std::fs::metadata(linked_path).unwrap().len(),
                case.body.len() as u64
            );
        }
        std::fs::remove_file(path).unwrap();
    } else {
        let expected = if case.body.is_empty() {
            if expected_error {
                "Request failed."
            } else {
                "Request completed."
            }
        } else {
            std::str::from_utf8(&case.body).unwrap()
        };
        assert_eq!(content.as_text().unwrap().text, expected);
    }
}

#[test]
fn proxy_environment_is_ignored() {
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let url = format!("http://{}", proxy.local_addr().unwrap());
    let case = HttpCase::new(502, "application/pdf", b"%PDF-1.7\x00\xff");
    let mut command = worker_command(&case);
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HtTp_PrOxY",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "PAY_DEBUGGER_PROXY",
    ] {
        command.env(name, &url);
    }
    command.env("NO_PROXY", "").env("no_proxy", "");
    let result = run_worker(command, TEST_TIMEOUT);
    assert!(
        matches!(proxy.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "worker connected to the proxy"
    );
    assert_worker_succeeded(result);
}

#[test]
fn supervisor_reaps_stalled_and_panicking_requests() {
    for fault in ["stall", "panic"] {
        let mut case = HttpCase::new(502, "text/plain", b"Bad Gateway");
        case.fault = Some(fault.to_string());
        let started = Instant::now();
        let result = run_worker(worker_command(&case), Duration::from_secs(3));
        assert!(result.output.contains(REQUEST_STARTED), "{}", result.output);
        assert!(!result.status.success());
        assert!(!result.output.contains(WORKER_OK));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "worker outlived supervisor deadline"
        );
        if fault == "stall" {
            assert!(result.timed_out, "{}", result.output);
        } else {
            assert!(
                result.output.contains("injected panic"),
                "{}",
                result.output
            );
        }
    }
}

#[test]
fn hosted_binary_responses_leave_no_files() {
    for (mime, body) in [
        ("application/octet-stream", &b"\x00\xff\xfe\x80"[..]),
        ("application/pdf", &b"%PDF-1.7\x00\xff"[..]),
    ] {
        let mut case = HttpCase::new(200, mime, body);
        case.hosted = true;
        let result = run_worker(worker_command(&case), TEST_TIMEOUT);
        let leftover_files = result.leftover_files.clone();
        assert_worker_succeeded(result);
        assert!(leftover_files.is_empty(), "{mime}: {leftover_files:?}");
    }
}

macro_rules! http_test {
    ($name:ident, $status:literal, $mime:literal, $body:expr) => {
        #[test]
        fn $name() {
            let case = HttpCase::new($status, $mime, $body);
            assert_worker_succeeded(run_worker(worker_command(&case), TEST_TIMEOUT));
        }
    };
}

http_test!(
    http_200,
    200,
    "application/json; charset=utf-8",
    br#"{"ok":true}"#
);
http_test!(http_204, 204, "text/plain", b"");
http_test!(
    http_400,
    400,
    "application/problem+json",
    br#"{"error":"bad request"}"#
);
http_test!(http_429, 429, "text/plain", b"rate limited");
http_test!(
    http_502,
    502,
    "text/html; charset=utf-8",
    b"<h1>Bad Gateway</h1>"
);
http_test!(http_502_empty_body, 502, "text/plain", b"");
http_test!(
    http_502_binary_body,
    502,
    "application/octet-stream",
    b"\x00\xff\xfe\x80"
);
http_test!(
    http_502_image_body,
    502,
    "image/png",
    b"\x89PNG\r\n\x1a\n\xff"
);
http_test!(
    http_502_pdf_body,
    502,
    "application/pdf",
    b"%PDF-1.7\x00\xff"
);
