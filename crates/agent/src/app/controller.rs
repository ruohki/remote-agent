//! The main-thread event loop that owns the tao window, the wry webview and the tray icon.

use super::{
    branding_json, dispatch_disconnect, dispatch_send, key_fingerprint, set_proxy, AppEvent,
    AppOptions,
};
use crate::baked;
use anyhow::{Context, Result};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use protocol::channel::ChatParty;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use tray_icon::{TrayIconBuilder, TrayIconEvent};
use wry::WebViewBuilder;

const INDEX: &str = include_str!("assets/index.html");
const CSS: &str = include_str!("assets/app.css");
const JS: &str = include_str!("assets/app.js");

/// Compose the single-file HTML (CSS + JS inlined) so no custom protocol / external origin is
/// involved — identical behaviour on WKWebView and WebView2.
fn page_html() -> String {
    INDEX
        .replace(
            "<link rel=\"stylesheet\" href=\"app://assets/app.css\" />",
            &format!("<style>{CSS}</style>"),
        )
        .replace(
            "<script src=\"app://assets/app.js\"></script>",
            &format!("<script>{JS}</script>"),
        )
}

/// Menu item ids.
const ID_OPEN: &str = "open";
const ID_CHAT: &str = "chat";
const ID_END: &str = "end";
const ID_QUIT: &str = "quit";

struct Controller {
    webview: wry::WebView,
    window: tao::window::Window,
    ready: bool,
    /// Buffered JS evaluated once the page reports ready.
    pending: Vec<String>,
    // last-known state, replayed to the page on (re)ready
    session_active: bool,
    operator: String,
    transcript: Vec<(ChatParty, String, u64)>,
    console_connected: bool,
    device: (String, String),
    opts: AppOptions,
    end_item: MenuItem,
    tray: tray_icon::TrayIcon,
}

impl Controller {
    fn eval(&mut self, js: String) {
        if self.ready {
            let _ = self.webview.evaluate_script(&js);
        } else {
            self.pending.push(js);
        }
    }

    fn on_ready(&mut self) {
        if self.ready {
            return;
        }
        self.ready = true;
        let mut js = String::new();
        js.push_str(&format!("window.__app.setBranding({});", branding_json()));
        js.push_str(&format!(
            "window.__app.setAbout({},{});",
            serde_json::json!(crate::AGENT_VERSION),
            serde_json::json!(baked::get()
                .map(|b| key_fingerprint(&b.public_key))
                .unwrap_or_else(|| "unsigned build".to_string()))
        ));
        js.push_str(&format!(
            "window.__app.setInstallable({});",
            self.opts.installable
        ));
        js.push_str(&format!(
            "window.__app.setConsole({},{});",
            serde_json::json!(baked::get()
                .map(|b| b.config.server_url.clone())
                .unwrap_or_default()),
            self.console_connected
        ));
        js.push_str(&format!(
            "window.__app.setDevice({},{});",
            serde_json::json!(self.device.0),
            serde_json::json!(self.device.1)
        ));
        if self.session_active {
            js.push_str(&format!(
                "window.__app.startSession({});",
                serde_json::json!(self.operator)
            ));
            for (from, text, ts) in &self.transcript {
                js.push_str(&push_js(*from, text, *ts));
            }
        } else {
            js.push_str("window.__app.endSession();");
        }
        // Flush anything buffered before ready.
        let pending = std::mem::take(&mut self.pending);
        let _ = self.webview.evaluate_script(&js);
        for p in pending {
            let _ = self.webview.evaluate_script(&p);
        }
    }

