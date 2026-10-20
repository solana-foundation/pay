//! A local Solana RPC for tests that build payments from canned challenges.
//!
//! The kit uses a challenge's `recentBlockhash` only once the client's own
//! RPC confirms it is live (`isBlockhashValid`), so a test that signs a
//! canned challenge needs an RPC that answers it. This one confirms every
//! blockhash and answers every other method with a JSON-RPC error, so a test
//! can never reach a real cluster through it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;

static URL: OnceLock<String> = OnceLock::new();

/// Start the RPC once per process. From then on every payment builder in the
/// process uses it in place of `PAY_RPC_URL` (see
/// [`crate::config::payment_rpc_url`]); the environment is left untouched.
pub fn install_blockhash_rpc() -> &'static str {
    URL.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test RPC");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("test RPC address")
        );
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = serve(stream);
                });
            }
        });
        url
    })
}

/// The installed RPC, if a test started one.
pub(crate) fn installed() -> Option<&'static str> {
    URL.get().map(String::as_str)
}

fn serve(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let mut length = 0usize;
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        let call: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let reply = serde_json::to_vec(&answer(&call)).unwrap_or_default();
        write!(
            writer,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            reply.len()
        )?;
        writer.write_all(&reply)?;
        writer.flush()?;
    }
}

fn answer(call: &serde_json::Value) -> serde_json::Value {
    let id = call["id"].clone();
    match call["method"].as_str() {
        Some("isBlockhashValid") => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "context": { "slot": 1 }, "value": true },
        }),
        _ => serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "not answered by the test RPC" },
        }),
    }
}
