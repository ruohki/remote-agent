//! macOS specifics: AppKit main loop, approval dialog (`NSAlert`), session indicator panel,
//! console user lookup and TCC permission checks.

use crate::approval::{ApprovalOutcome, IndicatorHandle};
use anyhow::{anyhow, Context, Result};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSBackingStoreType, NSButton, NSPanel, NSScreen, NSStatusWindowLevel, NSTextField, NSWindow,
    NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

// ─── main loop ──────────────────────────────────────────────────────────────────────────

/// Pump `NSApplication` on the current (main) thread while `work` runs on a worker thread.
/// The process exits with the worker's code when it finishes.
pub fn run_app_loop(work: impl FnOnce() -> i32 + Send + 'static) -> i32 {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!("run_app_loop must be called on the main thread");
        return 1;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let spawned = std::thread::Builder::new()
        .name("agent".into())
        .spawn(move || {
            let code = work();
            tracing::info!("agent finished with code {code}");
            std::process::exit(code);
        });
    if let Err(e) = spawned {
        tracing::error!("spawning agent thread: {e}");
        return 1;
    }
    app.run();
    0
}

/// Run `f` on the main thread. Runs inline when already there; otherwise dispatches to the
/// main queue, which requires [`run_app_loop`] to be pumping events.
pub fn run_on_main<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    if MainThreadMarker::new().is_some() {
        return Ok(f());
    }
    if !super::main_loop_running() {
        return Err(anyhow!(
            "main thread event loop is not running (start with `remote-agent run`)"
        ));
    }
    let (tx, rx) = mpsc::channel();
    DispatchQueue::main().exec_async(move || {
        let _ = tx.send(f());
    });
    rx.recv().context("main thread task was dropped")
}

// ─── console user & permissions ─────────────────────────────────────────────────────────

/// (uid, name) of the user owning the console (`/dev/console`), if someone is logged in.
pub fn console_user() -> Option<(u32, String)> {
    let path = c"/dev/console";
    // SAFETY: stat with a valid NUL terminated path and a zeroed out-struct.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let uid = st.st_uid;
    // uid 0 means nobody is logged in at the console (login window).
    if uid == 0 {
        return None;
    }
    // SAFETY: getpwuid returns a pointer to static storage or null.
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr((*pw).pw_name) }
        .to_string_lossy()
        .into_owned();
    Some((uid, name))
}

pub fn screen_capture_allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

/// Prompts the user (adds the app to the Screen Recording list). Returns the new state.
pub fn request_screen_capture() -> bool {
    CGRequestScreenCaptureAccess()
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

pub fn accessibility_allowed() -> bool {
    // SAFETY: plain FFI call without arguments.
    unsafe { AXIsProcessTrusted() }
}

// ─── approval dialog ────────────────────────────────────────────────────────────────────

/// Modal `NSAlert` with Allow/Deny; auto-denies after `timeout`.
pub fn approval_dialog(operator: &str, timeout: Duration) -> Result<ApprovalOutcome> {
    let operator = operator.to_owned();
    let done = Arc::new(AtomicBool::new(false));
    let done_timer = Arc::clone(&done);
    let (result_tx, result_rx) = mpsc::channel();

    // Timeout: abort the modal loop from the main queue if nobody answered.
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        if !done_timer.load(Ordering::SeqCst) {
            DispatchQueue::main().exec_async(move || {
                if let Some(mtm) = MainThreadMarker::new() {
                    NSApplication::sharedApplication(mtm).abortModal();
                }
            });
        }
    });

    let done_main = Arc::clone(&done);
    run_on_main(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            let _ = result_tx.send(Err(anyhow!("not on main thread")));
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&format!(
            "{operator} wants to control this computer"
        )));
        alert.setInformativeText(&NSString::from_str(&format!(
            "An operator ({operator}) is asking to view and control your screen through the remote support console.\n\nAllow the session? It ends automatically if you do not answer within {} seconds.",
            timeout.as_secs()
        )));
        alert.setAlertStyle(NSAlertStyle::Warning);
        let _allow = alert.addButtonWithTitle(&NSString::from_str("Allow"));
        let _deny = alert.addButtonWithTitle(&NSString::from_str("Deny"));
        alert.window().setLevel(NSStatusWindowLevel);
        let response = alert.runModal();
        done_main.store(true, Ordering::SeqCst);
        let outcome = if response == NSAlertFirstButtonReturn {
            ApprovalOutcome::Approved
        } else if response == NSAlertFirstButtonReturn + 1 {
            ApprovalOutcome::Denied
        } else {
            ApprovalOutcome::TimedOut
        };
        let _ = result_tx.send(Ok(outcome));
    })?;
    result_rx.recv().context("approval dialog task dropped")?
}