    fn handle(&mut self, ev: AppEvent, target: &tao::event_loop::EventLoopWindowTarget<AppEvent>) {
        match ev {
            AppEvent::SessionStarted { operator } => {
                self.session_active = true;
                self.operator = operator.clone();
                self.transcript.clear();
                self.end_item.set_enabled(true);
                let _ = self.tray.set_tooltip(Some(format!(
                    "{} — {} connected",
                    baked::product_name(),
                    operator
                )));
                self.eval(format!(
                    "window.__app.startSession({});",
                    serde_json::json!(operator)
                ));
            }
            AppEvent::SessionEnded => {
                self.session_active = false;
                self.end_item.set_enabled(false);
                let _ = self.tray.set_tooltip(Some(format!(
                    "{} — no active session",
                    baked::product_name()
                )));
                self.eval("window.__app.endSession();".into());
            }
            AppEvent::Chat { from, text, ts_ms } => {
                self.transcript.push((from, text.clone(), ts_ms));
                self.eval(push_js(from, &text, ts_ms));
                if matches!(from, ChatParty::Operator) {
                    self.show(target, true);
                }
            }
            AppEvent::Show { chat } => self.show(target, chat),
            AppEvent::Hide => self.hide(target),
            AppEvent::ConsoleStatus { connected } => {
                self.console_connected = connected;
                self.eval(format!(
                    "window.__app.setConsole({},{});",
                    serde_json::json!(baked::get()
                        .map(|b| b.config.server_url.clone())
                        .unwrap_or_default()),
                    connected
                ));
            }
            AppEvent::DeviceInfo { name, id } => {
                self.device = (name.clone(), id.clone());
                self.eval(format!(
                    "window.__app.setDevice({},{});",
                    serde_json::json!(name),
                    serde_json::json!(id)
                ));
            }
            AppEvent::InstallResult { ok, message } => {
                if ok {
                    self.opts.installable = false;
                    self.eval("window.__app.setInstallable(false);".into());
                }
                self.eval(format!(
                    "window.__app.installResult({},{});",
                    ok,
                    serde_json::json!(message)
                ));
            }
            AppEvent::Quit => { /* handled by caller (control flow) */ }
            AppEvent::__Ready => self.on_ready(),
        }
    }

    fn show(&self, target: &tao::event_loop::EventLoopWindowTarget<AppEvent>, chat: bool) {
        self.window.set_visible(true);
        self.window.set_focus();
        set_dock_visible(target, true);
        #[cfg(target_os = "windows")]
        {
            use tao::platform::windows::WindowExtWindows;
            let _ = self.window.set_skip_taskbar(false);
        }
        if chat {
            let _ = self
                .webview
                .evaluate_script("window.__app&&window.__app.show('chat');");
        }
    }

    fn hide(&self, target: &tao::event_loop::EventLoopWindowTarget<AppEvent>) {
        self.window.set_visible(false);
        set_dock_visible(target, false);
        #[cfg(target_os = "windows")]
        {
            use tao::platform::windows::WindowExtWindows;
            let _ = self.window.set_skip_taskbar(true);
        }
    }
}

fn push_js(from: ChatParty, text: &str, ts_ms: u64) -> String {
    let from = match from {
        ChatParty::Operator => "operator",
        ChatParty::Device => "device",
    };
    format!(
        "window.__app.push({});",
        serde_json::json!({ "from": from, "text": text, "ts_ms": ts_ms })
    )
}

/// Run the application: builds the window/webview/tray, spawns `work` on a worker thread and
/// pumps the event loop on the calling (main) thread. Returns the worker's exit code.
pub fn run(work: impl FnOnce() -> i32 + Send + 'static, opts: AppOptions) -> i32 {
    match build_and_run(work, opts) {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("app loop failed: {e:#}");
            // Fall back to running the worker without a UI so a headless environment still works.
            0
        }
    }
}

