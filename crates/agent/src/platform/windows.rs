//! Windows specifics: console user lookup, approval dialog (`MessageBoxW`), a session
//! indicator window, `SendSAS`, and helpers for launching a process in the active session.

use crate::approval::{ApprovalOutcome, IndicatorHandle};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSUserName,
    WTS_CURRENT_SERVER_HANDLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_SYSTEMMODAL, MB_TOPMOST, MB_YESNO,
};

/// Whether Windows apps use the dark theme (`HKCU\\...\\Personalize\\AppsUseLightTheme == 0`).
pub fn dark_theme() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let sub = HSTRING::from(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = HSTRING::from("AppsUseLightTheme");
    let mut value: u32 = 1;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: the out-buffer is a u32 sized as advertised; the strings outlive the call.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok() && value == 0
}

/// User name of the active console session, if someone is logged in.
pub fn console_user() -> Option<String> {
    // SAFETY: returns 0xFFFFFFFF when no one is logged in.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == 0xFFFF_FFFF {
        return None;
    }
    let mut buffer: PCWSTR = PCWSTR::null();
    let mut bytes: u32 = 0;
    // SAFETY: WTSQuerySessionInformationW allocates the string; we free it with WTSFreeMemory.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTSUserName,
            &mut buffer as *mut PCWSTR as *mut _,
            &mut bytes,
        )
    };
    if ok.is_err() || buffer.is_null() {
        return None;
    }
    // SAFETY: buffer points to a NUL-terminated wide string owned by us until freed.
    let name = unsafe { buffer.to_string().ok() };
    unsafe { WTSFreeMemory(buffer.as_ptr() as *mut _) };
    name.filter(|s| !s.is_empty())
}

/// `MessageBoxW` Yes/No, topmost, auto-denied after `timeout`.
///
/// Runs on a dedicated thread; the timeout posts a click by simulating a close through
/// `EndTask`-style behaviour is not available for a message box, so instead we open it with a
/// timeout via a watchdog that destroys it.
pub fn approval_dialog(operator: &str, timeout: Duration) -> Result<ApprovalOutcome> {
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};

    let text = HSTRING::from(format!(
        "{operator} wants to view and control this computer through the remote support console.\n\nAllow this session? It is denied automatically after {} seconds.",
        timeout.as_secs()
    ));
    let caption = HSTRING::from("Remote session request");
    let caption_find = caption.clone();
    let answered = Arc::new(AtomicBool::new(false));
    let answered_watch = Arc::clone(&answered);

    // Watchdog: close the dialog by window title if it is still open at timeout.
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        if !answered_watch.load(Ordering::SeqCst) {
            // SAFETY: FindWindowW/PostMessageW with valid arguments; failure is ignored.
            unsafe {
                if let Ok(hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(caption_find.as_ptr())) {
                    let _ =
                        PostMessageW(Some(hwnd), WM_CLOSE, Default::default(), Default::default());
                }
            }
        }
    });

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // SAFETY: MessageBoxW with valid wide strings; HWND null = no owner window.
        let result = unsafe {
            MessageBoxW(
                Some(HWND::default()),
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND | MB_SYSTEMMODAL,
            )
        };
        answered.store(true, Ordering::SeqCst);
        let outcome = if result == IDYES {
            ApprovalOutcome::Approved
        } else if result.0 == 0 {
            // 0 = box was closed by the watchdog (timeout).
            ApprovalOutcome::TimedOut
        } else {
            ApprovalOutcome::Denied
        };
        let _ = tx.send(outcome);
    });

    rx.recv().context("approval dialog thread dropped")
}

