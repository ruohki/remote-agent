//! Drives the real hub client (`run_agent`) against a mock console WebSocket server and
//! checks the handshake, heartbeats and ping/pong.

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
         device_id = \"dev_mock\"\n\
         device_secret = \"s3cr3t\"\n"
    );
    std::fs::write(dir.join("agent.toml"), toml).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_heartbeat_ping() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("warn")
        .try_init();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_url = format!("http://127.0.0.1:{port}");

    let tmp = tempfile::tempdir().unwrap();
    write_enrolled_config(tmp.path(), &server_url);

    // Run the real agent on its own thread + runtime (its future is not `Send`). A timeout
    // bounds the thread's lifetime so it does not outlive the test binary.
    let paths = remote_agent::config::Paths {
        dir: tmp.path().to_path_buf(),
    };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let _ =
                tokio::time::timeout(Duration::from_secs(30), remote_agent::hub::run_agent(paths))
                    .await;
        });
    });

    // Accept one connection and speak the protocol.
    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("agent never connected")
        .unwrap();
    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

    // 1. hello
    let hello = next_json(&mut ws).await;
    match hello {
        AgentToConsole::Hello {
            protocol_version,
            device_id,
            device_secret,
            capabilities,
            ..
        } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(device_id, "dev_mock");
            assert_eq!(device_secret, "s3cr3t");
            assert!(
                capabilities
                    .codecs
                    .contains(&protocol::common::VideoCodec::H264),
                "agent should always advertise H264"
            );
        }
        other => panic!("expected hello, got {other:?}"),
    }

    // 2. hello_ack with a short heartbeat interval
    let cfg = AgentConfig {
        heartbeat_interval_s: 1,
        ..AgentConfig::default()
    };
    send_json(
        &mut ws,
        &ConsoleToAgent::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            server_time_ms: 0,
            config: cfg,
        },
    )
    .await;

    // 3. a heartbeat should arrive within a couple of seconds
    let hb = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match next_json(&mut ws).await {
                AgentToConsole::Heartbeat { uptime_s, .. } => return uptime_s,
                _ => continue,
            }
        }
    })
    .await
    .expect("no heartbeat received");
    let _ = hb;

    // 4. ping → pong
    send_json(&mut ws, &ConsoleToAgent::Ping { nonce: 4242 }).await;
    let pong = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let AgentToConsole::Pong { nonce } = next_json(&mut ws).await {
                return nonce;
            }
        }
    })
    .await
    .expect("no pong received");
    assert_eq!(pong, 4242);

    // 5. goodbye → the agent should close this connection cleanly
    send_json(
        &mut ws,
        &ConsoleToAgent::Goodbye {
            reason: "test over".into(),
        },
    )
    .await;

    // The agent thread exits on its own 30s timeout.
}

async fn next_json<S>(ws: &mut S) -> AgentToConsole
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("timed out reading from agent")
            .expect("agent stream ended")
            .expect("ws error");
        match msg {
            Message::Text(t) => return serde_json::from_str(&t).expect("valid AgentToConsole"),
            Message::Binary(b) => return serde_json::from_slice(&b).expect("valid AgentToConsole"),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => panic!("agent closed the connection unexpectedly"),
            _ => continue,
        }
    }
}

async fn send_json<S>(ws: &mut S, msg: &ConsoleToAgent)
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    ws.send(Message::text(serde_json::to_string(msg).unwrap()))
        .await
        .unwrap();
}
