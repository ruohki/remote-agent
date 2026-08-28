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
