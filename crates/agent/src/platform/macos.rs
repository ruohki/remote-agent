//! macOS specifics: AppKit main loop, approval dialog (`NSAlert`), session indicator panel,
//! console user lookup and TCC permission checks.

use crate::approval::{ApprovalOutcome, IndicatorHandle};
use anyhow::{anyhow, Context, Result};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSBackingStoreType, NSButton,
    NSPanel, NSScreen, NSStatusWindowLevel, NSTextField, NSWindow, NSWindowCollectionBehavior,
    NSWindowSharingType, NSWindowStyleMask,
};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

// ─── capture exclusion ────────────────────────────────────────────────────────────────────

/// Exclude an `NSWindow` (e.g. the app window created by tao/wry) from every screen capture, so
/// the operator never sees it. `ns_window` is the pointer returned by tao's `WindowExtMacOS`.
pub fn exclude_ns_window_from_capture(ns_window: *mut c_void) {
    if ns_window.is_null() {
        return;
    }
    let addr = ns_window as usize;
    let _ = run_on_main(move || {
        // SAFETY: `addr` is a live `NSWindow` created by tao for the process lifetime; the call
        // happens on the main thread.
        let window: &NSWindow = unsafe { &*(addr as *const NSWindow) };
        window.setSharingType(NSWindowSharingType::None);
    });
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
/// `(total, visible)` windows owned by this process — diagnostics for window leaks.
pub fn window_counts() -> (usize, usize) {
    run_on_main(|| {
        let Some(mtm) = MainThreadMarker::new() else {
            return (0, 0);
        };
        let app = NSApplication::sharedApplication(mtm);
        let windows = app.windows();
        let total = windows.len();
        let visible = windows.iter().filter(|w| w.isVisible()).count();
        for w in windows.iter() {
            tracing::debug!(
                class = %w.class().name().to_str().unwrap_or("?"),
                title = %w.title(),
                visible = w.isVisible(),
                "window"
            );
        }
        (total, visible)
    })
    .unwrap_or((0, 0))
}

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
        let branding = crate::branding::current();
        let product = crate::branding::product_name();
        alert.setMessageText(&NSString::from_str(&format!("{product} — Support request")));
        let support = if branding.support_text.trim().is_empty() {
            String::new()
        } else {
            format!("\n\n{}", branding.support_text.trim())
        };
        alert.setInformativeText(&NSString::from_str(&format!(
            "{operator} requests remote access to this computer (screen sharing and remote control).\n\nThe request expires in {} seconds.{support}",
            timeout.as_secs()
        )));
        if let Some(icon) = dock_icon_image(mtm) {
            // SAFETY: the alert retains the image; main thread.
            unsafe { alert.setIcon(Some(&icon)) };
        }
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
//
// One banner panel per process: created on first use, re-pointed at each new session
// (operator name + callback), hidden when the session ends. Never destroyed.

/// Callback of the session currently attached to the banner (no-op between sessions).
struct BannerShared {
    on_disconnect: parking_lot::Mutex<Arc<dyn Fn() + Send + Sync>>,
}

fn detached_disconnect() -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(|| tracing::debug!("disconnect pressed but no session is attached"))
}

struct TargetIvars {
    shared: Arc<BannerShared>,
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
            let cb = Arc::clone(&*self.ivars().shared.on_disconnect.lock());
            cb();
        }
        // Clicking the banner body opens the branded app window.
        #[unsafe(method(openApp:))]
        fn open_app(&self, _sender: Option<&AnyObject>) {
            crate::app::post(crate::app::AppEvent::Show { chat: false });
        }
    }

    unsafe impl NSObjectProtocol for IndicatorTarget {}
);

