//! Small OS helpers shared by the other modules.
//!
//! TODO(builder-core):
//! * `logged_in_user()` — macOS: `SCDynamicStoreCopyConsoleUser` / `stat /dev/console`;
//!   Windows: `WTSQuerySessionInformation(WTSUserName)` for the active console session.
//! * `run_on_main(f)` — macOS: dispatch to the main queue (`dispatch2::Queue::main().exec_async`)
//!   because AppKit dialogs must run there; the `run` command must therefore pump the
//!   main run loop and drive tokio from a secondary thread.
//! * `doctor()` — print: enrollment status, screen recording permission
//!   (`CGPreflightScreenCaptureAccess`), accessibility permission (`AXIsProcessTrusted`),
//!   displays found, available encoders (hardware/software), reachability of the console.

use anyhow::Result;

pub fn logged_in_user() -> Option<String> {
    None
}

pub fn doctor() -> Result<()> {
    println!("remote-agent {}", crate::AGENT_VERSION);
    println!("os        : {:?} / {:?}", protocol::common::Os::current(), protocol::common::Arch::current());
    match crate::capture::list_displays() {
        Ok(d) => println!("displays  : {}", d.len()),
        Err(e) => println!("displays  : error: {e:#}"),
    }
    println!("encoders  : {:?}", crate::encode::available_codecs());
    Ok(())
}
