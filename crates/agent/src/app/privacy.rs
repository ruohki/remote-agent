//! The privacy-screen windows: one opaque, focusable, always-on-top window per display that
//! shows the branded "Screen hidden" surface (`assets/privacy.html`) and is excluded from the
//! screen capture, so the operator keeps seeing the desktop while the room sees the notice.
//!
//! Deliberately *not* a variant of the annotation overlay: that one is transparent and
//! click-through; this one must be opaque, must take the pointer so nobody clicks through to
//! the desktop, and must hold keyboard focus so `Esc` reaches the page. The lifecycle
//! (engage / disengage / confirm) is driven by [`crate::privacy`], which owns the guarantee that
//! the windows come down again.

use super::AppEvent;
use crate::branding;
use crate::privacy::{confirm, Confirm, PrivacyScreenInfo};
use anyhow::{Context, Result};
use protocol::common::DisplayInfo;
use std::collections::HashMap;
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

const HTML: &str = include_str!("assets/privacy.html");

/// JS → Rust messages from a privacy page.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PrivacyIpc {
    /// The page is loaded on `display`.
    Ready { display: u32 },
    /// "Show screen" (or `Esc`): the person at the device lifts the screen.
    Reveal,
    /// "End session".
    End,
}

struct Shield {
    window: Window,
    webview: WebView,
    ready: bool,
    pending: Vec<String>,
}

impl Shield {
    fn eval(&mut self, js: String) {
        if self.ready {
            let _ = self.webview.evaluate_script(&js);
        } else {
            self.pending.push(js);
        }
    }
}

struct Engagement {
    info: PrivacyScreenInfo,
    displays: Vec<DisplayInfo>,
    /// Completed once every page is ready (or on failure).
    confirm: Option<Confirm<Result<(), String>>>,
}

/// All privacy windows of the current engagement, keyed by display index.
#[derive(Default)]
pub(super) struct PrivacyManager {
    shields: HashMap<u32, Shield>,
    engagement: Option<Engagement>,
}

impl PrivacyManager {
    pub(super) fn owns(&self, id: WindowId) -> bool {
        self.shields.values().any(|s| s.window.id() == id)
    }

    /// Create the windows (hidden) for every display; each is shown once its page is ready
    /// and the confirmation completes when all are up. Any failure tears everything down and
    /// reports it — the screen is all-or-nothing.
    pub(super) fn engage(
        &mut self,
        target: &EventLoopWindowTarget<AppEvent>,
        info: PrivacyScreenInfo,
        displays: Vec<DisplayInfo>,
        confirm_tx: Confirm<Result<(), String>>,
    ) {
        if self.engagement.is_some() {
            confirm(&confirm_tx, Err("privacy screen already engaged".into()));
            return;
        }
        let mut shields = HashMap::new();
        for d in &displays {
            match create_shield(target, d, &info) {
                Ok(s) => {
                    shields.insert(d.index, s);
                }
                Err(e) => {
                    tracing::warn!(display = d.index, "privacy window: {e:#}");
                    drop(shields);
                    confirm(&confirm_tx, Err(format!("display {}: {e:#}", d.index)));
                    return;
                }
            }
        }
        self.shields = shields;
        self.engagement = Some(Engagement {
            info,
            displays,
            confirm: Some(confirm_tx),
        });
        tracing::info!(displays = self.shields.len(), "privacy windows created");
    }

    /// Remove every window. Always confirms, even when nothing was up.
    pub(super) fn disengage(&mut self, confirm_tx: Confirm<()>) {
        let had = !self.shields.is_empty();
        self.shields.clear();
        if let Some(e) = self.engagement.take() {
            if let Some(c) = e.confirm {
                confirm(&c, Err("released before it was shown".into()));
            }
        }
        if had {
            tracing::info!("privacy windows removed");
        }
        confirm(&confirm_tx, ());
    }

    pub(super) fn on_ipc(&mut self, msg: PrivacyIpc) {
        match msg {
            PrivacyIpc::Ready { display } => self.on_ready(display),
            PrivacyIpc::Reveal => super::dispatch_privacy_reveal(),
            PrivacyIpc::End => super::dispatch_disconnect(),
        }
    }