impl IndicatorTarget {
    fn new(mtm: MainThreadMarker, shared: Arc<BannerShared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { shared });
        // SAFETY: plain NSObject init on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// The process-wide banner. Addresses are `Retained` pointers leaked on purpose (the banner
/// lives as long as the process) and only dereferenced on the main thread.
struct BannerSingleton {
    panel_addr: usize,
    label_addr: usize,
    shared: Arc<BannerShared>,
}

static BANNER: parking_lot::Mutex<Option<BannerSingleton>> = parking_lot::Mutex::new(None);

/// Session-scoped handle: dropping it detaches the session and hides the banner.
struct PanelHandle {
    panel_addr: usize,
    shared: Arc<BannerShared>,
}

// SAFETY: the address is only dereferenced on the main queue.
unsafe impl Send for PanelHandle {}
unsafe impl Sync for PanelHandle {}

impl IndicatorHandle for PanelHandle {}

impl Drop for PanelHandle {
    fn drop(&mut self) {
        *self.shared.on_disconnect.lock() = detached_disconnect();
        let panel_addr = self.panel_addr;
        DispatchQueue::main().exec_async(move || {
            // SAFETY: main thread; the banner panel lives for the whole process.
            let panel: &NSPanel = unsafe { &*(panel_addr as *const NSPanel) };
            panel.orderOut(None);
        });
    }
}

fn banner_text(operator: &str) -> String {
    let product = crate::branding::product_name();
    format!("{product} — Support session active: {operator} is connected")
}

/// Branded app icon as an `NSImage` (from the logo or the default mark).
fn dock_icon_image(_mtm: MainThreadMarker) -> Option<Retained<objc2_app_kit::NSImage>> {
    use objc2::AllocAnyThread;
    let png = crate::branding::encode_png(&crate::branding::app_icon(256));
    let data = objc2_foundation::NSData::with_bytes(&png);
    objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &data)
}

/// Accessibility permission with the system prompt (`AXIsProcessTrustedWithOptions`).
pub fn request_accessibility() -> bool {
    use objc2_foundation::{NSDictionary, NSNumber};
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    }
    let key = NSString::from_str("AXTrustedCheckOptionPrompt");
    let value = NSNumber::new_bool(true);
    let dict = NSDictionary::from_slices(&[&*key], &[&*value]);
    // SAFETY: NSDictionary is toll-free bridged to CFDictionaryRef; it outlives the call.
    unsafe { AXIsProcessTrustedWithOptions(Retained::as_ptr(&dict) as *const c_void) }
}

/// Open System Settings on a privacy pane.
pub fn open_privacy_settings(pane: crate::platform::PrivacyPane) {
    let url = match pane {
        crate::platform::PrivacyPane::ScreenCapture => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        crate::platform::PrivacyPane::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
    };
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!("opening System Settings: {e}");
    }
}

/// Bundle / translocation / Applications-folder detection for the running executable.
pub fn exe_location() -> crate::platform::ExeLocation {
    let mut loc = crate::platform::ExeLocation::default();
    let Ok(exe) = std::env::current_exe() else {
        return loc;
    };
    let text = exe.to_string_lossy();
    loc.translocated = text.contains("/AppTranslocation/");
    // …/<Name>.app/Contents/MacOS/<bin>
    loc.bundle = exe
        .ancestors()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("app"))
        .map(|p| p.to_path_buf());
    if let Some(b) = &loc.bundle {
        let s = b.to_string_lossy();
        let home = std::env::var("HOME").unwrap_or_default();
        loc.in_applications = s.starts_with("/Applications/")
            || (!home.is_empty() && s.starts_with(&format!("{home}/Applications/")));
    }
    loc
}

/// Copy the bundle into `/Applications` (or `~/Applications` when not writable, or
/// `REMOTE_AGENT_APPLICATIONS_DIR`), relaunch from there and exit this instance.
pub fn move_to_applications() -> Result<String> {
    let loc = exe_location();
    let src = loc
        .bundle
        .clone()
        .context("not running from an app bundle")?;
    let name = src.file_name().context("bundle name")?.to_owned();
    let dest_dir = match std::env::var_os("REMOTE_AGENT_APPLICATIONS_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => {
            let sys = std::path::PathBuf::from("/Applications");
            let probe = sys.join(".remote-agent-write-test");
            let writable = std::fs::File::create(&probe).is_ok();
            let _ = std::fs::remove_file(&probe);
            if writable {
                sys
            } else {
                let home = std::env::var("HOME").context("HOME")?;
                std::path::PathBuf::from(home).join("Applications")
            }
        }
    };
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let dest = dest_dir.join(&name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).with_context(|| format!("replacing {}", dest.display()))?;
    }
    let status = std::process::Command::new("ditto")
        .arg(&src)
        .arg(&dest)
        .status()
        .context("running ditto")?;
    if !status.success() {
        anyhow::bail!("ditto exited with {status}");
    }
    // Clear quarantine so the copy launches without a second Gatekeeper round-trip.
    let _ = std::process::Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&dest)
        .status();
    // Relaunch from the new location, then leave. The translocated original cannot be deleted
    // (read-only mount); a plain download is removed so only one copy remains.
    std::process::Command::new("open")
        .arg("-n")
        .arg(&dest)
        .spawn()
        .context("relaunching")?;
    if !loc.translocated {
        let _ = std::fs::remove_dir_all(&src);
    }
    let dest_text = dest.display().to_string();
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(1500));
        std::process::exit(0);
    });
    Ok(format!("Moved to {dest_text}; relaunching"))
}

