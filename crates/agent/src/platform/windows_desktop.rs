//! Following the *input desktop*.
//!
//! A Windows session has several desktops: `Default` (the user's), `Winlogon` (sign-in screen,
//! lock screen, Ctrl+Alt+Del menu, UAC's secure desktop) and `Screen-saver`. Only threads
//! attached to the desktop that currently receives input can duplicate its output
//! (`DuplicateOutput` → `E_ACCESSDENIED` otherwise), inject input into it (`SendInput` is
//! silently dropped) or read its cursor. The SYSTEM session host therefore re-attaches the
//! threads that do that work whenever the input desktop changes.
//!
//! `SetThreadDesktop` refuses threads that own windows or hooks; the capture, pipeline,
//! cursor and input threads own neither.

use std::cell::RefCell;
use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};

thread_local! {
    /// The desktop this thread was last attached to (handle kept open while attached).
    static ATTACHED: RefCell<Option<(HDESK, String)>> = const { RefCell::new(None) };
    /// Last desktop a switch failed for (log each failure once, not per frame).
    static FAILED: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Last failure to open the input desktop at all.
    static OPEN_FAILED: RefCell<bool> = const { RefCell::new(false) };
}

/// Bumped whenever any thread of this process moved to another desktop. The video pipeline
/// watches it: across a desktop switch (sign-in, lock, UAC, Ctrl+Alt+Del) the picture changes
/// completely and the first keyframe is large, so it is sent twice.
static SWITCH_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn switch_epoch() -> u64 {
    SWITCH_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

fn thread_name() -> String {
    std::thread::current().name().unwrap_or("?").to_string()
}

/// Name of a desktop handle (`Default`, `Winlogon`, …).
fn desktop_name(h: HDESK) -> Option<String> {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    // SAFETY: the buffer is writable for its full byte length.
    unsafe {
        GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(h.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut _),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    }
    .ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Name of the desktop that receives input right now, if this process may open it.
pub fn input_desktop_name() -> Option<String> {
    // SAFETY: the handle is closed before returning.
    unsafe {
        let h = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        )
        .ok()?;
        let name = desktop_name(h);
        let _ = CloseDesktop(h);
        name
    }
}

/// Attach the calling thread to the current input desktop if it is not already. Returns the
/// desktop name when the thread switched, `None` when nothing changed or switching failed
/// (not SYSTEM, the thread owns windows, the desktop is mid-switch).
pub fn follow_input_desktop() -> Option<String> {
    // SAFETY: OpenInputDesktop returns an owned handle; it is either kept (attached) or closed.
    let h = match unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        )
    } {
        Ok(h) => {
            OPEN_FAILED.with(|f| *f.borrow_mut() = false);
            h
        }
        Err(e) => {
            OPEN_FAILED.with(|f| {
                if !*f.borrow() {
                    tracing::warn!(thread = %thread_name(), "OpenInputDesktop: {e}");
                    *f.borrow_mut() = true;
                }
            });
            return None;
        }
    };
    let Some(name) = desktop_name(h) else {
        // SAFETY: closing the handle we just opened.
        unsafe {
            let _ = CloseDesktop(h);
        }
        return None;
    };
    ATTACHED.with(|cell| {
        let mut cur = cell.borrow_mut();
        if cur.as_ref().map(|(_, n)| n == &name).unwrap_or(false) {
            // SAFETY: closing the duplicate handle; the attached one stays open.
            unsafe {
                let _ = CloseDesktop(h);
            }
            return None;
        }
        // SAFETY: `h` is a valid desktop handle; the previous one is closed only after the
        // thread left it.
        match unsafe { SetThreadDesktop(h) } {
            Ok(()) => {
                let from = cur.as_ref().map(|(_, n)| n.clone());
                if let Some((old, _)) = cur.replace((h, name.clone())) {
                    unsafe {
                        let _ = CloseDesktop(old);
                    }
                }
                FAILED.with(|f| *f.borrow_mut() = None);
                if from.is_some() {
                    SWITCH_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                tracing::info!(thread = %thread_name(), ?from, to = %name, "thread follows the input desktop");
                Some(name)
            }
            Err(e) => {
                FAILED.with(|f| {
                    let mut f = f.borrow_mut();
                    if f.as_deref() != Some(name.as_str()) {
                        tracing::warn!(thread = %thread_name(), desktop = %name, "SetThreadDesktop: {e}");
                        *f = Some(name.clone());
                    }
                });
                unsafe {
                    let _ = CloseDesktop(h);
                }
                None
            }
        }
    })
}