// ─── session indicator ──────────────────────────────────────────────────────────────────

struct TargetIvars {
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the type does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RemoteAgentIndicatorTarget"]
    #[ivars = TargetIvars]
    struct IndicatorTarget;

    impl IndicatorTarget {
        #[unsafe(method(disconnect:))]
        fn disconnect(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_disconnect)();
        }
    }

    unsafe impl NSObjectProtocol for IndicatorTarget {}
);

impl IndicatorTarget {
    fn new(mtm: MainThreadMarker, on_disconnect: Arc<dyn Fn() + Send + Sync>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { on_disconnect });
        // SAFETY: plain NSObject init on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Handle whose drop closes the indicator panel on the main thread.
///
/// The panel and its button target are AppKit objects (`!Send`), so we keep them as raw
/// `Retained` pointers (stored as addresses) and only ever touch them on the main thread —
/// where they were created and where drop dispatches their release.
struct PanelHandle {
    panel_addr: usize,
    target_addr: usize,
}

// SAFETY: the addresses are only converted back to `Retained` on the main queue.
unsafe impl Send for PanelHandle {}
unsafe impl Sync for PanelHandle {}

impl IndicatorHandle for PanelHandle {}

impl Drop for PanelHandle {
    fn drop(&mut self) {
        let panel_addr = self.panel_addr;
        let target_addr = self.target_addr;
        DispatchQueue::main().exec_async(move || {
            // SAFETY: reclaims the +1 references created in `show_indicator`; runs on the main
            // thread, and each address is turned back into a `Retained` exactly once.
            unsafe {
                if let Some(panel) = Retained::from_raw(panel_addr as *mut NSPanel) {
                    let window: &NSWindow = &panel;
                    window.close();
                }
                let _ = Retained::from_raw(target_addr as *mut IndicatorTarget);
            }
        });
    }
}

/// Create the banner. Returns a handle whose drop closes the panel.
pub fn show_indicator(
    operator: &str,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn IndicatorHandle>> {
    let operator = operator.to_owned();
    let (tx, rx) = mpsc::channel();
    run_on_main(move || {
        let result = (|| -> Result<Banner> {
            let mtm = MainThreadMarker::new().context("not on main thread")?;
            let width = 360.0;
            let height = 44.0;
            let screen = NSScreen::mainScreen(mtm).context("no main screen")?;
            let visible = screen.visibleFrame();
            let origin = NSPoint::new(
                visible.origin.x + visible.size.width - width - 12.0,
                visible.origin.y + visible.size.height - height - 12.0,
            );
            let frame = NSRect::new(origin, NSSize::new(width, height));
            let style = NSWindowStyleMask::Titled
                | NSWindowStyleMask::NonactivatingPanel
                | NSWindowStyleMask::UtilityWindow;
            let panel: Retained<NSPanel> = NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            );
            panel.setTitle(&NSString::from_str("Remote session"));
            panel.setLevel(NSStatusWindowLevel);
            panel.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary,
            );
            panel.setHidesOnDeactivate(false);
            // SAFETY: the panel is retained by us and released explicitly on close.
            unsafe { panel.setReleasedWhenClosed(false) };
            // Keep the banner out of the screen capture the operator sees.
            if std::env::var_os("REMOTE_AGENT_SHOW_WINDOWS").is_none() {
                panel.setSharingType(NSWindowSharingType::None);
            }

            let target = IndicatorTarget::new(mtm, on_disconnect);
            let label = NSTextField::labelWithString(
                &NSString::from_str(&format!("{operator} is controlling this computer")),
                mtm,
            );
            label.setFrame(NSRect::new(
                NSPoint::new(12.0, 12.0),
                NSSize::new(width - 120.0, 20.0),
            ));
            // SAFETY: target is retained for the panel's lifetime, selector exists on the class.
            let button = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str("Disconnect"),
                    Some(&target),
                    Some(sel!(disconnect:)),
                    mtm,
                )
            };
            button.setFrame(NSRect::new(
                NSPoint::new(width - 104.0, 8.0),
                NSSize::new(96.0, 28.0),
            ));
            if let Some(content) = panel.contentView() {
                content.addSubview(&label);
                content.addSubview(&button);
            }
            panel.orderFrontRegardless();
            Ok(Banner { panel, target })
        })();
        // Convert to raw addresses so they can cross the channel and later the main queue
        // (Retained is !Send). Ownership passes to PanelHandle.
        let sendable = result.map(|b| {
            let panel_addr = Retained::into_raw(b.panel) as usize;
            let target_addr = Retained::into_raw(b.target) as usize;
            (panel_addr, target_addr)
        });
        let _ = tx.send(sendable);
    })?;
    let (panel_addr, target_addr) = rx.recv().context("indicator task dropped")??;
    Ok(Box::new(PanelHandle {
        panel_addr,
        target_addr,
    }))
}

