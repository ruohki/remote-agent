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

// ─── chat window ────────────────────────────────────────────────────────────────────────

use crate::chat::{ChatHandle, ChatLine};
use objc2_app_kit::{NSControl, NSScrollView, NSTextView, NSView};
use objc2_foundation::NSRange;
use protocol::channel::ChatParty;

struct ChatIvars {
    on_send: Arc<dyn Fn(String) + Send + Sync>,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
    /// Address of the input `NSTextField` (owned by the panel's view hierarchy).
    input_addr: std::sync::atomic::AtomicUsize,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the type does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RemoteAgentChatTarget"]
    #[ivars = ChatIvars]
    struct ChatTarget;

    impl ChatTarget {
        #[unsafe(method(send:))]
        fn send(&self, _sender: Option<&AnyObject>) {
            let addr = self.ivars().input_addr.load(Ordering::SeqCst);
            if addr == 0 {
                return;
            }
            // SAFETY: the field is alive as long as the panel (which outlives the target).
            let field: &NSTextField = unsafe { &*(addr as *const NSTextField) };
            let text = field.stringValue().to_string();
            if text.trim().is_empty() {
                return;
            }
            field.setStringValue(&NSString::from_str(""));
            (self.ivars().on_send)(text);
        }

        #[unsafe(method(disconnect:))]
        fn disconnect(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_disconnect)();
        }
    }

    unsafe impl NSObjectProtocol for ChatTarget {}
);