/// Minimal session indicator: a topmost message-style toast is too intrusive, so we log and
/// return a no-op handle. A full always-on-top banner window is a known gap on Windows.
pub fn show_indicator(
    operator: &str,
    _on_disconnect: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn IndicatorHandle>> {
    tracing::info!("remote session active (operator: {operator})");
    struct NoBanner;
    impl IndicatorHandle for NoBanner {}
    Ok(Box::new(NoBanner))
}

/// Exclude a window (e.g. the tao/wry app window) from every screen capture that honours it
/// (DXGI desktop duplication on Windows 10 2004+). `hwnd` is tao's `WindowExtWindows::hwnd()`.
/// Make an overlay window transparent to input, topmost, and absent from the taskbar / Alt-Tab.
pub fn configure_overlay_hwnd(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    };
    if hwnd == 0 {
        return;
    }
    // SAFETY: plain style bit twiddling on a window we own.
    unsafe {
        let h = HWND(hwnd as *mut core::ffi::c_void);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let add = (WS_EX_TRANSPARENT.0
            | WS_EX_LAYERED.0
            | WS_EX_TOPMOST.0
            | WS_EX_TOOLWINDOW.0
            | WS_EX_NOACTIVATE.0) as isize;
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex | add);
    }
}

/// Make the session bar a layered, topmost tool window (per-pixel alpha from the transparent
/// WebView2 surface, no taskbar entry). Unlike the overlay it keeps receiving input.
pub fn configure_bar_hwnd(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST,
    };
    if hwnd == 0 {
        return;
    }
    // SAFETY: plain style bit twiddling on a window we own.
    unsafe {
        let h = HWND(hwnd as *mut core::ffi::c_void);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let add = (WS_EX_LAYERED.0 | WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0) as isize;
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex | add);
    }
}

/// Make a privacy-screen window a topmost tool window that still takes input and focus
/// (unlike the overlay, which is click-through and non-activating).
pub fn configure_privacy_hwnd(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_EX_TRANSPARENT,
    };
    if hwnd == 0 {
        return;
    }
    // SAFETY: plain style bit twiddling on a window we own.
    unsafe {
        let h = HWND(hwnd as *mut core::ffi::c_void);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let add = (WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0) as isize;
        let remove = (WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0) as isize;
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | add) & !remove);
        let _ = SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub fn exclude_hwnd_from_capture(hwnd: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };
    if hwnd == 0 {
        return;
    }
    // SAFETY: `hwnd` is a valid top-level window handle owned by this process.
    let _ = unsafe { SetWindowDisplayAffinity(HWND(hwnd as *mut _), WDA_EXCLUDEFROMCAPTURE) };
}

/// Trigger the secure attention sequence (Ctrl+Alt+Del) via `SendSAS`.
///
/// Requires the `SoftwareSASGeneration` policy to allow services, and that the agent runs as
/// SYSTEM. Loaded dynamically because `sas.dll` is not present on all SKUs.
pub fn send_sas() -> Result<()> {
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    // SAFETY: LoadLibraryA/GetProcAddress with a static name; the function signature matches
    // `VOID SendSAS(BOOL AsUser)`.
    unsafe {
        let lib = LoadLibraryA(s!("sas.dll")).context("loading sas.dll")?;
        let proc = GetProcAddress(lib, s!("SendSAS"));
        let Some(proc) = proc else {
            bail!("SendSAS not found in sas.dll");
        };
        let send_sas: extern "system" fn(i32) = std::mem::transmute(proc);
        send_sas(0); // FALSE = called from a service
    }
    Ok(())
}

// ─── active-session process launch (used by the Windows service) ─────────────────────────

