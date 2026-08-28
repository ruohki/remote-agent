//! Visual preview of the agent chat window (WKWebView messaging UI). Not part of the product;
//! run with `REMOTE_AGENT_SHOW_WINDOWS=1 cargo run -p remote-agent --example chat_preview`.
use protocol::channel::ChatParty;
use remote_agent::chat::ChatLine;
use remote_agent::platform;
use std::sync::Arc;

fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn main() {
    let code = platform::run_main_loop(|| {
        let handle = match platform::open_chat(
            "Alex (support)",
            Arc::new(|t| println!("[device→operator] {t}")),
            Arc::new(|| println!("[disconnect pressed]")),
        ) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("open_chat failed: {e:#}");
                return 1;
            }
        };
        handle.set_visible(true);
        let base = now();
        for (i, (from, text)) in [
            (ChatParty::Operator, "Hi! I'm Alex from IT support 👋"),
            (
                ChatParty::Operator,
                "I can see your screen now — can you see this chat window?",
            ),
            (ChatParty::Device, "Yes! This looks way nicer than before."),
            (ChatParty::Device, "Much better than the old box."),
            (
                ChatParty::Operator,
                "Great. I'll take a look at that printer issue now.",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            handle.push_line(&ChatLine {
                from,
                text: text.into(),
                ts_ms: base + i as u64 * 1000,
            });
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        std::thread::sleep(std::time::Duration::from_secs(20));
        0
    });
    std::process::exit(code);
}
