//! The branded session bar: a small always-on-top, borderless `tao` + `wry` window shown while
//! a support session is active (logo, product name, "<Name> is connected", Open / End session,
//! and a collapse control that shrinks it to a round pill). Draggable, position remembered,
//! excluded from the screen capture and covered by the injected-input guard like every other
//! agent window. Lives on the UI thread next to the main window (see `controller.rs`).

use super::AppEvent;
use crate::branding;
use anyhow::{Context, Result};
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder};
use wry::{WebView, WebViewBuilder};

const HTML: &str = include_str!("assets/session-bar.html");
const EXPANDED: (f64, f64) = (700.0, 46.0);
const COLLAPSED: (f64, f64) = (46.0, 46.0);
const POSITION_FILE: &str = "session-bar.json";

/// JS → Rust messages from the bar page.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BarIpc {
    Ready,
    Open,
    End,
    /// Emergency switch: pause / resume remote keyboard & mouse control.
    Pause,
    Resume,
    Collapse,
    Expand,
    Drag,
}

pub(super) struct SessionBar {
    window: Window,
    webview: WebView,
    ready: bool,
    pending: Vec<String>,
    collapsed: bool,
    /// Remote control paused by the device user.
    paused: bool,
    /// Top-left of the expanded bar in logical points (persisted).
    position: LogicalPosition<f64>,
    state_dir: Option<std::path::PathBuf>,
    operator: String,
    started_ms: u64,
}

impl SessionBar {
    pub(super) fn new(
        target: &EventLoopWindowTarget<AppEvent>,
        state_dir: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        let position =
            load_position(state_dir.as_deref()).unwrap_or_else(|| default_position(target));
        let window = WindowBuilder::new()
            .with_title(format!("{} — session", branding::product_name()))
            .with_decorations(false)
            .with_always_on_top(true)
            .with_resizable(false)
            .with_transparent(true)
            .with_visible(false)
            .with_focused(false)
            .with_inner_size(LogicalSize::new(EXPANDED.0, EXPANDED.1))
            .with_position(position)
            .build(target)
            .context("creating session bar window")?;
        window.set_visible_on_all_workspaces(true);
        super::controller::exclude_from_capture(&window);

        let ipc = |req: wry::http::Request<String>| match serde_json::from_str::<BarIpc>(req.body())
        {
            Ok(msg) => super::post(AppEvent::Bar(msg)),
            Err(e) => tracing::debug!("session bar IPC parse error: {e}"),
        };
        let webview = WebViewBuilder::new()
            .with_html(HTML)
            .with_transparent(true)
            .with_background_color((0, 0, 0, 0))
            .with_ipc_handler(ipc)
            .build(&window)
            .context("creating session bar webview")?;
        Ok(Self {
            window,
            webview,
            ready: false,
            pending: Vec::new(),
            collapsed: false,
            paused: false,
            position,
            state_dir,
            operator: String::new(),
            started_ms: 0,
        })
    }

    fn eval(&mut self, js: String) {
        if self.ready {
            let _ = self.webview.evaluate_script(&js);
        } else {
            self.pending.push(js);
        }
    }

    pub(super) fn on_ready(&mut self) {
        self.ready = true;
        self.push_state();
        for js in std::mem::take(&mut self.pending) {
            let _ = self.webview.evaluate_script(&js);
        }
    }

    fn push_state(&mut self) {
        let b = branding::current();
        let js = format!(
            "window.__bar&&window.__bar.set({});window.__bar&&window.__bar.collapsed({});",
            serde_json::json!({
                "product": branding::product_name(),
                "accent": branding::accent(),
                "logo": b.logo_png_base64,
                "operator": self.operator,
                "started_ms": self.started_ms,
                "paused": self.paused,
            }),
            self.collapsed
        );
        let _ = self.webview.evaluate_script(&js);
    }

    /// Reflect the pause state (amber stripe, "Remote control paused", Resume button).
    pub(super) fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.eval(format!("window.__bar&&window.__bar.paused({paused});"));
    }

    /// Show the bar for a new session (expanded).
    pub(super) fn show(&mut self, operator: &str) {
        self.operator = operator.to_string();
        self.started_ms = crate::chat::now_ms();
        self.paused = false;
        // Developer aid for screenshots of the collapsed pill.
        self.collapsed = std::env::var_os("REMOTE_AGENT_BAR_COLLAPSED").is_some();
        self.apply_size();
        self.window.set_outer_position(self.position);
        let js = format!(
            "window.__bar&&window.__bar.set({});window.__bar&&window.__bar.collapsed({});",
            serde_json::json!({ "operator": self.operator, "started_ms": self.started_ms, "paused": false }),
            self.collapsed
        );
        self.eval(js);
        self.window.set_visible(true);
    }

    pub(super) fn hide(&mut self) {
        self.window.set_visible(false);
    }

    pub(super) fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    /// Re-apply product/accent/logo after the branding changed.
    pub(super) fn rebrand(&mut self) {
        self.window
            .set_title(&format!("{} — session", branding::product_name()));
        if self.ready {
            self.push_state();
        }
    }

    pub(super) fn set_collapsed(&mut self, collapsed: bool) {
        if self.collapsed == collapsed {
            return;
        }
        self.collapsed = collapsed;
        self.apply_size();
        self.eval(format!(
            "window.__bar&&window.__bar.collapsed({collapsed});"
        ));
    }

    fn apply_size(&self) {
        let (w, h) = if self.collapsed { COLLAPSED } else { EXPANDED };
        self.window.set_inner_size(LogicalSize::new(w, h));
    }

    pub(super) fn drag(&self) {
        let _ = self.window.drag_window();
    }

    /// Remember where the user put the bar (called on `WindowEvent::Moved`).
    pub(super) fn moved(&mut self) {
        if let Ok(p) = self.window.outer_position() {
            let scale = self.window.scale_factor();
            self.position = p.to_logical(scale);
            if let Some(dir) = &self.state_dir {
                let _ = std::fs::write(
                    dir.join(POSITION_FILE),
                    serde_json::json!({ "x": self.position.x, "y": self.position.y }).to_string(),
                );
            }
        }
    }

    pub(super) fn window_id(&self) -> tao::window::WindowId {
        self.window.id()
    }
}

fn default_position(target: &EventLoopWindowTarget<AppEvent>) -> LogicalPosition<f64> {
    let (mut x, y) = (200.0, 12.0);
    if let Some(m) = target.primary_monitor() {
        let scale = m.scale_factor();
        let size = m.size().to_logical::<f64>(scale);
        let origin = m.position().to_logical::<f64>(scale);
        x = origin.x + (size.width - EXPANDED.0) / 2.0;
        return LogicalPosition::new(x, origin.y + y + 24.0);
    }
    LogicalPosition::new(x, y)
}

fn load_position(dir: Option<&std::path::Path>) -> Option<LogicalPosition<f64>> {
    let text = std::fs::read_to_string(dir?.join(POSITION_FILE)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let x = v.get("x")?.as_f64()?;
    let y = v.get("y")?.as_f64()?;
    if x.is_finite() && y.is_finite() && x > -10_000.0 && y > -10_000.0 {
        Some(LogicalPosition::new(x, y))
    } else {
        None
    }
}