/// Spawn `command` (an argv already including the program path) in the active interactive
/// console session as the logged-in user, returning the process handle. Used by the service
/// to run `remote-agent run` on the user's desktop.
#[cfg(feature = "winservice")]
pub fn spawn_in_active_session(command_line: &str) -> Result<ActiveProcess> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::HANDLE as WHANDLE;
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityIdentification, TokenPrimary, TOKEN_ALL_ACCESS,
    };
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, CREATE_UNICODE_ENVIRONMENT, NORMAL_PRIORITY_CLASS,
        PROCESS_INFORMATION, STARTUPINFOW,
    };

    // SAFETY: sequence of Win32 calls each checked for failure; handles closed on the way out.
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == 0xFFFF_FFFF {
            bail!("no active console session");
        }
        let mut user_token = WHANDLE::default();
        WTSQueryUserToken(session, &mut user_token).context("WTSQueryUserToken")?;
        let _user_guard = HandleGuard(user_token);

        let mut primary = WHANDLE::default();
        DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityIdentification,
            TokenPrimary,
            &mut primary,
        )
        .context("DuplicateTokenEx")?;
        let primary_guard = HandleGuard(primary);

        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        CreateEnvironmentBlock(&mut env, Some(primary), false).context("CreateEnvironmentBlock")?;

        let mut cmd: Vec<u16> = command_line
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        let mut si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_ptr() as *mut u16),
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        let result = CreateProcessAsUserW(
            Some(primary),
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | NORMAL_PRIORITY_CLASS,
            Some(env),
            PCWSTR::null(),
            &si,
            &mut pi,
        );
        let _ = &mut si;
        DestroyEnvironmentBlock(env).ok();
        drop(primary_guard);
        result.context("CreateProcessAsUserW")?;
        let _ = CloseHandle(pi.hThread);
        Ok(ActiveProcess {
            process: pi.hProcess,
            session,
        })
    }
}

#[cfg(feature = "winservice")]
struct HandleGuard(HANDLE);

#[cfg(feature = "winservice")]
impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: closing a handle we own exactly once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// A process running in the active session.
#[cfg(feature = "winservice")]
pub struct ActiveProcess {
    pub process: HANDLE,
    pub session: u32,
}

#[cfg(feature = "winservice")]
impl ActiveProcess {
    /// Wait up to `timeout` for exit; returns true when the process has exited.
    pub fn wait(&self, timeout: Duration) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::WaitForSingleObject;
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        // SAFETY: valid process handle.
        unsafe { WaitForSingleObject(self.process, ms) == WAIT_OBJECT_0 }
    }

    pub fn terminate(&self) {
        use windows::Win32::System::Threading::TerminateProcess;
        // SAFETY: valid process handle; ignore errors (it may already be gone).
        unsafe {
            let _ = TerminateProcess(self.process, 1);
        }
    }
}

#[cfg(feature = "winservice")]
impl Drop for ActiveProcess {
    fn drop(&mut self) {
        if !self.process.is_invalid() {
            // SAFETY: closing our own handle once.
            unsafe {
                let _ = CloseHandle(self.process);
            }
        }
    }
}

#[allow(dead_code)]
fn _unused(_h: HANDLE, _e: anyhow::Error) {}

// ─── chat window ────────────────────────────────────────────────────────────────────────

use crate::chat::{ChatHandle, ChatLine};
use protocol::channel::ChatParty;
use std::path::PathBuf;
use std::sync::Mutex;
use windows::Win32::Foundation::{HGLOBAL, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Controls::EM_SETSEL;
use windows::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageExtraInfo,
    GetMessageW, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, PostMessageW,
    RegisterClassW, SendMessageW, SetWindowDisplayAffinity, SetWindowLongPtrW, SetWindowPos,
    SetWindowTextW, ShowWindow, TranslateMessage, CW_USEDEFAULT, ES_AUTOVSCROLL, ES_MULTILINE,
    ES_READONLY, GWLP_USERDATA, HMENU, HWND_TOPMOST, MSG, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE, WINDOW_EX_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WM_KEYFIRST, WM_KEYLAST, WM_MOUSEFIRST, WM_MOUSELAST, WM_USER, WNDCLASSW, WS_BORDER,
    WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_OVERLAPPED, WS_SYSMENU, WS_VISIBLE,
    WS_VSCROLL,
};

const ID_SEND: usize = 1001;
const ID_DISCONNECT: usize = 1002;
const ID_INPUT: usize = 1003;
const ID_TRANSCRIPT: usize = 1004;
/// Posted to the chat window to refresh the transcript from the shared text.
const WM_APP_REFRESH: u32 = WM_USER + 1;
const EN_RETURN_HACK: u32 = 0;

