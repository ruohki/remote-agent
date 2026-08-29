//! Windows side of the privacy probe: sentinel windows in the display-affinity variants,
//! process facts (build, integrity level, session) and the windows this process shows.
//!
//! Each sentinel is a plain `WS_POPUP` window on its own thread with its own message loop,
//! painted a solid colour; the variants differ in `SetWindowDisplayAffinity`, `WS_EX_LAYERED`
//! and the undocumented `SetWindowCompositionAttribute(WCA_EXCLUDED_FROM_DDA)`, which is the
//! one flag Microsoft ships specifically for desktop duplication. Only depends on the
//! `windows` crate so it can be type-checked on a non-Windows host.

use super::{AppWindow, Expect, LogicalRect, Rgb, Variant};
use crate::capture::{CaptureConfig, Capturer};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::mpsc;
use std::time::Duration;
use windows::core::{s, w, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, COLORREF, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, PAINTSTRUCT,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, GetMessageW,
    GetSystemMetrics, GetWindowDisplayAffinity, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, PostQuitMessage, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowLongPtrW, ShowWindow,
    TranslateMessage, GWLP_USERDATA, LWA_ALPHA, MSG, SM_REMOTESESSION, SW_HIDE, SW_SHOWNOACTIVATE,
    WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR, WDA_NONE, WINDOW_DISPLAY_AFFINITY, WINDOW_EX_STYLE,
    WM_DESTROY, WM_ERASEBKGND, WM_PAINT, WM_USER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

/// How a sentinel window is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShieldStyle {
    /// `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` — the agent's shipping exclusion.
    pub affinity_exclude: bool,
    /// `WS_EX_LAYERED` + `SetLayeredWindowAttributes(alpha 255)` before the affinity call
    /// (the shape Electron settled on after non-layered windows degraded to black).
    pub layered: bool,
    /// `SetWindowCompositionAttribute(WCA_EXCLUDED_FROM_DDA)` — undocumented, Windows 10 1709+.
    pub wca_dda: bool,
    /// `WS_EX_TOPMOST` (the beacon is not, so it stays underneath).
    pub topmost: bool,
}

/// `WINDOWCOMPOSITIONATTRIB` value for `WCA_EXCLUDED_FROM_DDA`.
const WCA_EXCLUDED_FROM_DDA: u32 = 24;
/// Posted to a sentinel's thread to destroy the window and end its loop.
const WM_APP_DESTROY: u32 = WM_USER + 7;

pub(super) fn beacon_style() -> ShieldStyle {
    ShieldStyle {
        affinity_exclude: false,
        layered: false,
        wca_dda: false,
        topmost: false,
    }
}

pub(super) fn variants() -> Vec<Variant> {
    let style = |affinity_exclude, layered, wca_dda| ShieldStyle {
        affinity_exclude,
        layered,
        wca_dda,
        topmost: true,
    };
    vec![
        Variant {
            name: "control",
            description: "plain topmost window, no affinity; must be in the capture",
            expect: Expect::Visible,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: style(false, false, false),
        },
        Variant {
            name: "affinity-exclude",
            description:
                "WDA_EXCLUDEFROMCAPTURE on a non-layered window (the agent's shipping call)",
            expect: Expect::Observe,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: style(true, false, false),
        },
        Variant {
            name: "affinity-exclude-layered",
            description: "WS_EX_LAYERED (alpha 255) set before WDA_EXCLUDEFROMCAPTURE",
            expect: Expect::Observe,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: style(true, true, false),
        },
        Variant {
            name: "wca-excluded-from-dda",
            description: "SetWindowCompositionAttribute(WCA_EXCLUDED_FROM_DDA) only",
            expect: Expect::Observe,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: style(false, false, true),
        },
        Variant {
            name: "affinity-layered-plus-wca",
            description: "layered + WDA_EXCLUDEFROMCAPTURE + WCA_EXCLUDED_FROM_DDA",
            expect: Expect::Observe,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: style(true, true, true),
        },
        Variant {
            name: "affinity-exclude-layered-before-stream",
            description: "layered + WDA_EXCLUDEFROMCAPTURE, window created before the duplication",
            expect: Expect::Observe,
            before_stream: true,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: style(true, true, false),
        },
        Variant {
            name: "affinity-exclude-layered-rebuild",
            description:
                "layered + WDA_EXCLUDEFROMCAPTURE; the duplication is re-created with the window up",
            expect: Expect::Observe,
            before_stream: false,
            rebuild_stream: true,
            exclude_in_filter: false,
            style: style(true, true, false),
        },
    ]
}