struct Banner {
    panel: Retained<NSPanel>,
    target: Retained<IndicatorTarget>,
}

// ─── chat window: a WKWebView hosting the messaging UI (see chat_assets.rs) ───────────────
//
// The window is an `NSPanel` whose content view is a `WKWebView`. IPC: JS posts to a
// `WKScriptMessageHandler` named "agent"; Rust drives the page with `evaluateJavaScript`.
// Closing the window HIDES it (the session keeps running); the local user re-opens it from the
// menu-bar item or the banner. Remote-injected events are dropped app-wide by `install_input_guard`.

use crate::chat::{ChatHandle, ChatLine};
use crate::platform::chat_assets::chat_html;
use block2::RcBlock;
use objc2_app_kit::{NSView, NSWindowDelegate};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView,
    WKWebViewConfiguration,
};
use protocol::channel::ChatParty;

struct ChatDelegateIvars {
    on_send: Arc<dyn Fn(String) + Send + Sync>,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
    /// Set once the panel exists so `windowShouldClose:` can hide it.
    panel_addr: std::sync::atomic::AtomicUsize,
    /// Set once the webview exists so the `ready` handshake can drive the page.
    webview_addr: std::sync::atomic::AtomicUsize,
    shared: Arc<ChatShared>,
}

/// State shared between the IPC delegate and the [`ChatHandle`]: the page is only driven
/// after it reported `ready` (scripts evaluated before the HTML finished loading are lost),
/// so lines are buffered and replayed on the handshake.
#[derive(Default)]
struct ChatShared {
    ready: std::sync::atomic::AtomicBool,
    /// JS snippets in transcript order (replayed verbatim on `ready`).
    lines: parking_lot::Mutex<Vec<String>>,
}

