//! Windows resources: the executable's icon and its version information.
//!
//! Without these the agent is a nameless generic-icon binary in Explorer, the taskbar and
//! Alt-Tab, and its Properties → Details tab is empty — which is also a little of the
//! reputation SmartScreen weighs. `assets/app.ico` is the same icon the app draws for itself
//! (`branding::app_icon`), so a download looks like what it launches.
//!
//! The version block is generated from `CARGO_PKG_VERSION` rather than written by hand: a
//! resource script that has to be bumped alongside Cargo.toml is one that will disagree with
//! it.
use std::fmt::Write as _;

fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut parts: Vec<u16> = version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    parts.resize(4, 0);
    let mut quad = String::new();
    for (i, p) in parts.iter().enumerate() {
        let _ = write!(quad, "{}{p}", if i == 0 { "" } else { "," });
    }

    let rc = format!(
        r#"#include <winver.h>

1 ICON "{ico}"

VS_VERSION_INFO VERSIONINFO
FILEVERSION     {quad}
PRODUCTVERSION  {quad}
FILEOS          VOS_NT_WINDOWS32
FILETYPE        VFT_APP
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName",      "Remote Support"
            VALUE "FileDescription",  "Remote Support agent"
            VALUE "FileVersion",      "{version}"
            VALUE "InternalName",     "remote-agent"
            VALUE "OriginalFilename", "remote-agent.exe"
            VALUE "ProductName",      "Remote Support"
            VALUE "ProductVersion",   "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
        ico = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("assets/app.ico")
            .display()
            .to_string()
            .replace('\\', "\\\\"),
    );
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("agent.rc");
    std::fs::write(&out, rc).expect("writing the resource script");
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_optional()
        .expect("embedding Windows resources");
}