pub(super) fn environment() -> Vec<(String, String)> {
    let mut out = Vec::new();
    out.push((
        "Windows build".to_string(),
        format!(
            "{} (UBR {}, {})",
            registry_string("CurrentBuildNumber").unwrap_or_else(|| "?".into()),
            registry_dword("UBR")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into()),
            registry_string("DisplayVersion").unwrap_or_else(|| "?".into())
        ),
    ));
    out.push((
        "integrity level".to_string(),
        integrity_level().unwrap_or_else(|e| format!("unknown ({e})")),
    ));
    let mut session = 0u32;
    // SAFETY: plain query for our own process id.
    let session = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
        .map(|_| session.to_string())
        .unwrap_or_else(|_| "?".into());
    out.push(("session".to_string(), session));
    // SAFETY: plain metric query.
    let remote = unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0;
    out.push((
        "remote session".to_string(),
        if remote { "yes (RDP)" } else { "no" }.to_string(),
    ));
    out.push((
        "WCA_EXCLUDED_FROM_DDA".to_string(),
        if set_window_composition_attribute_fn().is_some() {
            "SetWindowCompositionAttribute available"
        } else {
            "SetWindowCompositionAttribute NOT available"
        }
        .to_string(),
    ));
    out
}

fn registry_string(name: &str) -> Option<String> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let sub = HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let name = HSTRING::from(name);
    let mut buf = [0u16; 128];
    let mut size: u32 = (buf.len() * 2) as u32;
    // SAFETY: the buffer is sized as advertised; the strings outlive the call.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn registry_dword(name: &str) -> Option<u32> {
    use windows::core::HSTRING;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
    let sub = HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let name = HSTRING::from(name);
    let mut value: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: the out-buffer is a u32 sized as advertised; the strings outlive the call.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(value)
}

/// Mandatory integrity level of this process (decides whether low-level hooks see input to
/// elevated windows, and whether `SendSAS` can work).
fn integrity_level() -> Result<String> {
    // SAFETY: token query on our own process; buffer sized by the first call.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("OpenProcessToken")?;
        let _guard = TokenGuard(token);
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed);
        if needed == 0 {
            bail!("GetTokenInformation size query failed");
        }
        let mut buf = vec![0u8; needed as usize];
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr() as *mut _),
            needed,
            &mut needed,
        )
        .context("GetTokenInformation")?;
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            bail!("integrity SID has no sub-authorities");
        }
        let rid = *GetSidSubAuthority(sid, (count - 1) as u32);
        let name = match rid {
            0..=0x0fff => "untrusted",
            0x1000..=0x1fff => "low",
            0x2000..=0x2fff => "medium",
            0x3000..=0x3fff => "high",
            _ => "system",
        };
        Ok(format!("{name} (0x{rid:x})"))
    }
}

struct TokenGuard(HANDLE);
impl Drop for TokenGuard {
    fn drop(&mut self) {
        // SAFETY: closing a handle we opened exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[repr(C)]
struct WindowCompositionAttribData {
    attrib: u32,
    data: *mut core::ffi::c_void,
    size: usize,
}

type SetWcaFn = unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> BOOL;

fn set_window_composition_attribute_fn() -> Option<SetWcaFn> {
    // SAFETY: user32 is always loaded; the signature matches the undocumented export.
    unsafe {
        let user32 = GetModuleHandleW(w!("user32.dll")).ok()?;
        let proc = GetProcAddress(user32, s!("SetWindowCompositionAttribute"))?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            SetWcaFn,
        >(proc))
    }
}

fn exclude_from_dda(hwnd: HWND) -> Result<()> {
    let f = set_window_composition_attribute_fn()
        .ok_or_else(|| anyhow!("SetWindowCompositionAttribute is not exported by user32"))?;
    let mut enabled = BOOL(1);
    let mut data = WindowCompositionAttribData {
        attrib: WCA_EXCLUDED_FROM_DDA,
        data: &mut enabled as *mut BOOL as *mut _,
        size: std::mem::size_of::<BOOL>(),
    };
    // SAFETY: valid window; the data block outlives the call.
    if !unsafe { f(hwnd, &mut data) }.as_bool() {
        bail!("SetWindowCompositionAttribute(WCA_EXCLUDED_FROM_DDA) failed");
    }
    Ok(())
}

// ─── sentinel window ─────────────────────────────────────────────────────────────────────

unsafe extern "system" fn shield_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let colour = COLORREF(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32);
            let brush = CreateSolidBrush(colour);
            let _ = FillRect(hdc, &ps.rcPaint, brush);
            let _ = DeleteObject(brush.into());
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub(super) struct Shield {
    hwnd: isize,
    thread: Option<std::thread::JoinHandle<()>>,
    affinity: WINDOW_DISPLAY_AFFINITY,
}

impl Shield {
    pub(super) fn create(rect: LogicalRect, colour: Rgb, style: ShieldStyle) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(isize, u32)>>();
        let thread = std::thread::Builder::new()
            .name("privacy-probe-window".into())
            .spawn(move || {
                let result = (|| -> Result<(isize, u32)> {
                    // SAFETY: standard window creation on this thread; every call checked.
                    unsafe {
                        let class_name = w!("RemoteAgentPrivacyProbe");
                        let wc = WNDCLASSW {
                            lpfnWndProc: Some(shield_wndproc),
                            lpszClassName: class_name,
                            ..Default::default()
                        };
                        RegisterClassW(&wc); // may already be registered
                        let mut ex = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
                        if style.topmost {
                            ex |= WS_EX_TOPMOST;
                        }
                        if style.layered {
                            ex |= WS_EX_LAYERED;
                        }
                        let hwnd = CreateWindowExW(
                            ex,
                            class_name,
                            w!("privacy probe"),
                            WS_POPUP,
                            rect.x.round() as i32,
                            rect.y.round() as i32,
                            rect.w.round() as i32,
                            rect.h.round() as i32,
                            None,
                            None,
                            None,
                            None,
                        )
                        .context("CreateWindowExW")?;
                        let rgb =
                            colour.r as u32 | (colour.g as u32) << 8 | (colour.b as u32) << 16;
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, rgb as isize);
                        if style.layered {
                            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)
                                .context("SetLayeredWindowAttributes")?;
                        }
                        if style.wca_dda {
                            exclude_from_dda(hwnd)?;
                        }
                        if style.affinity_exclude {
                            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)
                                .context("SetWindowDisplayAffinity")?;
                        }
                        let mut affinity: u32 = 0;
                        let _ = GetWindowDisplayAffinity(hwnd, &mut affinity);
                        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        Ok((hwnd.0 as isize, affinity))
                    }
                })();
                let ok = result.is_ok();
                let _ = ready_tx.send(result);
                if !ok {
                    return;
                }
                // SAFETY: standard message loop; WM_APP_DESTROY destroys the window, whose
                // WM_DESTROY posts the quit that ends the loop.
                unsafe {
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        if msg.message == WM_APP_DESTROY {
                            let _ = DestroyWindow(HWND(msg.wParam.0 as *mut _));
                            continue;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })
            .context("spawning the probe window thread")?;
        let (hwnd, affinity) = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .context("probe window did not start")??;
        Ok(Self {
            hwnd,
            thread: Some(thread),
            affinity: WINDOW_DISPLAY_AFFINITY(affinity),
        })
    }

    pub(super) fn window_id(&self) -> u64 {
        self.hwnd as u64
    }

    pub(super) fn set_visible(&self, visible: bool) {
        // SAFETY: valid window handle owned by our thread; ShowWindow may be called cross-thread.
        unsafe {
            let _ = ShowWindow(
                HWND(self.hwnd as *mut _),
                if visible { SW_SHOWNOACTIVATE } else { SW_HIDE },
            );
        }
    }
}

