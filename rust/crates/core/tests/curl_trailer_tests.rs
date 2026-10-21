//! Real curl transport tests. No wallet loading, signing, or paid retry is performed.
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use pay_core::client::receipt::decode_response_receipt;
use pay_core::runner::RunOutcome;

/// Re-enter just this test with automatic protocol selection, without mutating
/// the environment shared by other tests in this process. Returns true in the
/// parent, whose caller must leave the fixture body to the isolated child.
fn rerun_without_protocol_override(test_name: &str) -> bool {
    const WORKER: &str = "PAY_CURL_TRAILER_TEST_WORKER";
    if std::env::var(WORKER).ok().as_deref() == Some(test_name) {
        return false;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--color", "never"])
        .env(WORKER, test_name)
        .env_remove("PAY_PROTOCOL_ENFORCED")
        .output()
        .expect("start isolated curl fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed;"),
        "isolated fixture {test_name} failed or did not run:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn mpp_header() -> String {
    let request = serde_json::json!({
        "amount": "1000000",
        "currency": "USDC",
        "recipient": "So11111111111111111111111111111111111111112",
        "methodDetails": { "network": "devnet" }
    });
    format!(
        "Payment id=\"offline-fixture\", realm=\"local\", method=\"solana\", intent=\"charge\", request=\"{}\"",
        URL_SAFE_NO_PAD.encode(request.to_string())
    )
}

fn response(status: &str, headers: &[(&str, String)], trailers: bool) -> String {
    let mut wire = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
    for (name, value) in headers {
        wire.push_str(&format!("{name}: {value}\r\n"));
    }
    if trailers {
        wire.push_str("Transfer-Encoding: chunked\r\nTrailer: X-Fixture-Timing\r\n\r\n0\r\nX-Fixture-Timing: 7\r\n\r\n");
    } else {
        wire.push_str("Content-Length: 0\r\n\r\n");
    }
    wire
}

fn run_fixture(responses: impl FnOnce(&str) -> Vec<String>) -> (RunOutcome, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/paid", listener.local_addr().unwrap());
    let responses = responses(&url);
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        for response in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "curl did not reach the loopback fixture"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("loopback accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buffer).unwrap();
                assert!(
                    n > 0 && request.len() + n <= 65_536,
                    "incomplete fixture request"
                );
                request.extend_from_slice(&buffer[..n]);
            }
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    // -q must be first: ignore the user's curlrc. Bypass proxies without changing environment.
    let args = [
        "-q",
        "--silent",
        "--show-error",
        "--http1.1",
        "--noproxy",
        "*",
        "--max-time",
        "5",
        "--location",
        &url,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let result = pay_core::run_curl_with_headers(&args, &[]);
    // Join the bounded fixture even if curl returned an error.
    server.join().unwrap();
    (result.unwrap(), url)
}

#[test]
fn curl_control_without_trailers_recognizes_mpp() {
    if rerun_without_protocol_override("curl_control_without_trailers_recognizes_mpp") {
        return;
    }
    let (outcome, url) = run_fixture(|_| {
        vec![response(
            "402 Payment Required",
            &[("WWW-Authenticate", mpp_header())],
            false,
        )]
    });
    assert!(
        matches!(outcome, RunOutcome::MppChallenge { resource_url, .. } if resource_url == url)
    );
}

#[test]
fn curl_chunked_402_with_trailers_recognizes_mpp() {
    if rerun_without_protocol_override("curl_chunked_402_with_trailers_recognizes_mpp") {
        return;
    }
    let (outcome, url) = run_fixture(|_| {
        vec![response(
            "402 Payment Required",
            &[("WWW-Authenticate", mpp_header())],
            true,
        )]
    });
    let RunOutcome::MppChallenge {
        challenge,
        resource_url,
        ..
    } = outcome
    else {
        panic!("a chunked 402 with trailers must remain a payment challenge: {outcome:?}");
    };
    assert_eq!(resource_url, url);
    let request: pay_kit::mpp::ChargeRequest = challenge.request.decode().unwrap();
    assert_eq!(request.amount, "1000000");
}

#[test]
fn curl_chunked_402_with_trailers_recognizes_x402() {
    if rerun_without_protocol_override("curl_chunked_402_with_trailers_recognizes_x402") {
        return;
    }
    let (outcome, url) = run_fixture(|url| {
        let offer = serde_json::json!({
            "x402Version": 2,
            "resource": { "url": url },
            "accepts": [{
                "scheme": "exact", "network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp",
                "amount": "10000", "asset": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                "payTo": "6cvgmdrsVxyiuPzqMCSBnS7fAmA5Mk2VG4BcfVhC8jdC", "maxTimeoutSeconds": 300,
                "extra": { "feePayer": "AepWpq3GQwL8CeKMtZyKtKPa7W91Coygh3ropAJapVdU", "decimals": 6 }
            }]
        });
        vec![response(
            "402 Payment Required",
            &[("Payment-Required", STANDARD.encode(offer.to_string()))],
            true,
        )]
    });
    let RunOutcome::X402Challenge {
        challenge,
        resource_url,
        ..
    } = outcome
    else {
        panic!("a chunked 402 with trailers must retain its x402 offer: {outcome:?}");
    };
    assert_eq!(resource_url, url);
    assert_eq!(challenge.requirements.amount, "10000");
}

#[test]
fn curl_redirect_trailers_do_not_hide_the_final_mpp_challenge() {
    if rerun_without_protocol_override("curl_redirect_trailers_do_not_hide_the_final_mpp_challenge")
    {
        return;
    }
    let (outcome, url) = run_fixture(|url| {
        vec![
            response(
                "302 Found",
                &[
                    ("Location", url.into()),
                    ("X-Old-Response", "discard".into()),
                ],
                true,
            ),
            response(
                "402 Payment Required",
                &[("WWW-Authenticate", mpp_header())],
                true,
            ),
        ]
    });
    assert!(
        matches!(outcome, RunOutcome::MppChallenge { resource_url, .. } if resource_url == url)
    );
}

#[test]
fn curl_success_with_trailers_preserves_the_response_receipt() {
    if rerun_without_protocol_override("curl_success_with_trailers_preserves_the_response_receipt")
    {
        return;
    }
    let receipt = STANDARD.encode(
        serde_json::json!({
            "success": true, "transaction": "offline-fixture-signature", "network": "solana:devnet"
        })
        .to_string(),
    );
    let (outcome, _) = run_fixture(|_| {
        vec![response(
            "200 OK",
            &[("Payment-Response", receipt.clone())],
            true,
        )]
    });
    let RunOutcome::Completed {
        exit_code,
        response_headers,
        ..
    } = outcome
    else {
        panic!("expected completed response: {outcome:?}");
    };
    assert_eq!(exit_code, 0);
    assert_eq!(
        response_headers
            .iter()
            .find(|(k, _)| k == "payment-response")
            .unwrap()
            .1,
        receipt
    );
    let decoded = decode_response_receipt(&response_headers).unwrap();
    assert_eq!(
        decoded.signature.as_deref(),
        Some("offline-fixture-signature")
    );
    assert!(
        !response_headers
            .iter()
            .any(|(k, _)| k == "x-fixture-timing")
    );
}
