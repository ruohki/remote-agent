//! Console transport policy: TLS enforcement, SPKI pinning and the HTTP / WebSocket clients
//! every console-facing module uses.
//!
//! * **Policy** — `http://` / `ws://` console URLs are refused unless the host is local
//!   (loopback, RFC 1918 / ULA, link-local) or the operator explicitly opted in with
//!   `--insecure` / `REMOTE_AGENT_INSECURE=1` (logged loudly, once).
//! * **Pinning** — when a console TLS SPKI pin is known (baked trailer, persisted at
//!   enrollment) every TLS connection to the console additionally requires the leaf
//!   certificate's SubjectPublicKeyInfo SHA-256 to equal the pin. Chain validation against the
//!   system/webpki roots (plus `REMOTE_AGENT_CA_PEM` extra roots for private CAs) still applies.
//! * The pin can only change through a signed baked config (re-bake / re-enroll); there is no
//!   in-band rotation message yet (see the follow-up note in the report).

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use parking_lot::RwLock;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

static INSECURE: AtomicBool = AtomicBool::new(false);
static INSECURE_WARNED: AtomicBool = AtomicBool::new(false);
/// (console host, SPKI SHA-256) — the pin applies only to connections to that host.
static PIN: RwLock<Option<(String, [u8; 32])>> = RwLock::new(None);

/// Allow plain-text console URLs outside local/private ranges (CLI `--insecure`).
pub fn set_insecure(allowed: bool) {
    INSECURE.store(allowed, Ordering::SeqCst);
}

pub fn insecure_allowed() -> bool {
    INSECURE.load(Ordering::SeqCst)
}

/// Set (or clear) the process-wide console SPKI pin (base64 SHA-256 of the SPKI). The pin is
/// enforced only for TLS connections to the host of `console_url`, so downloads from other
/// hosts (release assets) are unaffected.
pub fn set_console_pin(pin: Option<&str>, console_url: &str) -> Result<()> {
    let parsed = match pin {
        Some(p) if !p.trim().is_empty() => {
            let host = url::Url::parse(console_url.trim())
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                .with_context(|| format!("console URL {console_url} has no host"))?;
            Some((host, parse_pin(p)?))
        }
        _ => None,
    };
    *PIN.write() = parsed;
    Ok(())
}

pub fn console_pin_active() -> bool {
    PIN.read().is_some()
}

/// Parse a base64 SPKI SHA-256 pin (32 bytes).
pub fn parse_pin(pin: &str) -> Result<[u8; 32]> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(pin.trim())
        .context("SPKI pin is not valid base64")?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("SPKI pin must be 32 bytes (SHA-256)"))
}

/// SHA-256 of the SubjectPublicKeyInfo of a DER certificate.
pub fn spki_sha256(cert_der: &[u8]) -> Result<[u8; 32]> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| anyhow!("parsing certificate: {e}"))?;
    Ok(Sha256::digest(cert.tbs_certificate.subject_pki.raw).into())
}

pub fn spki_sha256_base64(cert_der: &[u8]) -> Result<String> {
    Ok(base64::engine::general_purpose::STANDARD.encode(spki_sha256(cert_der)?))
}

// ─── URL policy ───────────────────────────────────────────────────────────────────────

/// Whether `host` is loopback, link-local, RFC 1918 or ULA (plain HTTP tolerated there).
pub fn is_local_or_private_host(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local
        }
        Err(_) => false,
    }
}

/// Classify a console URL. `Ok(false)` = secure or local; `Ok(true)` = insecure but allowed
/// by the override; `Err` = insecure and refused.
pub fn check_console_url(url: &str) -> Result<bool> {
    let parsed =
        url::Url::parse(url.trim()).with_context(|| format!("invalid console URL {url}"))?;
    let scheme = parsed.scheme();
    let host = parsed.host_str().unwrap_or("");
    match scheme {
        "https" | "wss" => Ok(false),
        "http" | "ws" => {
            if is_local_or_private_host(host) {
                return Ok(false);
            }
            if insecure_allowed() {
                if !INSECURE_WARNED.swap(true, Ordering::SeqCst) {
                    tracing::error!(
                        %url,
                        "INSECURE: talking to a public console over plain {scheme} because \
                         --insecure / REMOTE_AGENT_INSECURE is set. Credentials and session \
                         signaling are not encrypted."
                    );
                }
                Ok(true)
            } else {
                bail!(
                    "refusing plain {scheme} console URL {url}: the console must use HTTPS \
                     (or run on localhost / a private network). Use --insecure to override."
                )
            }
        }
        other => bail!("unsupported console URL scheme {other} in {url}"),
    }
}

// ─── TLS configuration with pinning ─────────────────────────────────────────────────

