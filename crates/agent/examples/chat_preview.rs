//! Visual preview of the branded agent application window (status + chat).
//! Run with `REMOTE_AGENT_SHOW_WINDOWS=1 cargo run -p remote-agent --example chat_preview`.
use protocol::channel::ChatParty;
use protocol::common::OperatorInfo;
use remote_agent::app::{self, AppOptions};
use remote_agent::chat::{ChatLine, ChatUi};
use std::sync::Arc;

fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn main() {
    let code = app::run(
        || {
            // Give the UI a moment to come up, then simulate a session with a few messages.
            std::thread::sleep(std::time::Duration::from_millis(600));
            app::set_console_status(true);
            app::set_device_info("Front desk iMac", "dev_preview123");
            let handle = app::AppChatUi
                .open(
                    &OperatorInfo {
                        id: "op".into(),
                        name: "Alex (support)".into(),
                    },
                    Arc::new(|t| println!("[device→operator] {t}")),
                    Arc::new(|| println!("[disconnect pressed]")),
                )
                .expect("open app chat");
            handle.set_visible(true);
            let base = now();
            for (i, (from, text)) in [
                (ChatParty::Operator, "Hi! I'm Alex from IT support 👋"),
                (ChatParty::Operator, "Can you see this window?"),
                (ChatParty::Device, "Yes! This looks great."),
                (ChatParty::Operator, "Perfect, taking a look now."),
            ]
            .into_iter()
            .enumerate()
            {
                handle.push_line(&ChatLine {
                    from,
                    text: text.into(),
                    ts_ms: base + i as u64 * 1000,
                });
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
            0
        },
        AppOptions {
            show_on_start: true,
            installable: true,
        },
    );
    std::process::exit(code);
}
