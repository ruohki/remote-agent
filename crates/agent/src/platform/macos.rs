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
    NSWindowCollectionBehavior, NSWindowStyleMask,
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