define_class!(
    // SAFETY: NSObject subclass; no Drop; used only on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RemoteAgentChatDelegate"]
    #[ivars = ChatDelegateIvars]
    struct ChatDelegate;

    unsafe impl NSObjectProtocol for ChatDelegate {}

    // JS → Rust bridge.
    unsafe impl WKScriptMessageHandler for ChatDelegate {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        #[allow(non_snake_case)]
        unsafe fn userContentController_didReceiveScriptMessage(
            &self,
            _ucc: &WKUserContentController,
            message: &WKScriptMessage,
        ) {
            // The body is the JSON string we posted from JS.
            let body = message.body();
            let Some(s) = body.downcast_ref::<NSString>() else {
                return;
            };
            let json = s.to_string();
            match serde_json::from_str::<IpcIn>(&json) {
                Ok(IpcIn::Send { text }) => (self.ivars().on_send)(text),
                Ok(IpcIn::Disconnect) => (self.ivars().on_disconnect)(),
                Ok(IpcIn::Ready) => {
                    let iv = self.ivars();
                    // The page reports ready twice (inline + load event); replay once.
                    if iv.shared.ready.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    tracing::info!("chat window ready");
                    let wv = iv.webview_addr.load(Ordering::SeqCst);
                    if wv != 0 {
                        let mut js = String::from(
                            "window.__agent&&(window.__agent.setConnected(true),window.__agent.setStatus('Connected'));",
                        );
                        for line in iv.shared.lines.lock().iter() {
                            js.push_str(line);
                        }
                        eval_js(wv, js);
                    }
                }
                Err(e) => tracing::debug!("chat IPC parse error: {e}"),
            }
        }
    }

    // Close button hides the window instead of ending the session.
    unsafe impl NSWindowDelegate for ChatDelegate {
        #[unsafe(method(windowShouldClose:))]
        #[allow(non_snake_case)]
        fn windowShouldClose(&self, _sender: &NSWindow) -> bool {
            let addr = self.ivars().panel_addr.load(Ordering::SeqCst);
            if addr != 0 {
                // SAFETY: panel_addr is a live NSPanel while the delegate exists.
                let panel: &NSPanel = unsafe { &*(addr as *const NSPanel) };
                panel.orderOut(None);
            }
            false
        }
    }
);

impl ChatDelegate {
    fn new(
        mtm: MainThreadMarker,
        on_send: Arc<dyn Fn(String) + Send + Sync>,
        on_disconnect: Arc<dyn Fn() + Send + Sync>,
        shared: Arc<ChatShared>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ChatDelegateIvars {
            on_send,
            on_disconnect,
            panel_addr: std::sync::atomic::AtomicUsize::new(0),
            webview_addr: std::sync::atomic::AtomicUsize::new(0),
            shared,
        });
        // SAFETY: plain NSObject init.
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum IpcIn {
    Ready,
    Send { text: String },
    Disconnect,
}

/// Handle whose drop closes the chat panel on the main thread.
struct ChatWebHandle {
    panel_addr: usize,
    webview_addr: usize,
    delegate_addr: usize,
    shared: Arc<ChatShared>,
}

// SAFETY: addresses are only reconstituted on the main queue.
unsafe impl Send for ChatWebHandle {}
unsafe impl Sync for ChatWebHandle {}

impl ChatHandle for ChatWebHandle {
    fn push_line(&self, line: &ChatLine) {
        let from = match line.from {
            ChatParty::Operator => "operator",
            ChatParty::Device => "device",
        };
        // Build the JS call with a JSON payload so text is always safely escaped.
        let payload = serde_json::json!({ "from": from, "text": line.text, "ts_ms": line.ts_ms });
        let js = format!("window.__agent&&window.__agent.push({payload});");
        self.shared.lines.lock().push(js.clone());
        if self.shared.ready.load(Ordering::SeqCst) {
            eval_js(self.webview_addr, js);
        }
        let show = matches!(line.from, ChatParty::Operator);
        if show {
            self.set_visible(true);
        }
    }

