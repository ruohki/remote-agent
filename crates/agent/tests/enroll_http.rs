//! `enroll::request` against a minimal in-process `/api/enroll`: a console rejection surfaces
//! as a downcastable `Rejected` carrying the console's message verbatim, and a success parses
//! into the `EnrollResponse` the agent saves.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve exactly one HTTP request: read headers + body, reply with `status` and `body`.
async fn serve_once(listener: &TcpListener, status: &str, body: &str) -> String {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let request = loop {
        let n = stream.read(&mut tmp).await.unwrap();
        if n == 0 {
            break String::from_utf8_lossy(&buf).into_owned();
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if let Some(head_end) = text.find("\r\n\r\n") {
            let content_length = text
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length: ")
                        .or(l.strip_prefix("Content-Length: "))
                })
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= head_end + 4 + content_length {
                break text;
            }
        }
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    stream.shutdown().await.ok();
    request
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejection_and_success() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = format!("http://127.0.0.1:{port}");

    // 1. wrong token → the console's message, verbatim
    let srv = serve_once(
        &listener,
        "401 Unauthorized",
        r#"{"error":{"code":"invalid_token","message":"invalid or expired token"}}"#,
    );
    let (request, result) = tokio::join!(
        srv,
        remote_agent::enroll::request(&server, "bad-token", Some("Front desk"))
    );
    assert!(
        request.starts_with("POST /api/enroll HTTP/1.1"),
        "{request}"
    );
    assert!(request.contains(r#""token":"bad-token""#), "{request}");
    assert!(
        request.contains(r#""display_name":"Front desk""#),
        "{request}"
    );
    let err = result.expect_err("401 must be an error");
    let rejected = err
        .downcast_ref::<remote_agent::enroll::Rejected>()
        .expect("downcastable rejection");
    assert_eq!(rejected.status, 401);
    assert_eq!(rejected.code, "invalid_token");
    assert_eq!(rejected.message, "invalid or expired token");
    assert_eq!(
        remote_agent::enroll::error_message(&err),
        "invalid or expired token"
    );

    // 2. good token → parsed response
    let body = serde_json::json!({
        "device_id": "dev_123",
        "device_secret": "s3cr3t",
        "server_url": format!("{server}/"),
        "config": protocol::config::AgentConfig::default(),
    })
    .to_string();
    let srv = serve_once(&listener, "201 Created", &body);
    let (_, result) = tokio::join!(
        srv,
        remote_agent::enroll::request(&server, "good-token", None)
    );
    let resp = result.expect("201 parses");
    assert_eq!(resp.device_id, "dev_123");
    assert_eq!(resp.device_secret, "s3cr3t");

    // 3. nothing listening → a short "cannot reach" message, not a reqwest dump
    drop(listener);
    let err = tokio::time::timeout(
        Duration::from_secs(30),
        remote_agent::enroll::request(&server, "tok", None),
    )
    .await
    .unwrap()
    .expect_err("connection refused");
    let msg = remote_agent::enroll::error_message(&err);
    assert!(msg.starts_with("Cannot reach the console:"), "{msg}");
    assert!(msg.len() < 120, "{msg}");
}
