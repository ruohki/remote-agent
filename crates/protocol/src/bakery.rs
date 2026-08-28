//! Agent "bakery": per-console configuration and branding baked into the agent binary.
//!
//! The console appends a **trailer** to a release binary so the resulting file needs no
//! arguments and carries its branding:
//!
//! ```text
//! [original executable bytes][payload JSON][u32 LE payload length][8-byte magic "RMTAGNT1"]
//! ```
//!
//! The payload is a [`BakedPayload`]: the [`BakedConfig`] plus the console's ed25519 public
//! key and a signature over the canonical JSON of the config. At startup the agent reads
//! its own executable ([`read_trailer`]), verifies the signature ([`verify_payload`]) and,
//! when valid, uses the embedded console URL / token / branding. A binary without a trailer
//! behaves as before (plain CLI).
//!
//! Security model: the signature proves the trailer was produced by the console whose public
//! key it carries and was not altered afterwards (e.g. someone rewriting the URL to steal
//! enrollments). Once enrolled, the agent pins that public key and refuses trailers from other
//! consoles on updates. Appending bytes invalidates macOS notarization / Windows Authenticode
//! signatures; signed distributions must place the payload in a signature-tolerant location
//! (PE certificate table, or a sidecar inside a signed `.app`) — the parser here only cares
//! about the trailer bytes, so both layouts can feed [`BakedPayload`] later.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Trailer magic (last 8 bytes of a baked binary).
pub const TRAILER_MAGIC: &[u8; 8] = b"RMTAGNT1";
/// Upper bound for a payload (logo included) to keep the tail scan cheap.
pub const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
/// Current payload schema version.
pub const BAKED_VERSION: u32 = 1;

/// Branding shown by the agent application window, banner and approval dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[ts(export)]
pub struct Branding {
    /// Product name, e.g. "Acme Remote Support".
    pub product_name: String,
    /// Accent colour as `#rrggbb`.
    pub accent: String,
    /// PNG logo, base64 (≤ 512 KiB decoded), optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub logo_png_base64: Option<String>,
    /// Short text shown to the person at the device ("Support by Acme IT, +49 …").
    #[serde(default)]
    pub support_text: String,
    /// Organisation name shown in the About section.
    #[serde(default)]
    pub organization: String,
}

/// What a baked binary knows about its console.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BakedConfig {
    pub version: u32,
    /// Console base URL, e.g. `https://remote.example.com`.
    pub server_url: String,
    /// Enrollment token to use automatically (long-lived install tokens or one-off quick-support tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub enroll_token: Option<String>,
    /// Quick-support build: run in the foreground as a temporary device, offer "Install as service".
    #[serde(default)]
    pub quick_support: bool,
    pub branding: Branding,
    /// Unix epoch seconds when the trailer was produced.
    pub issued_at: u64,
}

/// Signed envelope stored in the trailer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BakedPayload {
    pub config: BakedConfig,
    /// Console ed25519 public key, base64 (32 bytes).
    pub public_key: String,
    /// ed25519 signature over [`canonical_json`] of `config`, base64 (64 bytes).
    pub signature: String,
}

/// Deterministic JSON of the config used for signing (serde_json keeps struct field order,
/// so plain serialization of the typed struct is canonical enough).
pub fn canonical_json(config: &BakedConfig) -> Vec<u8> {
    serde_json::to_vec(config).expect("BakedConfig serializes")
}

/// Sign a config with the console key and build the payload.
pub fn sign_payload(config: BakedConfig, key: &SigningKey) -> BakedPayload {
    let sig = key.sign(&canonical_json(&config));
    BakedPayload {
        config,
        public_key: b64(key.verifying_key().as_bytes()),
        signature: b64(&sig.to_bytes()),
    }
}