    fn set_visible(&self, visible: bool) {
        let panel_addr = self.panel_addr;
        DispatchQueue::main().exec_async(move || {
            // SAFETY: main thread; panel alive while the handle exists.
            unsafe {
                let panel: &NSPanel = &*(panel_addr as *const NSPanel);
                if visible {
                    panel.orderFrontRegardless();
                } else {
                    panel.orderOut(None);
                }
            }
        });
    }
}

impl Drop for ChatWebHandle {
    fn drop(&mut self) {
        let (panel_addr, webview_addr, delegate_addr) =
            (self.panel_addr, self.webview_addr, self.delegate_addr);
        clear_menu_chat();
        DispatchQueue::main().exec_async(move || {
            // SAFETY: reclaims the +1 refs from `open_chat`; main thread; once each.
            unsafe {
                if let Some(panel) = Retained::from_raw(panel_addr as *mut NSPanel) {
                    let window: &NSWindow = &panel;
                    window.setDelegate(None);
                    window.close();
                }
                let _ = Retained::from_raw(webview_addr as *mut WKWebView);
                let _ = Retained::from_raw(delegate_addr as *mut ChatDelegate);
            }
        });
    }
}

/// Evaluate `js` in the webview on the main thread (fire and forget).
fn eval_js(webview_addr: usize, js: String) {
    DispatchQueue::main().exec_async(move || {
        // SAFETY: main thread; the webview is alive while the handle exists.
        unsafe {
            let wv: &WKWebView = &*(webview_addr as *const WKWebView);
            wv.evaluateJavaScript_completionHandler(&NSString::from_str(&js), None);
        }
    });
}

/// Create the chat panel with an embedded WKWebView.
pub fn open_chat(
    operator: &str,
    on_send: Arc<dyn Fn(String) + Send + Sync>,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn ChatHandle>> {
    install_input_guard();
    let operator = operator.to_owned();
    let (tx, rx) = mpsc::channel();
    run_on_main(move || {
        let result = (|| -> Result<(usize, usize, usize, Arc<ChatShared>)> {
            let mtm = MainThreadMarker::new().context("not on main thread")?;
            let width = 380.0;
            let height = 460.0;
            let screen = NSScreen::mainScreen(mtm).context("no main screen")?;
            let visible = screen.visibleFrame();
            let origin = NSPoint::new(
                visible.origin.x + visible.size.width - width - 12.0,
                visible.origin.y + visible.size.height - height - 72.0,
            );
            let frame = NSRect::new(origin, NSSize::new(width, height));
            let style = NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Resizable
                | NSWindowStyleMask::NonactivatingPanel
                | NSWindowStyleMask::UtilityWindow;
            let panel: Retained<NSPanel> = NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            );
            panel.setTitle(&NSString::from_str(&format!("{operator} — Remote support")));
            // Floating: visible above normal windows but not maximally intrusive; the session
            // scopes its lifetime, so it is only on top while a session is active.
            panel.setLevel(objc2_app_kit::NSFloatingWindowLevel);
            panel.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary,
            );
            panel.setHidesOnDeactivate(false);
            panel.setMovableByWindowBackground(true);
            // SAFETY: retained by us; released explicitly on close.
            unsafe { panel.setReleasedWhenClosed(false) };
            // The operator must never SEE the chat window: exclude it from all screen capture.
            // (Skippable only for local UI previews via REMOTE_AGENT_SHOW_WINDOWS.)
            if std::env::var_os("REMOTE_AGENT_SHOW_WINDOWS").is_none() {
                panel.setSharingType(NSWindowSharingType::None);
            }

            let shared = Arc::new(ChatShared::default());
            let delegate = ChatDelegate::new(mtm, on_send, on_disconnect, Arc::clone(&shared));

            // WKWebView configuration + "agent" message handler.
            let config = unsafe { WKWebViewConfiguration::new(mtm) };
            let ucc = unsafe { config.userContentController() };
            let handler = ProtocolObject::from_ref(&*delegate);
            unsafe { ucc.addScriptMessageHandler_name(handler, &NSString::from_str("agent")) };

            let content_frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
            let webview = unsafe {
                WKWebView::initWithFrame_configuration(
                    WKWebView::alloc(mtm),
                    content_frame,
                    &config,
                )
            };
            // Wire window.__ipc → the "agent" handler.
            let bootstrap =
                "window.__ipc=function(s){window.webkit.messageHandlers.agent.postMessage(s);};";
            unsafe {
                webview.evaluateJavaScript_completionHandler(&NSString::from_str(bootstrap), None);
            }
            webview.setAutoresizingMask(
                objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                    | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            let html = chat_html(&operator);
            unsafe { webview.loadHTMLString_baseURL(&NSString::from_str(&html), None) };

            if let Some(content) = panel.contentView() {
                let wv_view: &NSView = &webview;
                content.addSubview(wv_view);
            }

            let panel_ptr = Retained::as_ptr(&panel) as usize;
            delegate
                .ivars()
                .panel_addr
                .store(panel_ptr, Ordering::SeqCst);
            let win: &NSWindow = &panel;
            win.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

            panel.orderFrontRegardless();

            let webview_addr = Retained::into_raw(webview) as usize;
            delegate
                .ivars()
                .webview_addr
                .store(webview_addr, Ordering::SeqCst);
            let panel_addr = Retained::into_raw(panel) as usize;
            let delegate_addr = Retained::into_raw(delegate) as usize;
            // Let the menu-bar item re-open this window and disconnect the session.
            set_menu_chat(panel_addr);
            Ok((panel_addr, webview_addr, delegate_addr, shared))
        })();
        let _ = tx.send(result);
    })?;
    let (panel_addr, webview_addr, delegate_addr, shared) =
        rx.recv().context("chat task dropped")??;
    Ok(Box::new(ChatWebHandle {
        panel_addr,
        webview_addr,
        delegate_addr,
        shared,
    }))
}

// ─── input guard: drop remote-injected events aimed at our own windows ────────────────────

use objc2_app_kit::{NSEvent, NSEventMask};
use objc2_core_graphics::CGEventField;

static INPUT_GUARD: std::sync::Once = std::sync::Once::new();

/// Install a process-wide local event monitor that swallows any mouse/keyboard event carrying
/// the injection marker (see `crate::input::INJECTED_EVENT_MARKER`). Local (real) input is
/// untouched, so the person at the device keeps full control of our windows while the operator
/// cannot click or type into them. Idempotent.
pub fn install_input_guard() {
    INPUT_GUARD.call_once(|| {
        let _ = run_on_main(|| {
            let Some(_mtm) = MainThreadMarker::new() else {
                return;
            };
            let mask = NSEventMask::LeftMouseDown
                | NSEventMask::LeftMouseUp
                | NSEventMask::RightMouseDown
                | NSEventMask::RightMouseUp
                | NSEventMask::OtherMouseDown
                | NSEventMask::OtherMouseUp
                | NSEventMask::ScrollWheel
                | NSEventMask::KeyDown
                | NSEventMask::KeyUp;
            let block = RcBlock::new(|event: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
                // SAFETY: the monitor hands us a valid, live NSEvent for the duration of the call.
                let event: &NSEvent = unsafe { event.as_ref() };
                if event_is_injected(event) {
                    // Returning null drops the event before any of our windows sees it.
                    std::ptr::null_mut()
                } else {
                    event as *const NSEvent as *mut NSEvent
                }
            });
            // SAFETY: valid mask and a block matching the expected signature; the returned
            // monitor object is intentionally leaked for the process lifetime.
            let monitor =
                unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) };
            std::mem::forget(monitor);
            std::mem::forget(block);
        });
    });
}