impl Drop for Shield {
    fn drop(&mut self) {
        // SAFETY: the window thread owns the HWND; it destroys it on this message. Posting to
        // the thread (not the window) so it also works if creation half-failed.
        unsafe {
            if let Some(t) = self.thread.as_ref() {
                let tid = thread_id_of(t);
                let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                    tid,
                    WM_APP_DESTROY,
                    WPARAM(self.hwnd as usize),
                    LPARAM(0),
                );
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Win32 thread id of a std thread.
fn thread_id_of(t: &std::thread::JoinHandle<()>) -> u32 {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::System::Threading::GetThreadId;
    // SAFETY: the join handle wraps a valid thread handle.
    unsafe { GetThreadId(HANDLE(t.as_raw_handle())) }
}

/// What `GetWindowDisplayAffinity` claims — recorded because it is known to disagree with
/// what the capture shows.
pub(super) fn shield_note(shield: &Shield) -> Option<String> {
    let name = match shield.affinity {
        WDA_NONE => "WDA_NONE",
        WDA_MONITOR => "WDA_MONITOR",
        WDA_EXCLUDEFROMCAPTURE => "WDA_EXCLUDEFROMCAPTURE",
        _ => "?",
    };
    Some(format!("affinity readback {name}"))
}

/// No filter-based exclusion exists for desktop duplication.
pub(super) fn create_capturer_excluding(
    _cfg: &CaptureConfig,
    _window_id: u64,
) -> Result<Box<dyn Capturer>> {
    bail!("desktop duplication has no per-window exclusion filter")
}

/// Visible top-level windows of this process (screen coordinates → global logical).
pub(super) fn app_windows() -> Vec<AppWindow> {
    let mut out: Vec<AppWindow> = Vec::new();
    // SAFETY: the callback only runs during the call and writes into `out`.
    unsafe {
        let _ = EnumWindows(Some(enum_callback), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

unsafe extern "system" fn enum_callback(hwnd: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the `*mut Vec<AppWindow>` passed by `app_windows`.
    let out = &mut *(data.0 as *mut Vec<AppWindow>);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != GetCurrentProcessId() || !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    let mut affinity: u32 = 0;
    let _ = GetWindowDisplayAffinity(hwnd, &mut affinity);
    let affinity = WINDOW_DISPLAY_AFFINITY(affinity);
    let mut title = [0u16; 128];
    let n = GetWindowTextW(hwnd, &mut title) as usize;
    out.push(AppWindow {
        id: hwnd.0 as u64,
        title: String::from_utf16_lossy(&title[..n.min(title.len())]),
        excluded: affinity == WDA_EXCLUDEFROMCAPTURE,
        rect: LogicalRect {
            x: rect.left as f64,
            y: rect.top as f64,
            w: (rect.right - rect.left) as f64,
            h: (rect.bottom - rect.top) as f64,
        },
    });
    BOOL(1)
}

#[allow(dead_code)]
fn _unused(_: WINDOW_EX_STYLE, _: PCWSTR) {}