    fn on_ready(&mut self, display: u32) {
        let Some(e) = self.engagement.as_ref() else {
            return;
        };
        let state = state_json(&e.info, &e.displays, display);
        let primary = e.displays.iter().any(|d| d.index == display && d.primary);
        if let Some(s) = self.shields.get_mut(&display) {
            if !s.ready {
                s.ready = true;
                let _ = s
                    .webview
                    .evaluate_script(&format!("window.__privacy&&window.__privacy.set({state});"));
                for js in std::mem::take(&mut s.pending) {
                    let _ = s.webview.evaluate_script(&js);
                }
                s.window.set_visible(true);
                if primary {
                    s.window.set_focus();
                }
            }
        }
        if self.shields.values().all(|s| s.ready) {
            if let Some(c) = self.engagement.as_mut().and_then(|e| e.confirm.take()) {
                confirm(&c, Ok(()));
            }
        }
    }

    /// The session is alive (posted every second while engaged).
    pub(super) fn heartbeat(&mut self) {
        for s in self.shields.values_mut() {
            s.eval("window.__privacy&&window.__privacy.heartbeat();".into());
        }
    }

    /// Branding changed while engaged.
    pub(super) fn rebrand(&mut self) {
        let Some(e) = self.engagement.as_ref() else {
            return;
        };
        let states: Vec<(u32, String)> = self
            .shields
            .keys()
            .map(|idx| (*idx, state_json(&e.info, &e.displays, *idx)))
            .collect();
        for (idx, state) in states {
            if let Some(s) = self.shields.get_mut(&idx) {
                s.window
                    .set_title(&format!("{} — screen hidden", branding::product_name()));
                s.eval(format!("window.__privacy&&window.__privacy.set({state});"));
            }
        }
    }
}

fn state_json(info: &PrivacyScreenInfo, displays: &[DisplayInfo], display: u32) -> String {
    let b = branding::current();
    serde_json::json!({
        "product": branding::product_name(),
        "accent": branding::accent(),
        "logo": b.logo_png_base64,
        "organization": b.organization,
        "support_text": b.support_text,
        "operator": info.operator,
        "device": info.device,
        "started_ms": info.started_ms,
        "display": { "index": display, "count": displays.len() },
        "backdrop": info.backdrops.get(&display),
    })
    .to_string()
}

fn create_shield(
    target: &EventLoopWindowTarget<AppEvent>,
    info: &DisplayInfo,
    _screen: &PrivacyScreenInfo,
) -> Result<Shield> {
    let scale = if info.scale > 0.0 {
        info.scale as f64
    } else {
        1.0
    };
    let size = LogicalSize::new(info.width as f64 / scale, info.height as f64 / scale);
    let position = LogicalPosition::new(info.x as f64, info.y as f64);
    let window = WindowBuilder::new()
        .with_title(format!("{} — screen hidden", branding::product_name()))
        .with_decorations(false)
        .with_transparent(false)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_focused(false)
        .with_visible(false)
        .with_inner_size(size)
        .with_position(position)
        .build(target)
        .context("creating privacy window")?;
    window.set_visible_on_all_workspaces(true);
    // No dev bypass here: a privacy window that reaches the operator's stream is the feature
    // inverted (see `privacy::support`, which refuses under REMOTE_AGENT_SHOW_WINDOWS).
    exclude_from_capture(&window);
    configure_privacy(&window);

    let display = info.index;
    let ipc =
        move |req: wry::http::Request<String>| match serde_json::from_str::<PrivacyIpc>(req.body())
        {
            Ok(msg) => super::post(AppEvent::Privacy(msg)),
            Err(e) => tracing::debug!("privacy IPC parse error: {e}"),
        };
    let webview = WebViewBuilder::new()
        .with_html(HTML.replace("__DISPLAY__", &display.to_string()))
        .with_background_color((10, 15, 21, 255))
        .with_ipc_handler(ipc)
        .build(&window)
        .context("creating privacy webview")?;
    window.set_outer_position(position);
    Ok(Shield {
        window,
        webview,
        ready: false,
        pending: Vec::new(),
    })
}

fn exclude_from_capture(window: &Window) {
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowExtMacOS;
        crate::platform::macos::exclude_ns_window_from_capture(window.ns_window());
    }
    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::WindowExtWindows;
        crate::platform::windows::exclude_hwnd_from_capture(window.hwnd());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = window;
    }
}

/// Opaque, shielding-level, all Spaces, takes the pointer and the keyboard.
fn configure_privacy(window: &Window) {
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowExtMacOS;
        crate::platform::macos::configure_privacy_ns_window(window.ns_window());
    }
    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::WindowExtWindows;
        crate::platform::windows::configure_privacy_hwnd(window.hwnd());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = window;
    }
}