/// Callbacks of the session currently attached to the (single) chat window; `None` between
/// sessions, when typing / Disconnect are no-ops.
type SendCb = Arc<dyn Fn(String) + Send + Sync>;
type DisconnectCb = Arc<dyn Fn() + Send + Sync>;

struct ChatShared {
    callbacks: Mutex<Option<(SendCb, DisconnectCb)>>,
    text: Mutex<String>,
    hwnd: Mutex<Option<isize>>,
    transcript: Mutex<Option<isize>>,
    input: Mutex<Option<isize>>,
}

impl ChatShared {
    fn on_send(&self, text: String) {
        let cb = self
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(s, _)| Arc::clone(s));
        match cb {
            Some(cb) => cb(text),
            None => tracing::debug!("chat line typed but no session is attached"),
        }
    }
    fn on_disconnect(&self) {
        let cb = self
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(_, d)| Arc::clone(d));
        match cb {
            Some(cb) => cb(),
            None => tracing::debug!("disconnect pressed but no session is attached"),
        }
    }
}

/// The process-wide chat window (created on the first session, re-used afterwards).
static CHAT: Mutex<Option<Arc<ChatShared>>> = Mutex::new(None);

/// Whether `msg` is a mouse or keyboard input message (for the injected-input guard).
fn is_input_message(msg: u32) -> bool {
    (WM_MOUSEFIRST..=WM_MOUSELAST).contains(&msg) || (WM_KEYFIRST..=WM_KEYLAST).contains(&msg)
}

