//! Transparent, click-through overlay windows that show the operator's screen annotations
//! (strokes + laser pointer) on the device. One overlay per display, created lazily on the
//! first annotation for that display and dropped when the session ends. Excluded from the
//! screen capture: the operator renders their own strokes locally, so the stream must not
//! carry a second, lagging copy.

use super::AppEvent;
use crate::annotate::{sanitize_color, AnnotateEvent};
use anyhow::{Context, Result};
use protocol::common::DisplayInfo;
use std::collections::HashMap;
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

const HTML: &str = include_str!("assets/annotate.html");

/// JS → Rust messages from an overlay page.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayIpc {
    Ready,
}

struct Overlay {
    window: Window,
    webview: WebView,
    ready: bool,
    pending: Vec<String>,
}

impl Overlay {
    fn eval(&mut self, js: String) {
        if self.ready {
            let _ = self.webview.evaluate_script(&js);
        } else {
            self.pending.push(js);
        }
    }

    fn on_ready(&mut self) {
        self.ready = true;
        for js in std::mem::take(&mut self.pending) {
            let _ = self.webview.evaluate_script(&js);
        }
    }
}

/// All overlays of the current session, keyed by display index.
#[derive(Default)]
pub(super) struct OverlayManager {
    overlays: HashMap<u32, Overlay>,
}

impl OverlayManager {
    /// Whether any overlay window exists (drives the "Annotations active" hint).
    pub(super) fn is_active(&self) -> bool {
        !self.overlays.is_empty()
    }

    pub(super) fn owns(&self, id: WindowId) -> bool {
        self.overlays.values().any(|o| o.window.id() == id)
    }

    /// An overlay page reported ready. The IPC carries no window identity, so every overlay
    /// that is still waiting is marked ready (they all load the same tiny page).
    pub(super) fn mark_ready(&mut self) {
        for o in self.overlays.values_mut().filter(|o| !o.ready) {
            o.on_ready();
        }
    }

    /// Apply one annotation instruction, creating the display's overlay when needed.
    /// Returns `true` when the set of overlays changed.
    pub(super) fn apply(
        &mut self,
        target: &EventLoopWindowTarget<AppEvent>,
        ev: AnnotateEvent,
    ) -> bool {
        match ev {
            AnnotateEvent::Stroke {
                id,
                display,
                color,
                width,
                points,
            } => {
                let created = self.ensure(target, display);
                if let Some(o) = self.overlays.get_mut(&display) {
                    let payload = serde_json::json!({
                        "id": id,
                        "color": sanitize_color(&color),
                        "width": width.clamp(1.0, 64.0),
                        "points": points,
                    });
                    o.eval(format!("window.__ann&&window.__ann.stroke({payload});"));
                }
                created
            }
            AnnotateEvent::End { id } => {
                for o in self.overlays.values_mut() {
                    o.eval(format!("window.__ann&&window.__ann.end({id});"));
                }
                false
            }
            AnnotateEvent::Pointer {
                display,
                point,
                color,
            } => {
                // Hide the pointer everywhere else, show it on the addressed display.
                let created = if point.is_some() {
                    self.ensure(target, display)
                } else {
                    false
                };
                for (idx, o) in self.overlays.iter_mut() {
                    let payload = if *idx == display {
                        serde_json::json!({ "point": point, "color": sanitize_color(&color) })
                    } else {
                        serde_json::json!({ "point": null })
                    };
                    o.eval(format!("window.__ann&&window.__ann.pointer({payload});"));
                }
                created
            }
            AnnotateEvent::Clear => {
                for o in self.overlays.values_mut() {
                    o.eval("window.__ann&&window.__ann.clear();".into());
                }
                false
            }
        }
    }

    /// Drop every overlay (session ended / annotations disabled / displays changed).
    pub(super) fn clear_all(&mut self) -> bool {
        let had = !self.overlays.is_empty();
        self.overlays.clear();
        had
    }

    fn ensure(&mut self, target: &EventLoopWindowTarget<AppEvent>, idx: u32) -> bool {
        if self.overlays.contains_key(&idx) {
            return false;
        }
        let displays = crate::capture::list_displays().unwrap_or_default();
        let Some(info) = displays.iter().find(|d| d.index == idx) else {
            tracing::debug!(
                display_index = idx,
                "annotation for unknown display ignored"
            );
            return false;
        };
        match create_overlay(target, info) {
            Ok(o) => {
                self.overlays.insert(idx, o);
                tracing::info!(display_index = idx, "annotation overlay created");
                true
            }
            Err(e) => {
                tracing::warn!(display_index = idx, "annotation overlay unavailable: {e:#}");
                false
            }
        }
    }
}

fn create_overlay(target: &EventLoopWindowTarget<AppEvent>, info: &DisplayInfo) -> Result<Overlay> {
    let scale = if info.scale > 0.0 {
        info.scale as f64
    } else {
        1.0
    };
    let size = LogicalSize::new(info.width as f64 / scale, info.height as f64 / scale);
    let position = LogicalPosition::new(info.x as f64, info.y as f64);
    let window = WindowBuilder::new()
        .with_title("annotations")
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top(true)
        .with_resizable(false)
        .with_focused(false)
        .with_visible(false)
        .with_inner_size(size)
        .with_position(position)
        .build(target)
        .context("creating annotation overlay window")?;
    window.set_visible_on_all_workspaces(true);
    // Click-through: every pointer event goes to whatever is underneath.
    if let Err(e) = window.set_ignore_cursor_events(true) {
        tracing::debug!("overlay ignore-cursor-events: {e}");
    }
    super::controller::exclude_from_capture(&window);
    configure_overlay(&window);

    let ipc = |req: wry::http::Request<String>| match serde_json::from_str::<OverlayIpc>(req.body())
    {
        Ok(OverlayIpc::Ready) => super::post(AppEvent::OverlayReady),
        Err(e) => tracing::debug!("overlay IPC parse error: {e}"),
    };
    let webview = WebViewBuilder::new()
        .with_html(HTML)
        .with_transparent(true)
        .with_background_color((0, 0, 0, 0))
        .with_ipc_handler(ipc)
        .build(&window)
        .context("creating annotation overlay webview")?;
    // Position again after the webview exists (some platforms re-layout on child insertion).
    window.set_outer_position(position);
    window.set_visible(true);
    Ok(Overlay {
        window,
        webview,
        ready: false,
        pending: Vec::new(),
    })
}

/// Platform tweaks so the overlay sits above full-screen apps on every Space (macOS) and is
/// a transparent, non-activating tool window (Windows).
fn configure_overlay(window: &Window) {
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowExtMacOS;
        crate::platform::macos::configure_overlay_ns_window(window.ns_window());
    }
    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::WindowExtWindows;
        crate::platform::windows::configure_overlay_hwnd(window.hwnd());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = window;
    }
}
