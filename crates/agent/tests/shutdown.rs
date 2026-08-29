//! A shutdown request while connected must end the console connection cleanly: the agent
//! sends a close frame and `run_agent` returns `Ok` instead of reconnecting.
//!
//! Lives in its own test binary because the shutdown flag is process-wide.

use futures_util::{SinkExt, StreamExt};
use protocol::agent::{AgentToConsole, ConsoleToAgent};
use protocol::config::AgentConfig;
use protocol::PROTOCOL_VERSION;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

fn write_enrolled_config(dir: &std::path::Path, server_url: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let toml = format!(
        "server_url = \"{server_url}\"\n\
         device_id = \"dev_shutdown\"\n\
         device_secret = \"s3cr3t\"\n"
    );
    std::fs::write(dir.join("agent.toml"), toml).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_request_closes_the_connection_and_returns() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_url = format!("http://127.0.0.1:{port}");

    let tmp = tempfile::tempdir().unwrap();
    write_enrolled_config(tmp.path(), &server_url);
    let paths = remote_agent::config::Paths {
        dir: tmp.path().to_path_buf(),
    };

    // The real agent on its own thread + runtime; it reports how `run_agent` ended.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            match tokio::time::timeout(Duration::from_secs(30), remote_agent::hub::run_agent(paths))
                .await
            {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(_) => Err("run_agent did not return within 30 s".into()),
            }
        });
        let _ = done_tx.send(result);
    });

    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("agent never connected")
        .unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

    // hello → hello_ack
    match next_json(&mut ws).await {
        Some(AgentToConsole::Hello {
            protocol_version, ..
        }) => {
            assert_eq!(protocol_version, PROTOCOL_VERSION)
        }
        other => panic!("expected hello, got {other:?}"),
    }
    send_json(
        &mut ws,
        &ConsoleToAgent::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            server_time_ms: 0,
            config: AgentConfig {
                heartbeat_interval_s: 1,
                ..AgentConfig::default()
            },
        },
    )
    .await;

    // Wait until the agent is in its main loop (first heartbeat), then ask the process to
    // stop, as SIGTERM / Quit would.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match next_json(&mut ws).await {
                Some(AgentToConsole::Heartbeat { .. }) => return,
                Some(_) => continue,
                None => panic!("connection ended before the first heartbeat"),
            }
        }
    })
    .await
    .expect("no heartbeat received");
    assert!(!remote_agent::shutdown::requested());
    remote_agent::shutdown::request("integration test");

    // The agent closes the socket itself, with a normal close frame naming the reason.
    let close = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(frame))) => return frame,
                Some(Ok(_)) => continue,
                other => panic!("connection ended without a close frame: {other:?}"),
            }
        }
    })
    .await
    .expect("no close frame within 5 s");
    let frame = close.expect("close frame carries a reason");
    assert!(
        frame.reason.contains("shutting down"),
        "close reason: {}",
        frame.reason
    );

    // … and `run_agent` returns Ok instead of reconnecting.
    let result = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("run_agent did not finish");
    assert_eq!(result, Ok(()));
    assert_eq!(
        remote_agent::shutdown::reason().as_deref(),
        Some("integration test")
    );

    // No reconnect attempt follows. (The branding refresh task's plain HTTP request may sit
    // in the backlog; only a `/ws/agent` upgrade would be a reconnect.)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Ok((mut stream, _))) = tokio::time::timeout_at(deadline, listener.accept()).await {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 512];
        let n = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(0);
        let request_line = String::from_utf8_lossy(&buf[..n])
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            !request_line.contains(protocol::AGENT_WS_PATH),
            "the agent reconnected after a shutdown: {request_line}"
        );
    }
}

async fn next_json<S>(ws: &mut S) -> Option<AgentToConsole>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match ws.next().await? {
            Ok(Message::Text(t)) => return serde_json::from_str(&t).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
}

async fn send_json<S>(ws: &mut S, msg: &ConsoleToAgent)
where
    S: SinkExt<Message> + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Debug,
{
    ws.send(Message::text(serde_json::to_string(msg).unwrap()))
        .await
        .unwrap();
}
