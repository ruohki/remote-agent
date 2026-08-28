//! Bake a signed configuration/branding trailer into an agent binary.
//!
//! ```text
//! cargo run -p remote-agent --example bake -- \
//!   --in target/debug/remote-agent --out /tmp/branded-agent \
//!   --key /tmp/console.ed25519 --server http://127.0.0.1:18100 \
//!   --token <ENROLL_TOKEN> --product "Acme Remote" --accent '#7c3aed' \
//!   --org "Acme Inc" --support "Call +49 123" [--logo logo.png] [--quick]
//! ```
//!
//! The signing key file is 32 raw bytes; if it does not exist it is generated and written.
//! This mirrors what the console does server-side (`protocol::bakery`).

use ed25519_dalek::SigningKey;
use protocol::bakery::{
    append_trailer, sign_payload, strip_trailer, BakedConfig, Branding, BAKED_VERSION,
};
use std::collections::HashMap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut m: HashMap<String, String> = HashMap::new();
    let mut quick = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--quick" {
            quick = true;
            i += 1;
            continue;
        }
        if let Some(key) = a.strip_prefix("--") {
            let val = args.get(i + 1).cloned().unwrap_or_default();
            m.insert(key.to_string(), val);
            i += 2;
        } else {
            i += 1;
        }
    }
    let get = |k: &str| m.get(k).cloned().unwrap_or_default();
    let require = |k: &str| {
        m.get(k)
            .cloned()
            .unwrap_or_else(|| panic!("--{k} is required"))
    };

    let key_path = require("key");
    let key = load_or_create_key(&key_path);

    let logo = m.get("logo").map(|p| {
        use base64::Engine;
        let bytes = std::fs::read(p).expect("reading logo");
        base64::engine::general_purpose::STANDARD.encode(bytes)
    });

    let cfg = BakedConfig {
        version: BAKED_VERSION,
        server_url: require("server"),
        enroll_token: m.get("token").cloned().filter(|s| !s.is_empty()),
        quick_support: quick,
        branding: Branding {
            product_name: get("product"),
            accent: if get("accent").is_empty() {
                "#3b82f6".into()
            } else {
                get("accent")
            },
            logo_png_base64: logo,
            support_text: get("support"),
            organization: get("org"),
        },
        issued_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };

    let base = std::fs::read(require("in")).expect("reading input binary");
    let base = strip_trailer(&base); // allow re-baking
    let payload = sign_payload(cfg, &key);
    let out = append_trailer(base, &payload);
    let out_path = require("out");
    std::fs::write(&out_path, &out).expect("writing output");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755));
    }
    println!(
        "wrote {out_path} ({} bytes, +{} trailer)",
        out.len(),
        out.len() - base.len()
    );
    println!("public key: {}", payload.public_key);
}

fn load_or_create_key(path: &str) -> SigningKey {
    if let Ok(bytes) = std::fs::read(path) {
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .expect("key file must be 32 bytes");
        SigningKey::from_bytes(&arr)
    } else {
        let mut seed = [0u8; 32];
        rand::fill(&mut seed);
        std::fs::write(path, seed).expect("writing key");
        println!("generated new signing key at {path}");
        SigningKey::from_bytes(&seed)
    }
}
