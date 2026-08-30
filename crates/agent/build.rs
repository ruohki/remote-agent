//! Windows resources: the executable's icon and its version information.
//!
//! Without these the agent is a nameless generic-icon binary in Explorer, the taskbar and
//! Alt-Tab, and its Properties → Details tab is empty — which is also a little of the
//! reputation SmartScreen weighs. `assets/app.ico` is the same icon the app draws for itself
//! (`branding::app_icon`), so a download looks like what it launches.
fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/agent.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/agent.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embedding Windows resources");
    }
}
