//! The main-thread event loop that owns the tao window, the wry webview and the tray icon.

use super::bar::{BarIpc, SessionBar};
use super::{
    branding_json, dispatch_disconnect, dispatch_send, key_fingerprint, set_proxy, AppEvent,
    AppOptions,
};
use crate::baked;
use crate::branding;
use anyhow::{Context, Result};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use protocol::channel::ChatParty;
use protocol::common::DeviceMode;
use protocol::config::LocalOverrides;
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
const ID_STATUS: &str = "status";

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
    /// Last policy JSON pushed from the hub (replayed to the page on ready).
    policy: String,
    opts: AppOptions,
    /// Console this agent is enrolled with (set by the hub; the trailer is only a fallback).
    console_url: String,
    /// Last permission state pushed to the page (`(screen, accessibility)`).
    permissions: Option<(bool, bool)>,
    /// Branded session bar (created on the first session).
    bar: Option<SessionBar>,
    location: crate::platform::ExeLocation,
    end_item: MenuItem,
    chat_item: MenuItem,
    status_item: MenuItem,
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
            serde_json::json!(self.console_url),
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
        if !self.policy.is_empty() {
            js.push_str(&format!("window.__app.setPolicy({});", self.policy));
        }
        js.push_str(&format!(
            "window.__app.setLocation({});",
            serde_json::json!({
                "movable": self.location.movable(),
                "translocated": self.location.translocated,
                "path": self.location.bundle.as_ref().map(|p| p.display().to_string()),
            })
        ));
        if let Some((screen, accessibility)) = self.permissions {
            js.push_str(&format!(
                "window.__app.setPermissions({});",
                serde_json::json!({ "supported": crate::platform::permissions_supported(), "screen": screen, "accessibility": accessibility })
            ));
        }
        // Developer aid for screenshots: open a specific screen at start.
        if let Ok(screen) = std::env::var("REMOTE_AGENT_START_SCREEN") {
            js.push_str(&format!(
                "window.__app.show({});",
                serde_json::json!(screen)
            ));
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
                self.chat_item.set_enabled(true);
                self.refresh_tooltip();
                self.show_bar(target, &operator);
                self.eval(format!(
                    "window.__app.startSession({});",
                    serde_json::json!(operator)
                ));
            }
            AppEvent::SessionEnded => {
                self.session_active = false;
                self.end_item.set_enabled(false);
                self.chat_item.set_enabled(false);
                self.refresh_tooltip();
                if let Some(bar) = self.bar.as_mut() {
                    bar.hide();
                }
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
                self.update_status_item();
                self.eval(format!(
                    "window.__app.setConsole({},{});",
                    serde_json::json!(self.console_url),
                    connected
                ));
            }
            AppEvent::ConsoleInfo { url } => {
                self.console_url = url;
                self.update_status_item();
                self.eval(format!(
                    "window.__app.setConsole({},{});",
                    serde_json::json!(self.console_url),
                    self.console_connected
                ));
            }
            AppEvent::Branding(json) => {
                self.apply_branding(&json);
            }
            AppEvent::Permissions {
                supported,
                screen,
                accessibility,
            } => {
                if self.permissions != Some((screen, accessibility)) {
                    self.permissions = Some((screen, accessibility));
                    self.eval(format!(
                        "window.__app.setPermissions({});",
                        serde_json::json!({ "supported": supported, "screen": screen, "accessibility": accessibility })
                    ));
                    self.refresh_tooltip();
                }
            }
            AppEvent::PermissionNeeded => {
                self.show(target, false);
                let _ = self
                    .webview
                    .evaluate_script("window.__app&&window.__app.show('home');");
            }
            AppEvent::Bar(msg) => self.on_bar(target, msg),
            AppEvent::MoveResult { ok, message } => {
                self.eval(format!(
                    "window.__app.moveResult({},{});",
                    ok,
                    serde_json::json!(message)
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
            AppEvent::Policy(json) => {
                self.policy = json.clone();
                self.eval(format!("window.__app.setPolicy({json});"));
            }
            AppEvent::Quit => { /* handled by caller (control flow) */ }
            AppEvent::__Ready => self.on_ready(),
        }
    }

    /// Show the branded session bar (creating it on first use).
    fn show_bar(
        &mut self,
        target: &tao::event_loop::EventLoopWindowTarget<AppEvent>,
        operator: &str,
    ) {
        if self.bar.is_none() {
            match SessionBar::new(target, super::state_dir()) {
                Ok(b) => self.bar = Some(b),
                Err(e) => {
                    tracing::warn!("session bar unavailable: {e:#}");
                    return;
                }
            }
        }
        if let Some(bar) = self.bar.as_mut() {
            bar.show(operator);
        }
    }

    fn on_bar(&mut self, target: &tao::event_loop::EventLoopWindowTarget<AppEvent>, msg: BarIpc) {
        match msg {
            BarIpc::Ready => {
                if let Some(bar) = self.bar.as_mut() {
                    bar.on_ready();
                }
            }
            BarIpc::Open => self.show(target, false),
            BarIpc::End => dispatch_disconnect(),
            BarIpc::Collapse => {
                if let Some(bar) = self.bar.as_mut() {
                    bar.set_collapsed(true);
                }
            }
            BarIpc::Expand => {
                if let Some(bar) = self.bar.as_mut() {
                    bar.set_collapsed(false);
                }
            }
            BarIpc::Drag => {
                if let Some(bar) = self.bar.as_ref() {
                    bar.drag();
                }
            }
        }
    }

    /// Tray tooltip: product, session state, and a warning while Screen Recording is missing.
    fn refresh_tooltip(&self) {
        let product = branding::product_name();
        let mut text = if self.session_active {
            format!("{product} — Session active: {} is connected", self.operator)
        } else {
            format!("{product} — No active session")
        };
        if matches!(self.permissions, Some((false, _))) {
            text = format!("{product} — Screen Recording permission required");
        }
        let _ = self.tray.set_tooltip(Some(text));
    }

    /// Status line of the tray menu: connected / disconnected and to which console.
    fn update_status_item(&self) {
        let host = console_host(&self.console_url);
        let text = if self.console_connected {
            format!("Connected to {host}")
        } else if host.is_empty() {
            "Not connected".to_string()
        } else {
            format!("Disconnected from {host}")
        };
        self.status_item.set_text(text);
    }

    /// Re-brand everything the window owns: page, title, tray icon + tooltip, dock/window icon.
    fn apply_branding(&mut self, page_json: &str) {
        self.eval(format!("window.__app.setBranding({page_json});"));
        let product = branding::product_name();
        self.window.set_title(&product);
        self.refresh_tooltip();
        if let Some(bar) = self.bar.as_mut() {
            bar.rebrand();
        }
        apply_tray_icon(&self.tray);
        apply_app_icons(&self.window);
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

    let product = branding::product_name();
    let window = WindowBuilder::new()
        .with_title(&product)
        .with_inner_size(tao::dpi::LogicalSize::new(720.0, 520.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(560.0, 420.0))
        .with_visible(opts.show_on_start)
        .build(&event_loop)
        .context("creating app window")?;
    exclude_from_capture(&window);
    apply_app_icons(&window);
    #[cfg(target_os = "macos")]
    crate::platform::macos::apply_debug_appearance();

    let ipc = move |req: wry::http::Request<String>| {
        let body = req.body();
        match serde_json::from_str::<IpcIn>(body) {
            Ok(IpcIn::Ready) => super::post(AppEvent::__Ready),
            Ok(IpcIn::Send { text }) => dispatch_send(text),
            Ok(IpcIn::Disconnect) => dispatch_disconnect(),
            Ok(IpcIn::OpenScreen { .. }) => {}
            Ok(IpcIn::SetOverrides { overrides }) => {
                super::dispatch_overrides(overrides.into_local());
            }
            Ok(IpcIn::Install) => spawn_install(),
            Ok(IpcIn::RequestPermission { which }) => {
                std::thread::spawn(move || {
                    match which.as_str() {
                        "screen" => {
                            crate::platform::request_screen_capture();
                        }
                        "accessibility" => {
                            crate::platform::request_accessibility();
                        }
                        _ => {}
                    }
                    post_permissions();
                });
            }
            Ok(IpcIn::OpenSettings { which }) => {
                let pane = if which == "accessibility" {
                    crate::platform::PrivacyPane::Accessibility
                } else {
                    crate::platform::PrivacyPane::ScreenCapture
                };
                crate::platform::open_privacy_settings(pane);
            }
            Ok(IpcIn::MoveToApplications) => {
                std::thread::spawn(|| {
                    let (ok, message) = match crate::platform::move_to_applications() {
                        Ok(m) => (true, m),
                        Err(e) => (false, format!("Move failed: {e:#}")),
                    };
                    super::post(AppEvent::MoveResult { ok, message });
                });
            }
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
    let status_item = MenuItem::with_id(ID_STATUS, "Not connected", false, None);
    let open_item = MenuItem::with_id(ID_OPEN, "Open", true, None);
    let chat_item = MenuItem::with_id(ID_CHAT, "Open chat", false, None);
    let end_item = MenuItem::with_id(ID_END, "End session", false, None);
    let quit_item = MenuItem::with_id(ID_QUIT, "Quit", true, None);
    menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &open_item,
        &chat_item,
        &end_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .ok();
    let (tray_icon, tray_template) = tray_icon_for_platform();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("{product} — no active session"))
        .with_icon(tray_icon)
        .with_icon_as_template(tray_template)
        .build()
        .context("creating tray icon")?;

    // Live re-branding: whenever the console branding changes, refresh the window.
    branding::on_change(|_| super::refresh_branding());

    // OS permission state: re-checked every 2 s (cheap TCC queries); the controller only
    // re-renders on change.
    if crate::platform::permissions_supported() {
        std::thread::Builder::new()
            .name("permissions".into())
            .spawn(|| loop {
                post_permissions();
                std::thread::sleep(std::time::Duration::from_secs(2));
            })
            .ok();
    }

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
        policy: String::new(),
        opts,
        console_url: baked::get()
            .map(|b| b.config.server_url.clone())
            .unwrap_or_default(),
        permissions: None,
        bar: None,
        location: crate::platform::exe_location(),
        end_item,
        chat_item,
        status_item,
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
                window_id,
                ..
            } => {
                let is_bar = controller.bar.as_ref().map(|b| b.window_id()) == Some(window_id);
                if is_bar {
                    // The bar has no close control; ignore.
                } else {
                    // Close hides the window; the session keeps running.
                    controller.hide(target);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Moved(_),
                window_id,
                ..
            } => {
                if let Some(bar) = controller.bar.as_mut() {
                    if bar.window_id() == window_id && bar.is_visible() {
                        bar.moved();
                    }
                }
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
    SetOverrides {
        overrides: UiOverrides,
    },
    Install,
    RequestPermission {
        which: String,
    },
    OpenSettings {
        which: String,
    },
    MoveToApplications,
}

/// The Settings toggles as the page reports them: each value is the *effective local choice*.
/// `require_approval = true` forces help-me; the `allow_*` values are the local allow decision.
/// Converted to a restrict-only [`LocalOverrides`] (a value that matches the console default
/// becomes `None`, since the local user can only tighten, never loosen).
#[derive(serde::Deserialize)]
struct UiOverrides {
    require_approval: bool,
    allow_input: bool,
    allow_audio: bool,
    allow_clipboard: bool,
    allow_file_transfer: bool,
}

impl UiOverrides {
    fn into_local(self) -> LocalOverrides {
        let restrict = |allow: bool| if allow { None } else { Some(false) };
        LocalOverrides {
            mode: if self.require_approval {
                Some(DeviceMode::HelpMe)
            } else {
                None
            },
            allow_input: restrict(self.allow_input),
            allow_audio: restrict(self.allow_audio),
            allow_clipboard: restrict(self.allow_clipboard),
            allow_file_transfer: restrict(self.allow_file_transfer),
        }
    }
}

/// Query the OS permissions and post them to the UI thread.
fn post_permissions() {
    super::post(AppEvent::Permissions {
        supported: crate::platform::permissions_supported(),
        screen: crate::platform::screen_capture_allowed(),
        accessibility: crate::platform::accessibility_allowed(),
    });
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

/// Tray / menu-bar icon: a template (alpha-only) image on macOS so the system tints it for
/// light and dark menu bars; the coloured logo or a theme-aware glyph elsewhere.
fn tray_icon_for_platform() -> (tray_icon::Icon, bool) {
    #[cfg(target_os = "macos")]
    {
        // 36 px → rendered at 18 pt, crisp on Retina.
        let img = branding::template_icon(36);
        (
            tray_icon::Icon::from_rgba(img.data, img.width, img.height).expect("valid tray icon"),
            true,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let dark = crate::platform::dark_theme();
        let img = branding::tray_icon_colored(32, dark);
        (
            tray_icon::Icon::from_rgba(img.data, img.width, img.height).expect("valid tray icon"),
            false,
        )
    }
}

fn apply_tray_icon(tray: &tray_icon::TrayIcon) {
    let (icon, template) = tray_icon_for_platform();
    if let Err(e) = tray.set_icon_with_as_template(Some(icon), template) {
        tracing::debug!("tray icon update failed: {e}");
    }
}

/// Dock icon (macOS) / window + taskbar icon (Windows) from the branding logo or the default mark.
fn apply_app_icons(window: &tao::window::Window) {
    let icon = branding::dock_icon(256);
    #[cfg(target_os = "macos")]
    {
        let png = branding::encode_png(&branding::dock_icon(512));
        crate::platform::set_dock_icon(&png);
        let _ = (window, icon);
    }
    #[cfg(not(target_os = "macos"))]
    {
        match tao::window::Icon::from_rgba(icon.data, icon.width, icon.height) {
            Ok(i) => window.set_window_icon(Some(i)),
            Err(e) => tracing::debug!("window icon: {e}"),
        }
    }
}

/// `https://console.example.com:8443/` → `console.example.com:8443`.
fn console_host(url: &str) -> String {
    let s = url.trim();
    let s = s.split("://").nth(1).unwrap_or(s);
    s.split('/').next().unwrap_or("").to_string()
}

/// Exclude our window from the screen capture the operator sees.
pub(super) fn exclude_from_capture(window: &tao::window::Window) {
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