fn event_is_injected(event: &NSEvent) -> bool {
    if let Some(cg) = event.CGEvent() {
        let v = objc2_core_graphics::CGEvent::integer_value_field(
            Some(&cg),
            CGEventField::EventSourceUserData,
        );
        return v == crate::input::INJECTED_EVENT_MARKER;
    }
    false
}

// ─── menu-bar status item ─────────────────────────────────────────────────────────────────

use objc2_app_kit::{NSMenu, NSMenuItem, NSStatusBar, NSVariableStatusItemLength};
use std::sync::atomic::AtomicUsize;

struct MenuIvars {
    disconnect: Arc<dyn Fn() + Send + Sync>,
}

define_class!(
    // SAFETY: NSObject subclass, main-thread only, no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RemoteAgentMenuTarget"]
    #[ivars = MenuIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(openChat:))]
        fn open_chat(&self, _sender: Option<&AnyObject>) {
            let addr = MENU_CHAT_PANEL.load(Ordering::SeqCst);
            if addr != 0 {
                // SAFETY: only set to a live panel address while a chat exists; cleared on drop.
                let panel: &NSPanel = unsafe { &*(addr as *const NSPanel) };
                panel.orderFrontRegardless();
            }
        }

        #[unsafe(method(disconnect:))]
        fn disconnect(&self, _sender: Option<&AnyObject>) {
            (self.ivars().disconnect)();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            std::process::exit(0);
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}
);

/// Address of the current chat panel, so the menu can re-open it; 0 when no session.
static MENU_CHAT_PANEL: AtomicUsize = AtomicUsize::new(0);

fn set_menu_chat(panel_addr: usize) {
    MENU_CHAT_PANEL.store(panel_addr, Ordering::SeqCst);
}
fn clear_menu_chat() {
    MENU_CHAT_PANEL.store(0, Ordering::SeqCst);
}