fn build_and_run(work: impl FnOnce() -> i32 + Send + 'static, opts: AppOptions) -> Result<i32> {
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    set_proxy(event_loop.create_proxy());
    crate::platform::mark_main_loop_running();

    let product = baked::product_name();
    let window = WindowBuilder::new()
        .with_title(&product)
        .with_inner_size(tao::dpi::LogicalSize::new(720.0, 520.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(560.0, 420.0))
        .with_visible(opts.show_on_start)
        .build(&event_loop)
        .context("creating app window")?;
    exclude_from_capture(&window);

    let ipc = move |req: wry::http::Request<String>| {
        let body = req.body();
        match serde_json::from_str::<IpcIn>(body) {
            Ok(IpcIn::Ready) => super::post(AppEvent::__Ready),
            Ok(IpcIn::Send { text }) => dispatch_send(text),
            Ok(IpcIn::Disconnect) => dispatch_disconnect(),
            Ok(IpcIn::OpenScreen { .. }) => {}
            Ok(IpcIn::Install) => spawn_install(),
            Err(e) => tracing::debug!("app IPC parse error: {e}"),
        }
    };

    let webview = WebViewBuilder::new()
        .with_html(page_html())
        .with_ipc_handler(ipc)
        .with_background_color((0, 0, 0, 0))
        .build(&window)
        .context("creating webview")?;

    // Tray icon + menu.
    let menu = Menu::new();
    let open_item = MenuItem::with_id(ID_OPEN, "Open", true, None);
    let chat_item = MenuItem::with_id(ID_CHAT, "Open chat", true, None);
    let end_item = MenuItem::with_id(ID_END, "End session", false, None);
    let quit_item = MenuItem::with_id(ID_QUIT, "Quit", true, None);
    menu.append_items(&[
        &open_item,
        &chat_item,
        &end_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .ok();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("{product} — no active session"))
        .with_icon(tray_rgba_icon())
        .with_icon_as_template(true)
        .build()
        .context("creating tray icon")?;

    if !opts.show_on_start {
        set_dock_visible(&event_loop, false);
    }

    // Route tray + menu events into the loop via the proxy.
    let menu_rx = MenuEvent::receiver();
    let tray_rx = TrayIconEvent::receiver();

    let mut controller = Controller {
        webview,
        window,
        ready: false,
        pending: Vec::new(),
        session_active: false,
        operator: String::new(),
        transcript: Vec::new(),
        console_connected: false,
        device: (String::new(), String::new()),
        opts,
        end_item,
        tray,
    };

    // Worker thread runs the agent; it exits the process when done.
    let worker_started = Arc::new(AtomicBool::new(false));
    let ws = Arc::clone(&worker_started);
    let mut work = Some(work);

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        // Start the worker once the loop is live.
        if !ws.swap(true, Ordering::SeqCst) {
            if let Some(work) = work.take() {
                std::thread::Builder::new()
                    .name("agent".into())
                    .spawn(move || {
                        let code = work();
                        tracing::info!("agent worker finished with code {code}");
                        std::process::exit(code);
                    })
                    .expect("spawn agent worker");
            }
        }

        // Drain tray / menu events.
        while let Ok(ev) = menu_rx.try_recv() {
            match ev.id.as_ref() {
                ID_OPEN => controller.show(target, false),
                ID_CHAT => controller.show(target, true),
                ID_END => dispatch_disconnect(),
                ID_QUIT => *control_flow = ControlFlow::Exit,
                _ => {}
            }
        }
        while let Ok(ev) = tray_rx.try_recv() {
            if let TrayIconEvent::DoubleClick { .. } = ev {
                controller.show(target, false);
            }
        }

        match event {
            Event::UserEvent(AppEvent::__Ready) => controller.on_ready(),
            Event::UserEvent(AppEvent::Quit) => *control_flow = ControlFlow::Exit,
            Event::UserEvent(ev) => controller.handle(ev, target),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // Close hides the window; the session keeps running.
                controller.hide(target);
            }
            _ => {}
        }
    });
}

/// JS → Rust IPC messages.
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum IpcIn {
    Ready,
    Send {
        text: String,
    },
    Disconnect,
    OpenScreen {
        #[allow(dead_code)]
        screen: String,
    },
    Install,
}

/// Elevate and install the service on a worker thread, reporting the result back to the UI.
fn spawn_install() {
    std::thread::spawn(|| {
        let (ok, message) = match crate::service::install_elevated() {
            Ok(msg) => (true, msg),
            Err(e) => (false, format!("Install failed: {e}")),
        };
        super::post(AppEvent::InstallResult { ok, message });
    });
}

/// A tiny generated tray icon (rounded square in the brand accent). 22×22 RGBA.
fn tray_rgba_icon() -> tray_icon::Icon {
    let (w, h) = (22u32, 22u32);
    let (r, g, b) = accent_rgb();
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            // rounded-corner mask
            let inset = 2.0;
            let fx = x as f32;
            let fy = y as f32;
            let inside = fx >= inset
                && fy >= inset
                && fx <= (w as f32 - inset - 1.0)
                && fy <= (h as f32 - inset - 1.0);
            if inside {
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = 255;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, w, h).expect("valid tray icon")
}

fn accent_rgb() -> (u8, u8, u8) {
    let accent = baked::get()
        .map(|b| b.branding().accent.clone())
        .filter(|s| s.len() == 7 && s.starts_with('#'))
        .unwrap_or_else(|| "#3b82f6".to_string());
    let parse = |s: &str| u8::from_str_radix(s, 16).unwrap_or(0);
    (
        parse(&accent[1..3]),
        parse(&accent[3..5]),
        parse(&accent[5..7]),
    )
}

/// Exclude our window from the screen capture the operator sees.
fn exclude_from_capture(window: &tao::window::Window) {
    if std::env::var_os("REMOTE_AGENT_SHOW_WINDOWS").is_some() {
        return;
    }
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

/// Show / hide the macOS dock icon (Regular vs Accessory). No-op elsewhere.
fn set_dock_visible(target: &tao::event_loop::EventLoopWindowTarget<AppEvent>, visible: bool) {
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
        target.set_activation_policy_at_runtime(if visible {
            ActivationPolicy::Regular
        } else {
            ActivationPolicy::Accessory
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (target, visible);
    }
}