/// Verify the payload's signature against the public key it carries. Returns the key on
/// success so the caller can pin it.
pub fn verify_payload(payload: &BakedPayload) -> Result<VerifyingKey, String> {
    let pk = unb64(&payload.public_key)?;
    let pk: [u8; 32] = pk
        .try_into()
        .map_err(|_| "public key must be 32 bytes".to_string())?;
    let key = VerifyingKey::from_bytes(&pk).map_err(|e| format!("invalid public key: {e}"))?;
    let sig = unb64(&payload.signature)?;
    let sig: [u8; 64] = sig
        .try_into()
        .map_err(|_| "signature must be 64 bytes".to_string())?;
    let sig = Signature::from_bytes(&sig);
    if payload.config.version != BAKED_VERSION {
        return Err(format!(
            "unsupported baked config version {}",
            payload.config.version
        ));
    }
    key.verify(&canonical_json(&payload.config), &sig)
        .map_err(|_| "signature does not match".to_string())?;
    Ok(key)
}

/// Append a trailer for `payload` to `binary`.
pub fn append_trailer(binary: &[u8], payload: &BakedPayload) -> Vec<u8> {
    let json = serde_json::to_vec(payload).expect("BakedPayload serializes");
    let mut out = Vec::with_capacity(binary.len() + json.len() + 12);
    out.extend_from_slice(binary);
    out.extend_from_slice(&json);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(TRAILER_MAGIC);
    out
}

/// Parse a trailer from the end of a binary. `Ok(None)` when there is none.
pub fn read_trailer(bytes: &[u8]) -> Result<Option<BakedPayload>, String> {
    if bytes.len() < 12 || &bytes[bytes.len() - 8..] != TRAILER_MAGIC {
        return Ok(None);
    }
    let len_start = bytes.len() - 12;
    let len = u32::from_le_bytes(bytes[len_start..len_start + 4].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD_BYTES || len > len_start {
        return Err("corrupt trailer length".into());
    }
    let json = &bytes[len_start - len..len_start];
    serde_json::from_slice(json)
        .map(Some)
        .map_err(|e| format!("corrupt trailer payload: {e}"))
}

/// Strip a trailer (if any) — used before re-baking an already baked binary.
pub fn strip_trailer(bytes: &[u8]) -> &[u8] {
    if bytes.len() >= 12 && &bytes[bytes.len() - 8..] == TRAILER_MAGIC {
        let len_start = bytes.len() - 12;
        let len = u32::from_le_bytes(bytes[len_start..len_start + 4].try_into().unwrap()) as usize;
        if len <= len_start {
            return &bytes[..len_start - len];
        }
    }
    bytes
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("invalid base64: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BakedConfig {
        BakedConfig {
            version: BAKED_VERSION,
            server_url: "https://remote.example.com".into(),
            enroll_token: Some("tok".into()),
            quick_support: true,
            branding: Branding {
                product_name: "Acme Remote".into(),
                accent: "#3b82f6".into(),
                logo_png_base64: None,
                support_text: "Call +49 123".into(),
                organization: "Acme".into(),
            },
            issued_at: 1_700_000_000,
        }
    }

    #[test]
    fn sign_append_read_verify_roundtrip() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let payload = sign_payload(sample(), &key);
        let baked = append_trailer(b"MZ...binary...", &payload);
        let back = read_trailer(&baked).unwrap().expect("trailer present");
        assert_eq!(back, payload);
        let pk = verify_payload(&back).unwrap();
        assert_eq!(pk, key.verifying_key());
        assert_eq!(strip_trailer(&baked), b"MZ...binary...");
        assert!(read_trailer(b"plain binary").unwrap().is_none());
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let mut payload = sign_payload(sample(), &key);
        payload.config.server_url = "https://evil.example".into();
        assert!(verify_payload(&payload).is_err());
    }

    #[test]
    fn rebaking_replaces_the_old_trailer() {
        let key = SigningKey::from_bytes(&[1u8; 32]);
        let first = append_trailer(b"bin", &sign_payload(sample(), &key));
        let mut cfg = sample();
        cfg.branding.product_name = "Other".into();
        let second = append_trailer(strip_trailer(&first), &sign_payload(cfg, &key));
        let back = read_trailer(&second).unwrap().unwrap();
        assert_eq!(back.config.branding.product_name, "Other");
        assert_eq!(strip_trailer(&second), b"bin");
    }
}