impl ChatTarget {
    fn new(
        mtm: MainThreadMarker,
        on_send: Arc<dyn Fn(String) + Send + Sync>,
        on_disconnect: Arc<dyn Fn() + Send + Sync>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ChatIvars {
            on_send,
            on_disconnect,
            input_addr: std::sync::atomic::AtomicUsize::new(0),
        });
        // SAFETY: plain NSObject init on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Handle whose drop closes the chat panel on the main thread (see [`PanelHandle`]).
struct ChatPanelHandle {
    panel_addr: usize,
    target_addr: usize,
    transcript_addr: usize,
    text: Arc<parking_lot::Mutex<String>>,
}

// SAFETY: the addresses are only converted back to `Retained` / references on the main queue.
unsafe impl Send for ChatPanelHandle {}
unsafe impl Sync for ChatPanelHandle {}

impl ChatHandle for ChatPanelHandle {
    fn push_line(&self, line: &ChatLine) {
        let who = match line.from {
            ChatParty::Operator => "Operator",
            ChatParty::Device => "You",
        };
        let rendered = {
            let mut t = self.text.lock();
            if !t.is_empty() {
                t.push('\n');
            }
            t.push_str(&format!("{who}: {}", line.text));
            t.clone()
        };
        let transcript_addr = self.transcript_addr;
        let panel_addr = self.panel_addr;
        DispatchQueue::main().exec_async(move || {
            // SAFETY: main thread; the views are alive while the panel is (we hold +1).
            unsafe {
                let tv: &NSTextView = &*(transcript_addr as *const NSTextView);
                let s = NSString::from_str(&rendered);
                tv.setString(&s);
                let len = s.length();
                tv.scrollRangeToVisible(NSRange::new(len, 0));
                let panel: &NSPanel = &*(panel_addr as *const NSPanel);
                panel.orderFrontRegardless();
            }
        });
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

impl Drop for ChatPanelHandle {
    fn drop(&mut self) {
        let panel_addr = self.panel_addr;
        let target_addr = self.target_addr;
        DispatchQueue::main().exec_async(move || {
            // SAFETY: reclaims the +1 references created in `open_chat`; main thread; each
            // address is turned back into a `Retained` exactly once.
            unsafe {
                if let Some(panel) = Retained::from_raw(panel_addr as *mut NSPanel) {
                    let window: &NSWindow = &panel;
                    window.close();
                }
                let _ = Retained::from_raw(target_addr as *mut ChatTarget);
            }
        });
    }
}

/// Create the chat panel: transcript, input field, Send and Disconnect.
pub fn open_chat(
    operator: &str,
    on_send: Arc<dyn Fn(String) + Send + Sync>,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn ChatHandle>> {
    let operator = operator.to_owned();
    let (tx, rx) = mpsc::channel();
    run_on_main(move || {
        let result = (|| -> Result<(usize, usize, usize)> {
            let mtm = MainThreadMarker::new().context("not on main thread")?;
            let width = 360.0;
            let height = 300.0;
            let screen = NSScreen::mainScreen(mtm).context("no main screen")?;
            let visible = screen.visibleFrame();
            // Below the session banner (which sits in the top-right corner).
            let origin = NSPoint::new(
                visible.origin.x + visible.size.width - width - 12.0,
                visible.origin.y + visible.size.height - height - 72.0,
            );
            let frame = NSRect::new(origin, NSSize::new(width, height));
            let style = NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::NonactivatingPanel
                | NSWindowStyleMask::UtilityWindow;
            let panel: Retained<NSPanel> = NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            );
            panel.setTitle(&NSString::from_str(&format!("Chat with {operator}")));
            panel.setLevel(NSStatusWindowLevel);
            panel.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary,
            );
            panel.setHidesOnDeactivate(false);
            // SAFETY: the panel is retained by us and released explicitly on close.
            unsafe { panel.setReleasedWhenClosed(false) };

            let target = ChatTarget::new(mtm, on_send, on_disconnect);

            // Transcript (read-only text view in a scroll view).
            let scroll = NSScrollView::initWithFrame(
                NSScrollView::alloc(mtm),
                NSRect::new(
                    NSPoint::new(8.0, 44.0),
                    NSSize::new(width - 16.0, height - 52.0),
                ),
            );
            scroll.setHasVerticalScroller(true);
            let text_view = NSTextView::initWithFrame(
                NSTextView::alloc(mtm),
                NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(width - 16.0, height - 52.0),
                ),
            );
            text_view.setEditable(false);
            text_view.setString(&NSString::from_str(""));
            let doc: &NSView = &text_view;
            scroll.setDocumentView(Some(doc));

            // Input field + buttons.
            let input = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
            input.setFrame(NSRect::new(
                NSPoint::new(8.0, 8.0),
                NSSize::new(width - 16.0 - 64.0 - 96.0 - 8.0, 28.0),
            ));
            input.setPlaceholderString(Some(&NSString::from_str("Type a message…")));
            let control: &NSControl = &input;
            // SAFETY: target is retained for the panel's lifetime; selector exists on the class.
            unsafe {
                control.setTarget(Some(&target));
                control.setAction(Some(sel!(send:)));
            }
            target
                .ivars()
                .input_addr
                .store(Retained::as_ptr(&input) as usize, Ordering::SeqCst);

            // SAFETY: see above.
            let send = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str("Send"),
                    Some(&target),
                    Some(sel!(send:)),
                    mtm,
                )
            };
            send.setFrame(NSRect::new(
                NSPoint::new(width - 8.0 - 96.0 - 8.0 - 64.0, 8.0),
                NSSize::new(64.0, 28.0),
            ));
            // SAFETY: see above.
            let disconnect = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str("Disconnect"),
                    Some(&target),
                    Some(sel!(disconnect:)),
                    mtm,
                )
            };
            disconnect.setFrame(NSRect::new(
                NSPoint::new(width - 8.0 - 96.0, 8.0),
                NSSize::new(96.0, 28.0),
            ));
            if let Some(content) = panel.contentView() {
                content.addSubview(&scroll);
                content.addSubview(&input);
                content.addSubview(&send);
                content.addSubview(&disconnect);
            }
            // The text view is owned by the scroll view (part of the panel's hierarchy).
            let transcript_addr = Retained::as_ptr(&text_view) as usize;
            panel.orderFrontRegardless();
            Ok((
                Retained::into_raw(panel) as usize,
                Retained::into_raw(target) as usize,
                transcript_addr,
            ))
        })();
        let _ = tx.send(result);
    })?;
    let (panel_addr, target_addr, transcript_addr) = rx.recv().context("chat task dropped")??;
    Ok(Box::new(ChatPanelHandle {
        panel_addr,
        target_addr,
        transcript_addr,
        text: Arc::new(parking_lot::Mutex::new(String::new())),
    }))
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
