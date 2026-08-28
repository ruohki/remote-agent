//! SPKI pinning against a local TLS stub: correct pin connects, wrong pin is refused, a pin for
//! another host does not apply. Uses `REMOTE_AGENT_CA_PEM` to trust the stub's self-signed cert
//! (the same mechanism enterprises use for private CAs).

use remote_agent::transport;
use std::io::Write;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Stub {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    pem_path: std::path::PathBuf,
}

/// One certificate for the whole test binary: the trust store is cached process-wide.
fn stub() -> &'static Stub {
    static STUB: OnceLock<Stub> = OnceLock::new();
    STUB.get_or_init(|| {
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let dir = std::env::temp_dir().join(format!("remote-agent-tls-pin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pem_path = dir.join("ca.pem");
        let mut f = std::fs::File::create(&pem_path).unwrap();
        f.write_all(ck.cert.pem().as_bytes()).unwrap();
        std::env::set_var("REMOTE_AGENT_CA_PEM", &pem_path);
        Stub {
            cert_der: ck.cert.der().to_vec(),
            key_der: ck.signing_key.serialize_der(),
            pem_path,
        }
    })
}

/// Global pin state is process-wide: serialise the tests that touch it (async-aware lock,
/// it is held across awaits).
async fn lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

async fn serve_once() -> u16 {
    let s = stub();
    let cert = rustls_pki_types::CertificateDer::from(s.cert_der.clone());
    let key = rustls_pki_types::PrivateKeyDer::try_from(s.key_der.clone()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = tls.read(&mut buf).await;
                let body = b"ok";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(resp.as_bytes()).await;
                let _ = tls.write_all(body).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    port
}

fn pin_of_stub() -> String {
    transport::spki_sha256_base64(&stub().cert_der).unwrap()
}

async fn get(url: &str) -> anyhow::Result<u16> {
    let client = transport::http_client(url, Duration::from_secs(5))?;
    let resp = client.get(url).send().await?;
    Ok(resp.status().as_u16())
}

#[tokio::test]
async fn correct_pin_connects_and_wrong_pin_is_refused() {
    let _g = lock().await;
    assert!(stub().pem_path.exists());
    let port = serve_once().await;
    let url = format!("https://localhost:{port}/api/info");

    transport::set_console_pin(Some(&pin_of_stub()), &url).unwrap();
    let status = get(&url)
        .await
        .unwrap_or_else(|e| panic!("correct pin must connect: {e:#}"));
    assert_eq!(status, 200);

    let wrong = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [9u8; 32]);
    transport::set_console_pin(Some(&wrong), &url).unwrap();
    let err = get(&url).await.expect_err("wrong pin must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("pin") || msg.contains("certificate"),
        "error should explain the pin mismatch: {msg}"
    );
    transport::set_console_pin(None, &url).unwrap();
}

#[tokio::test]
async fn pin_for_another_host_does_not_apply() {
    let _g = lock().await;
    let port = serve_once().await;
    let url = format!("https://localhost:{port}/api/info");
    let wrong = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [1u8; 32]);
    transport::set_console_pin(Some(&wrong), "https://console.example.com").unwrap();
    let status = get(&url)
        .await
        .unwrap_or_else(|e| panic!("pin is scoped to the console host: {e:#}"));
    assert_eq!(status, 200);
    transport::set_console_pin(None, &url).unwrap();
}

#[tokio::test]
async fn plain_http_to_public_host_is_refused_without_override() {
    stub(); // the trust store is cached process-wide: set REMOTE_AGENT_CA_PEM before any client
    let _g = lock().await;
    transport::set_insecure(false);
    let err = transport::http_client("http://console.example.com", Duration::from_secs(1))
        .expect_err("plain http to a public host must be refused");
    assert!(format!("{err:#}").contains("HTTPS"));
    assert!(transport::http_client("http://127.0.0.1:1", Duration::from_secs(1)).is_ok());
}