/// Developer aid: `REMOTE_AGENT_APPEARANCE=light|dark` forces the app's appearance (screenshots).
pub fn apply_debug_appearance() {
    let Some(want) = std::env::var_os("REMOTE_AGENT_APPEARANCE") else {
        return;
    };
    let name = match want.to_string_lossy().as_ref() {
        "dark" => "NSAppearanceNameDarkAqua",
        "light" => "NSAppearanceNameAqua",
        _ => return,
    };
    let _ = run_on_main(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let appearance = objc2_app_kit::NSAppearance::appearanceNamed(&NSString::from_str(name));
        NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
    });
}

/// Set the dock icon from PNG bytes (main thread, fire and forget).
pub fn set_dock_icon(png: &[u8]) {
    let png = png.to_vec();
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        use objc2::AllocAnyThread;
        let data = objc2_foundation::NSData::with_bytes(&png);
        let image = objc2_app_kit::NSImage::initWithData(objc2_app_kit::NSImage::alloc(), &data);
        if let Some(image) = image {
            // SAFETY: main thread; NSApplication is a singleton.
            unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
        }
    });
}

/// Show the banner for `operator`. Returns a handle whose drop hides the banner again.
pub fn show_indicator(
    operator: &str,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn IndicatorHandle>> {
    let operator = operator.to_owned();
    // Attach to the existing banner when there is one.
    let existing = BANNER
        .lock()
        .as_ref()
        .map(|b| (b.panel_addr, b.label_addr, Arc::clone(&b.shared)));
    if let Some((panel_addr, label_addr, shared)) = existing {
        *shared.on_disconnect.lock() = on_disconnect;
        let text = banner_text(&operator);
        run_on_main(move || {
            // SAFETY: main thread; both objects live for the whole process.
            unsafe {
                let label: &NSTextField = &*(label_addr as *const NSTextField);
                label.setStringValue(&NSString::from_str(&text));
                let panel: &NSPanel = &*(panel_addr as *const NSPanel);
                panel.orderFrontRegardless();
            }
        })?;
        return Ok(Box::new(PanelHandle { panel_addr, shared }));
    }

    let shared = Arc::new(BannerShared {
        on_disconnect: parking_lot::Mutex::new(on_disconnect),
    });
    let shared_for_main = Arc::clone(&shared);
    let (tx, rx) = mpsc::channel();
    run_on_main(move || {
        let result = (|| -> Result<(usize, usize)> {
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
            panel.setTitle(&NSString::from_str(&format!(
                "{} — remote session",
                crate::branding::product_name()
            )));
            panel.setLevel(NSStatusWindowLevel);
            panel.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary,
            );
            panel.setHidesOnDeactivate(false);
            // SAFETY: the panel is retained by us for the whole process.
            unsafe { panel.setReleasedWhenClosed(false) };
            // Keep the banner out of the screen capture the operator sees.
            if std::env::var_os("REMOTE_AGENT_SHOW_WINDOWS").is_none() {
                panel.setSharingType(NSWindowSharingType::None);
            }

            let target = IndicatorTarget::new(mtm, shared_for_main);
            // Accent stripe on the left edge (branding colour).
            let (ar, ag, ab) = crate::branding::accent_rgb();
            let stripe = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            stripe.setFrame(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(5.0, height),
            ));
            stripe.setDrawsBackground(true);
            stripe.setBackgroundColor(Some(
                &objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(
                    ar as f64 / 255.0,
                    ag as f64 / 255.0,
                    ab as f64 / 255.0,
                    1.0,
                ),
            ));
            let label =
                NSTextField::labelWithString(&NSString::from_str(&banner_text(&operator)), mtm);
            label.setFrame(NSRect::new(
                NSPoint::new(16.0, 12.0),
                NSSize::new(width - 124.0, 20.0),
            ));
            // SAFETY: target is retained for the panel's lifetime, selector exists on the class.
            let button = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str("End session"),
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
                content.addSubview(&stripe);
                content.addSubview(&label);
                content.addSubview(&button);
                // Clicking the banner (outside the button) opens the app window.
                let gesture = unsafe {
                    objc2_app_kit::NSClickGestureRecognizer::initWithTarget_action(
                        objc2_app_kit::NSClickGestureRecognizer::alloc(mtm),
                        Some(&target),
                        Some(sel!(openApp:)),
                    )
                };
                content.addGestureRecognizer(&gesture);
            }
            panel.orderFrontRegardless();
            // Leak the +1 references on purpose: the banner lives as long as the process.
            let _ = Retained::into_raw(target);
            let label_addr = Retained::into_raw(label) as usize;
            let panel_addr = Retained::into_raw(panel) as usize;
            Ok((panel_addr, label_addr))
        })();
        let _ = tx.send(result);
    })?;
    let (panel_addr, label_addr) = rx.recv().context("indicator task dropped")??;
    *BANNER.lock() = Some(BannerSingleton {
        panel_addr,
        label_addr,
        shared: Arc::clone(&shared),
    });
    Ok(Box::new(PanelHandle { panel_addr, shared }))
}