#[derive(Debug)]
struct PinnedVerifier {
    inner: Arc<WebPkiServerVerifier>,
    /// (console host, pin) — checked only when `server_name` is that host.
    pin: Option<(String, [u8; 32])>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        let applies = match (&self.pin, server_name) {
            (Some((host, _)), ServerName::DnsName(name)) => {
                name.as_ref().eq_ignore_ascii_case(host)
            }
            (Some((host, _)), ServerName::IpAddress(ip)) => {
                std::net::IpAddr::from(*ip).to_string() == *host
            }
            _ => false,
        };
        if let Some((_, pin)) = self.pin.as_ref().filter(|_| applies) {
            let pin = *pin;
            let actual = spki_sha256(end_entity.as_ref()).map_err(|e| {
                rustls::Error::General(format!("cannot read console certificate: {e}"))
            })?;
            if actual != pin {
                return Err(rustls::Error::General(
                    "console certificate does not match the pinned public key (SPKI pin \
                     mismatch): possible interception or a console certificate rotation \
                     that requires re-enrollment"
                        .into(),
                ));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Extra trust anchors (PEM bundle) for consoles behind a private CA.
fn extra_roots() -> Vec<CertificateDer<'static>> {
    use rustls_pki_types::pem::PemObject;
    let Some(path) = std::env::var_os("REMOTE_AGENT_CA_PEM") else {
        return Vec::new();
    };
    match CertificateDer::pem_file_iter(&path) {
        Ok(iter) => iter.filter_map(|c| c.ok()).collect(),
        Err(e) => {
            tracing::warn!("REMOTE_AGENT_CA_PEM {}: {e}", path.to_string_lossy());
            Vec::new()
        }
    }
}

fn roots() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    Arc::clone(ROOTS.get_or_init(|| {
        let mut store = RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in extra_roots() {
            if let Err(e) = store.add(cert) {
                tracing::warn!("ignoring extra CA certificate: {e}");
            }
        }
        Arc::new(store)
    }))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    static P: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    Arc::clone(P.get_or_init(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider())))
}

/// rustls client config for the console: system roots (+ extra CA) and the active pin.
pub fn tls_config() -> Result<Arc<rustls::ClientConfig>> {
    let inner = WebPkiServerVerifier::builder_with_provider(roots(), provider())
        .build()
        .map_err(|e| anyhow!("building certificate verifier: {e}"))?;
    let verifier = Arc::new(PinnedVerifier {
        inner,
        pin: PIN.read().clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow!("TLS protocol versions: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// HTTP client for console requests (policy-checked URL, pinned TLS).
pub fn http_client(console_url: &str, timeout: Duration) -> Result<reqwest::Client> {
    check_console_url(console_url)?;
    let tls = tls_config()?;
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(format!("remote-agent/{}", crate::AGENT_VERSION))
        .use_preconfigured_tls(Arc::try_unwrap(tls).unwrap_or_else(|a| (*a).clone()))
        .build()
        .context("building HTTP client")
}

/// WebSocket connection to the console (policy-checked URL, pinned TLS).
pub async fn ws_connect(
    ws_url: &str,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
)> {
    check_console_url(ws_url)?;
    let connector = tokio_tungstenite::Connector::Rustls(tls_config()?);
    tokio_tungstenite::connect_async_tls_with_config(ws_url, None, true, Some(connector))
        .await
        .with_context(|| format!("connecting to {ws_url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_private_hosts() {
        for h in [
            "localhost",
            "127.0.0.1",
            "10.1.2.3",
            "192.168.69.207",
            "172.16.0.9",
            "::1",
            "[fd12::1]",
            "fe80::1",
            "169.254.1.1",
            "dev.localhost",
        ] {
            assert!(is_local_or_private_host(h), "{h} should be local/private");
        }
        for h in [
            "example.com",
            "8.8.8.8",
            "2001:db8::1",
            "console.corp.example",
        ] {
            assert!(!is_local_or_private_host(h), "{h} should be public");
        }
    }

    #[test]
    fn url_policy() {
        set_insecure(false);
        assert!(!check_console_url("https://console.example.com").unwrap());
        assert!(!check_console_url("wss://console.example.com/ws/agent").unwrap());
        assert!(!check_console_url("http://localhost:8080").unwrap());
        assert!(!check_console_url("ws://192.168.1.5:8080/ws/agent").unwrap());
        assert!(check_console_url("http://console.example.com").is_err());
        assert!(check_console_url("ws://8.8.8.8:8080/ws/agent").is_err());
        assert!(check_console_url("ftp://console.example.com").is_err());
        set_insecure(true);
        assert!(check_console_url("http://console.example.com").unwrap());
        set_insecure(false);
    }

    #[test]
    fn pin_parsing_and_matching() {
        let pin = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert_eq!(parse_pin(&pin).unwrap(), [7u8; 32]);
        assert!(parse_pin("not base64!").is_err());
        assert!(parse_pin(&base64::engine::general_purpose::STANDARD.encode([1u8; 16])).is_err());
        set_console_pin(Some(&pin), "https://console.example.com").unwrap();
        assert!(console_pin_active());
        set_console_pin(None, "https://console.example.com").unwrap();
        assert!(!console_pin_active());
    }

    #[test]
    fn spki_of_self_signed_cert_is_stable() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let der = cert.cert.der();
        let a = spki_sha256(der).unwrap();
        let b = spki_sha256(der).unwrap();
        assert_eq!(a, b);
        assert_eq!(spki_sha256_base64(der).unwrap().len(), 44);
    }
}
