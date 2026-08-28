//! Session chat: a platform-independent model driven by the session task, and a [`ChatUi`]
//! trait behind which the native windows (NSPanel / Win32) live. A future tray/menu-bar
//! application window can implement [`ChatUi`] without touching the session code.

use anyhow::Result;
use protocol::channel::ChatParty;
use protocol::common::OperatorInfo;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// One line of the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    pub from: ChatParty,
    pub text: String,
    /// Unix epoch milliseconds.
    pub ts_ms: u64,
}

/// Transcript + unread bookkeeping for one session.
#[derive(Debug, Clone)]
pub struct ChatModel {
    operator: OperatorInfo,
    lines: Vec<ChatLine>,
    unread: usize,
}

/// Longest message accepted in either direction (characters).
pub const MAX_CHAT_CHARS: usize = 4000;
/// Transcript lines kept in memory per session.
pub const MAX_CHAT_LINES: usize = 500;

impl ChatModel {
    pub fn new(operator: OperatorInfo) -> Self {
        Self {
            operator,
            lines: Vec::new(),
            unread: 0,
        }
    }

    pub fn operator(&self) -> &OperatorInfo {
        &self.operator
    }

    /// Append a line (trimmed, length-capped). Operator lines count as unread until
    /// [`mark_read`](Self::mark_read). Returns `None` for empty messages.
    pub fn push(&mut self, from: ChatParty, text: &str, ts_ms: Option<u64>) -> Option<ChatLine> {
        let text: String = text.trim().chars().take(MAX_CHAT_CHARS).collect();
        if text.is_empty() {
            return None;
        }
        let line = ChatLine {
            from,
            text,
            ts_ms: ts_ms.unwrap_or_else(now_ms),
        };
        if from == ChatParty::Operator {
            self.unread += 1;
        }
        self.lines.push(line.clone());
        if self.lines.len() > MAX_CHAT_LINES {
            let excess = self.lines.len() - MAX_CHAT_LINES;
            self.lines.drain(..excess);
        }
        Some(line)
    }

    pub fn mark_read(&mut self) {
        self.unread = 0;
    }

    pub fn unread(&self) -> usize {
        self.unread
    }

    pub fn lines(&self) -> &[ChatLine] {
        &self.lines
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Live handle to a chat window; dropping it closes the window.
pub trait ChatHandle: Send + Sync {
    fn push_line(&self, line: &ChatLine);
    fn set_visible(&self, visible: bool);
}

/// Factory for the device-side chat presentation.
pub trait ChatUi: Send + Sync + 'static {
    /// Open the chat window for `operator`. `on_send` is invoked with the text the local user
    /// typed; `on_disconnect` when they press *Disconnect*.
    fn open(
        &self,
        operator: &OperatorInfo,
        on_send: Arc<dyn Fn(String) + Send + Sync>,
        on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn ChatHandle>>;
}

/// The native window (NSPanel on macOS, Win32 on Windows).
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeChatUi;

impl ChatUi for NativeChatUi {
    fn open(
        &self,
        operator: &OperatorInfo,
        on_send: Arc<dyn Fn(String) + Send + Sync>,
        on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn ChatHandle>> {
        crate::platform::open_chat(&operator.name, on_send, on_disconnect)
    }
}

/// Headless variant (tests, no UI loop).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoChatUi;

struct NoHandle;
impl ChatHandle for NoHandle {
    fn push_line(&self, _line: &ChatLine) {}
    fn set_visible(&self, _visible: bool) {}
}

impl ChatUi for NoChatUi {
    fn open(
        &self,
        _operator: &OperatorInfo,
        _on_send: Arc<dyn Fn(String) + Send + Sync>,
        _on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Box<dyn ChatHandle>> {
        Ok(Box::new(NoHandle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_counts_operator_lines_only() {
        let mut m = ChatModel::new(OperatorInfo {
            id: "o".into(),
            name: "Op".into(),
        });
        assert!(m.push(ChatParty::Operator, "  ", None).is_none());
        m.push(ChatParty::Operator, "hi", Some(1));
        m.push(ChatParty::Device, "hello", Some(2));
        assert_eq!(m.unread(), 1);
        m.mark_read();
        assert_eq!(m.unread(), 0);
        assert_eq!(m.lines().len(), 2);
        assert_eq!(m.lines()[0].ts_ms, 1);
    }

    #[test]
    fn caps_length_and_history() {
        let mut m = ChatModel::new(OperatorInfo {
            id: "o".into(),
            name: "Op".into(),
        });
        let long = "x".repeat(MAX_CHAT_CHARS + 10);
        let line = m.push(ChatParty::Device, &long, None).unwrap();
        assert_eq!(line.text.chars().count(), MAX_CHAT_CHARS);
        for i in 0..(MAX_CHAT_LINES + 5) {
            m.push(ChatParty::Device, &i.to_string(), None);
        }
        assert_eq!(m.lines().len(), MAX_CHAT_LINES);
    }
}