unsafe extern "system" fn chat_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds an `Arc<ChatShared>` pointer set at creation.
    let shared_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const ChatShared;
    // Reject remote-injected input so the operator can never operate the chat window; the local
    // user's real input carries no marker and passes through.
    if is_input_message(msg)
        && GetMessageExtraInfo().0 == crate::input::INJECTED_EVENT_MARKER as isize
    {
        return LRESULT(0);
    }
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as usize;
            if !shared_ptr.is_null() {
                let shared = &*shared_ptr;
                match id {
                    ID_SEND => {
                        if let Some(input) = *shared.input.lock().unwrap_or_else(|e| e.into_inner())
                        {
                            let input = HWND(input as *mut _);
                            let len = GetWindowTextLengthW(input) as usize;
                            let mut buf = vec![0u16; len + 1];
                            let n = GetWindowTextW(input, &mut buf) as usize;
                            let text = String::from_utf16_lossy(&buf[..n]);
                            if !text.trim().is_empty() {
                                let _ = SetWindowTextW(input, windows::core::w!(""));
                                shared.on_send(text);
                            }
                        }
                    }
                    ID_DISCONNECT => shared.on_disconnect(),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_APP_REFRESH => {
            if !shared_ptr.is_null() {
                let shared = &*shared_ptr;
                if let Some(t) = *shared.transcript.lock().unwrap_or_else(|e| e.into_inner()) {
                    let t = HWND(t as *mut _);
                    let text = shared
                        .text
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = SetWindowTextW(t, PCWSTR(wide.as_ptr()));
                    let len = text.encode_utf16().count();
                    SendMessageW(t, EM_SETSEL, Some(WPARAM(len)), Some(LPARAM(len as isize)));
                }
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE,
                );
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_DESTROY => {
            windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

struct ChatWindowHandle {
    shared: Arc<ChatShared>,
}

impl ChatHandle for ChatWindowHandle {
    fn push_line(&self, line: &ChatLine) {
        let who = match line.from {
            ChatParty::Operator => "Operator",
            ChatParty::Device => "You",
        };
        {
            let mut t = self.shared.text.lock().unwrap_or_else(|e| e.into_inner());
            if !t.is_empty() {
                t.push_str("\r\n");
            }
            t.push_str(&format!("{who}: {}", line.text));
        }
        if let Some(h) = *self.shared.hwnd.lock().unwrap_or_else(|e| e.into_inner()) {
            // SAFETY: posting to our own window.
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(h as *mut _)),
                    WM_APP_REFRESH,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    fn set_visible(&self, visible: bool) {
        if let Some(h) = *self.shared.hwnd.lock().unwrap_or_else(|e| e.into_inner()) {
            // SAFETY: valid window handle owned by our thread.
            unsafe {
                let _ = ShowWindow(
                    HWND(h as *mut _),
                    if visible { SW_SHOWNOACTIVATE } else { SW_HIDE },
                );
            }
        }
    }
}

impl Drop for ChatWindowHandle {
    fn drop(&mut self) {
        // Detach the session; the window itself lives for the whole process and is hidden.
        *self
            .shared
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        {
            let mut t = self.shared.text.lock().unwrap_or_else(|e| e.into_inner());
            if !t.is_empty() {
                t.push_str("\r\n");
            }
            t.push_str("— Session ended —");
        }
        self.set_visible(false);
    }
}

/// Create the chat window on its own UI thread.
pub fn open_chat(
    operator: &str,
    on_send: Arc<dyn Fn(String) + Send + Sync>,
    on_disconnect: Arc<dyn Fn() + Send + Sync>,
) -> Result<Box<dyn ChatHandle>> {
    let title = HSTRING::from(format!("Chat with {operator}"));
    // Re-use the existing window: new callbacks, empty transcript, new title.
    let existing = CHAT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(Arc::clone);
    if let Some(shared) = existing {
        *shared.callbacks.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((on_send, on_disconnect));
        shared
            .text
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        if let Some(h) = *shared.hwnd.lock().unwrap_or_else(|e| e.into_inner()) {
            // SAFETY: our own window; WM_SETTEXT is marshalled across threads.
            unsafe {
                let _ = SetWindowTextW(HWND(h as *mut _), &title);
            }
        }
        return Ok(Box::new(ChatWindowHandle { shared }));
    }
    let shared = Arc::new(ChatShared {
        callbacks: Mutex::new(Some((on_send, on_disconnect))),
        text: Mutex::new(String::new()),
        hwnd: Mutex::new(None),
        transcript: Mutex::new(None),
        input: Mutex::new(None),
    });
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let thread_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("chat-window".into())
        .spawn(move || {
            // SAFETY: classic Win32 window creation on a dedicated thread with its own loop.
            let result: Result<()> = unsafe {
                (|| {
                    let class_name = windows::core::w!("RemoteAgentChat");
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(chat_wndproc),
                        lpszClassName: class_name,
                        ..Default::default()
                    };
                    RegisterClassW(&wc); // may already be registered
                    let width = 380;
                    let height = 320;
                    let hwnd = CreateWindowExW(
                        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                        class_name,
                        &title,
                        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                        CW_USEDEFAULT,
                        CW_USEDEFAULT,
                        width,
                        height,
                        None,
                        None,
                        None,
                        None,
                    )
                    .context("CreateWindowExW")?;
                    let arc_ptr = Arc::as_ptr(&thread_shared);
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, arc_ptr as isize);
                    let edit_class = windows::core::w!("EDIT");
                    let button_class = windows::core::w!("BUTTON");
                    let transcript = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        edit_class,
                        None,
                        WS_CHILD
                            | WS_VISIBLE
                            | WS_BORDER
                            | WS_VSCROLL
                            | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                                (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32,
                            ),
                        8,
                        8,
                        width - 32,
                        height - 92,
                        Some(hwnd),
                        Some(HMENU(ID_TRANSCRIPT as *mut _)),
                        None,
                        None,
                    )
                    .context("transcript")?;
                    let input = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        edit_class,
                        None,
                        WS_CHILD | WS_VISIBLE | WS_BORDER,
                        8,
                        height - 76,
                        width - 32 - 70 - 100 - 12,
                        26,
                        Some(hwnd),
                        Some(HMENU(ID_INPUT as *mut _)),
                        None,
                        None,
                    )
                    .context("input")?;
                    let _send = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        button_class,
                        windows::core::w!("Send"),
                        WS_CHILD | WS_VISIBLE,
                        width - 32 - 100 - 6 - 70,
                        height - 76,
                        70,
                        26,
                        Some(hwnd),
                        Some(HMENU(ID_SEND as *mut _)),
                        None,
                        None,
                    )
                    .context("send button")?;
                    let _disc = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        button_class,
                        windows::core::w!("Disconnect"),
                        WS_CHILD | WS_VISIBLE,
                        width - 32 - 100,
                        height - 76,
                        100,
                        26,
                        Some(hwnd),
                        Some(HMENU(ID_DISCONNECT as *mut _)),
                        None,
                        None,
                    )
                    .context("disconnect button")?;
                    *thread_shared.hwnd.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(hwnd.0 as isize);
                    *thread_shared
                        .transcript
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(transcript.0 as isize);
                    *thread_shared
                        .input
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(input.0 as isize);
                    // Exclude our window from screen capture (Win10 2004+ / DXGI duplication);
                    // silently ignored on older builds.
                    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
                    // Created hidden: the session shows it on the first operator message.
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    Ok(())
                })()
            };
            let ok = result.is_ok();
            let _ = ready_tx.send(result);
            if !ok {
                return;
            }
            // SAFETY: standard message loop; WM_USER+2 destroys the window and ends the loop.
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_USER + 2 {
                        let _ = DestroyWindow(msg.hwnd);
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            let _ = EN_RETURN_HACK;
        })
        .context("spawning chat window thread")?;
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .context("chat window did not start")??;
    *CHAT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&shared));
    Ok(Box::new(ChatWindowHandle { shared }))
}