/// Install the menu-bar (status bar) item. Idempotent; `disconnect` ends the active session.
/// The item is always present so the person at the device can reach *Open chat* / *Disconnect*
/// even after closing the chat window. Menu items are enabled only while a session is active.
pub fn install_menu_bar(status_text: &str, disconnect: Arc<dyn Fn() + Send + Sync>) {
    static MENU: std::sync::Once = std::sync::Once::new();
    let status_text = status_text.to_owned();
    MENU.call_once(|| {
        let _ = run_on_main(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let bar = NSStatusBar::systemStatusBar();
            let item = bar.statusItemWithLength(NSVariableStatusItemLength);
            if let Some(button) = item.button(mtm) {
                button.setTitle(&NSString::from_str("🖥"));
            }
            let target = {
                let this = MenuTarget::alloc(mtm).set_ivars(MenuIvars { disconnect });
                let t: Retained<MenuTarget> = unsafe { msg_send![super(this), init] };
                t
            };
            let menu = NSMenu::new(mtm);
            let title = NSMenuItem::new(mtm);
            title.setTitle(&NSString::from_str(&status_text));
            title.setEnabled(false);
            menu.addItem(&title);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            // SAFETY: selectors exist on MenuTarget; target retained below.
            let open = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str("Open chat"),
                    Some(sel!(openChat:)),
                    &NSString::from_str(""),
                )
            };
            // SAFETY: target outlives the menu (forgotten below).
            unsafe { open.setTarget(Some(&target)) };
            menu.addItem(&open);
            let disc = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str("Disconnect session"),
                    Some(sel!(disconnect:)),
                    &NSString::from_str(""),
                )
            };
            // SAFETY: target outlives the menu (forgotten below).
            unsafe { disc.setTarget(Some(&target)) };
            menu.addItem(&disc);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            let quit = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str("Quit Remote Agent"),
                    Some(sel!(quit:)),
                    &NSString::from_str("q"),
                )
            };
            // SAFETY: target outlives the menu (forgotten below).
            unsafe { quit.setTarget(Some(&target)) };
            menu.addItem(&quit);
            item.setMenu(Some(&menu));
            // Keep the status item + target + menu alive for the process lifetime.
            let _ = Retained::into_raw(item);
            let _ = Retained::into_raw(target);
            let _ = Retained::into_raw(menu);
        });
    });
}

// ─── clipboard (NSPasteboard) ───────────────────────────────────────────────────────────

use objc2::runtime::AnyClass;
use objc2::ClassType;
use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
use objc2_foundation::{NSArray as NSArr, NSURL};
use std::path::PathBuf;

pub fn clipboard_sequence() -> Option<u64> {
    let pb = NSPasteboard::generalPasteboard();
    Some(pb.changeCount() as u64)
}

pub fn clipboard_files() -> Result<Vec<PathBuf>> {
    let pb = NSPasteboard::generalPasteboard();
    let classes: Retained<NSArr<AnyClass>> = NSArr::from_slice(&[NSURL::class()]);
    // SAFETY: valid class array, no options.
    let objects = unsafe { pb.readObjectsForClasses_options(&classes, None) };
    let mut out = Vec::new();
    if let Some(objects) = objects {
        for obj in objects.iter() {
            if let Some(url) = obj.downcast_ref::<NSURL>() {
                // SAFETY: plain accessors.
                let is_file = url.isFileURL();
                if !is_file {
                    continue;
                }
                if let Some(path) = url.path() {
                    out.push(PathBuf::from(path.to_string()));
                }
            }
        }
    }
    Ok(out)
}

pub fn set_clipboard_files(paths: &[PathBuf]) -> Result<()> {
    let pb = NSPasteboard::generalPasteboard();
    let urls: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
        .iter()
        .map(|p| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&p.display().to_string()));
            ProtocolObject::from_retained(url)
        })
        .collect();
    let array = NSArr::from_retained_slice(&urls);
    pb.clearContents();
    if !pb.writeObjects(&array) {
        return Err(anyhow!("NSPasteboard writeObjects failed"));
    }
    Ok(())
}

use objc2::runtime::ProtocolObject;