// ─── chat window: a WKWebView hosting the messaging UI (see chat_assets.rs) ───────────────
//
// The window is an `NSPanel` whose content view is a `WKWebView`. IPC: JS posts to a
// `WKScriptMessageHandler` named "agent"; Rust drives the page with `evaluateJavaScript`.
// There is exactly ONE chat window per process: it is created hidden on the first session and
// re-used afterwards (operator name swapped, transcript reset). Closing the window HIDES it
// (the session keeps running); the local user re-opens it from the menu-bar item. When a
// session ends the window shows "Session ended" and is hidden unless the user is looking at
// it. Remote-injected events are dropped app-wide by `install_input_guard`.

use crate::chat::{ChatHandle, ChatLine};
use crate::platform::chat_assets::chat_html;
use block2::RcBlock;
use objc2_app_kit::{NSView, NSWindowDelegate};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView,
    WKWebViewConfiguration,
};
use protocol::channel::ChatParty;

type SendCb = Arc<dyn Fn(String) + Send + Sync>;
type DisconnectCb = Arc<dyn Fn() + Send + Sync>;

/// Callbacks of the session currently attached to the window (no-ops between sessions).
struct ChatCallbacks {
    on_send: SendCb,
    on_disconnect: DisconnectCb,
}

impl ChatCallbacks {
    fn detached() -> Self {
        Self {
            on_send: Arc::new(|_| tracing::debug!("chat line typed but no session is attached")),
            on_disconnect: detached_disconnect(),
        }
    }
}

/// State shared between the IPC delegate, the [`ChatHandle`]s and the singleton registry.
///
/// The page is only driven after it reported `ready` (scripts evaluated before the HTML
/// finished loading are lost), so lines are buffered and replayed on the handshake.
#[derive(Default)]
struct ChatShared {
    ready: std::sync::atomic::AtomicBool,
    /// Whether a session is attached (drives the connected state shown in the page).
    active: std::sync::atomic::AtomicBool,
    /// JS snippets of the current session's transcript (replayed verbatim on `ready`).
    lines: parking_lot::Mutex<Vec<String>>,
    callbacks: parking_lot::Mutex<Option<ChatCallbacks>>,
    /// Operator lines since the local user last replied or closed the window.
    unread: std::sync::atomic::AtomicUsize,
}

impl ChatShared {
    fn callbacks(&self) -> (SendCb, DisconnectCb) {
        let guard = self.callbacks.lock();
        match guard.as_ref() {
            Some(c) => (Arc::clone(&c.on_send), Arc::clone(&c.on_disconnect)),
            None => {
                let d = ChatCallbacks::detached();
                (d.on_send, d.on_disconnect)
            }
        }
    }
}