// ─── clipboard (CF_HDROP) ───────────────────────────────────────────────────────────────

pub fn clipboard_sequence() -> Option<u64> {
    // SAFETY: plain Win32 call.
    Some(unsafe { GetClipboardSequenceNumber() } as u64)
}

pub fn clipboard_files() -> Result<Vec<PathBuf>> {
    // SAFETY: standard clipboard access sequence.
    unsafe {
        if !IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
            return Ok(Vec::new());
        }
        OpenClipboard(None).context("OpenClipboard")?;
        let result = (|| -> Result<Vec<PathBuf>> {
            let handle = GetClipboardData(CF_HDROP.0 as u32).context("GetClipboardData")?;
            let hdrop = HDROP(handle.0);
            let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let n = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                out.push(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
            }
            Ok(out)
        })();
        let _ = CloseClipboard();
        result
    }
}

pub fn set_clipboard_files(paths: &[PathBuf]) -> Result<()> {
    // DROPFILES header followed by a double-NUL terminated list of wide strings.
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.as_os_str().encode_wide());
        list.push(0);
    }
    list.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let total = header + list.len() * 2;
    // SAFETY: standard HGLOBAL + clipboard sequence; ownership of the HGLOBAL passes to the
    // clipboard on success.
    unsafe {
        let hglobal: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, total).context("GlobalAlloc")?;
        let ptr = GlobalLock(hglobal) as *mut u8;
        if ptr.is_null() {
            bail!("GlobalLock failed");
        }
        let df = DROPFILES {
            pFiles: header as u32,
            pt: Default::default(),
            fNC: false.into(),
            fWide: true.into(),
        };
        std::ptr::write_unaligned(ptr as *mut DROPFILES, df);
        std::ptr::copy_nonoverlapping(list.as_ptr() as *const u8, ptr.add(header), list.len() * 2);
        let _ = GlobalUnlock(hglobal);
        OpenClipboard(None).context("OpenClipboard")?;
        let result = (|| -> Result<()> {
            EmptyClipboard().context("EmptyClipboard")?;
            SetClipboardData(CF_HDROP.0 as u32, Some(HANDLE(hglobal.0)))
                .context("SetClipboardData")?;
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

use std::os::windows::ffi::OsStrExt;