struct ChatDelegateIvars {
    /// Set once the panel exists so `windowShouldClose:` can hide it.
    panel_addr: std::sync::atomic::AtomicUsize,
    /// Set once the webview exists so the `ready` handshake can drive the page.
    webview_addr: std::sync::atomic::AtomicUsize,
    shared: Arc<ChatShared>,
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
            let iv = self.ivars();
            match serde_json::from_str::<IpcIn>(&json) {
                Ok(IpcIn::Send { text }) => {
                    iv.shared.unread.store(0, Ordering::SeqCst);
                    (iv.shared.callbacks().0)(text)
                }
                Ok(IpcIn::Disconnect) => (iv.shared.callbacks().1)(),
                Ok(IpcIn::Ready) => {
                    // The page reports ready twice (inline + load event); replay once.
                    if iv.shared.ready.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    tracing::info!("chat window ready");
                    let wv = iv.webview_addr.load(Ordering::SeqCst);
                    if wv != 0 {
                        let mut js = String::new();
                        if iv.shared.active.load(Ordering::SeqCst) {
                            js.push_str(CONNECTED_JS);
                        } else {
                            js.push_str(ENDED_JS);
                        }
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
            self.ivars().shared.unread.store(0, Ordering::SeqCst);
            let addr = self.ivars().panel_addr.load(Ordering::SeqCst);
            if addr != 0 {
                // SAFETY: panel_addr is a live NSPanel (process lifetime).
                let panel: &NSPanel = unsafe { &*(addr as *const NSPanel) };
                panel.orderOut(None);
            }
            false
        }
    }
);

impl ChatDelegate {
    fn new(mtm: MainThreadMarker, shared: Arc<ChatShared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ChatDelegateIvars {
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

const CONNECTED_JS: &str =
    "window.__agent&&(window.__agent.setConnected(true),window.__agent.setStatus('Connected'));";
const ENDED_JS: &str = "window.__agent&&(window.__agent.setConnected(false),window.__agent.setStatus('Session ended'));";

/// The process-wide chat window. Addresses are `Retained` pointers leaked on purpose and only
/// dereferenced on the main thread.
struct ChatSingleton {
    panel_addr: usize,
    webview_addr: usize,
    shared: Arc<ChatShared>,
}

static CHAT: parking_lot::Mutex<Option<ChatSingleton>> = parking_lot::Mutex::new(None);

/// Session-scoped handle: dropping it detaches the session from the window (which stays).
struct ChatWebHandle {
    panel_addr: usize,
    webview_addr: usize,
    shared: Arc<ChatShared>,
}

// SAFETY: addresses are only dereferenced on the main queue.
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
        if matches!(line.from, ChatParty::Operator) {
            self.shared.unread.fetch_add(1, Ordering::SeqCst);
            self.set_visible(true);
        }
    }

    fn set_visible(&self, visible: bool) {
        let panel_addr = self.panel_addr;
        DispatchQueue::main().exec_async(move || {
            // SAFETY: main thread; the panel lives for the whole process.
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
        // Detach the session: typing/End session become no-ops, the page shows "Session ended".
        self.shared.active.store(false, Ordering::SeqCst);
        *self.shared.callbacks.lock() = None;
        if self.shared.ready.load(Ordering::SeqCst) {
            eval_js(self.webview_addr, ENDED_JS.to_owned());
        }
        // Keep the window on screen only when the operator wrote something the person at the
        // device has neither answered nor dismissed; otherwise hide it until the next session.
        if self.shared.unread.load(Ordering::SeqCst) == 0 {
            self.set_visible(false);
        }
    }
}

/// Evaluate `js` in the webview on the main thread (fire and forget).
fn eval_js(webview_addr: usize, js: String) {
    DispatchQueue::main().exec_async(move || {
        // SAFETY: main thread; the webview lives for the whole process.
        unsafe {
            let wv: &WKWebView = &*(webview_addr as *const WKWebView);
            wv.evaluateJavaScript_completionHandler(&NSString::from_str(&js), None);
        }
    });
}

fn chat_title(operator: &str) -> String {
    format!("{operator} — Remote support")
}

/// Attach a session to the chat window, creating the window (hidden) on first use.
pub fn open_chat(
    operator: &str,
    on_send: Arc<dyn Fn(String) + Send + Sync>,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn ChatHandle>> {
    install_input_guard();
    let operator = operator.to_owned();
    let callbacks = ChatCallbacks {
        on_send,
        on_disconnect,
    };

    // Re-use the existing window: new operator, empty transcript, connected state.
    let existing = CHAT
        .lock()
        .as_ref()
        .map(|c| (c.panel_addr, c.webview_addr, Arc::clone(&c.shared)));
    if let Some((panel_addr, webview_addr, shared)) = existing {
        shared.lines.lock().clear();
        shared.unread.store(0, Ordering::SeqCst);
        *shared.callbacks.lock() = Some(callbacks);
        shared.active.store(true, Ordering::SeqCst);
        let title = chat_title(&operator);
        let reset = format!(
            "window.__agent&&window.__agent.reset({});{CONNECTED_JS}",
            serde_json::json!(operator)
        );
        let ready = shared.ready.load(Ordering::SeqCst);
        run_on_main(move || {
            // SAFETY: main thread; objects live for the whole process.
            unsafe {
                let panel: &NSPanel = &*(panel_addr as *const NSPanel);
                panel.setTitle(&NSString::from_str(&title));
                if ready {
                    let wv: &WKWebView = &*(webview_addr as *const WKWebView);
                    wv.evaluateJavaScript_completionHandler(&NSString::from_str(&reset), None);
                }
            }
        })?;
        return Ok(Box::new(ChatWebHandle {
            panel_addr,
            webview_addr,
            shared,
        }));
    }

    let shared = Arc::new(ChatShared::default());
    *shared.callbacks.lock() = Some(callbacks);
    shared.active.store(true, Ordering::SeqCst);
    let shared_for_main = Arc::clone(&shared);
    let (tx, rx) = mpsc::channel();
    run_on_main(move || {
        let result = (|| -> Result<(usize, usize)> {
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
                | NSWindowStyleMask::Miniaturizable
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
            panel.setTitle(&NSString::from_str(&chat_title(&operator)));
            // Floating: visible above normal windows but not maximally intrusive.
            panel.setLevel(objc2_app_kit::NSFloatingWindowLevel);
            panel.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary,
            );
            panel.setHidesOnDeactivate(false);
            panel.setMovableByWindowBackground(true);
            // SAFETY: retained by us for the whole process.
            unsafe { panel.setReleasedWhenClosed(false) };
            // The operator must never SEE the chat window: exclude it from all screen capture.
            // (Skippable only for local UI previews via REMOTE_AGENT_SHOW_WINDOWS.)
            if std::env::var_os("REMOTE_AGENT_SHOW_WINDOWS").is_none() {
                panel.setSharingType(NSWindowSharingType::None);
            }

            let delegate = ChatDelegate::new(mtm, shared_for_main);

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

            // Created hidden: the session shows it on the first operator message.
            let webview_addr = Retained::into_raw(webview) as usize;
            delegate
                .ivars()
                .webview_addr
                .store(webview_addr, Ordering::SeqCst);
            let panel_addr = Retained::into_raw(panel) as usize;
            // Leak the delegate on purpose (window delegate + script handler for the process).
            let _ = Retained::into_raw(delegate);
            // Let the menu-bar item re-open this window at any time.
            set_menu_chat(panel_addr);
            Ok((panel_addr, webview_addr))
        })();
        let _ = tx.send(result);
    })?;
    let (panel_addr, webview_addr) = rx.recv().context("chat task dropped")??;
    *CHAT.lock() = Some(ChatSingleton {
        panel_addr,
        webview_addr,
        shared: Arc::clone(&shared),
    });
    Ok(Box::new(ChatWebHandle {
        panel_addr,
        webview_addr,
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
                // SAFETY: only ever set to the process-wide chat panel, which is never freed.
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

/// Address of the (process-wide) chat panel, so the menu can re-open it; 0 until created.
static MENU_CHAT_PANEL: AtomicUsize = AtomicUsize::new(0);

fn set_menu_chat(panel_addr: usize) {
    MENU_CHAT_PANEL.store(panel_addr, Ordering::SeqCst);
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
