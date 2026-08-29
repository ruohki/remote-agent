//! macOS side of the privacy probe: plain AppKit sentinel windows in the configurations the
//! agent uses (and a privacy screen would use), process facts, and the windows this process
//! shows. Everything touching AppKit runs on the main thread through `run_on_main`.

use super::{AppWindow, Expect, LogicalRect, Rgb, Variant};
use crate::capture::{CaptureConfig, Capturer};
use crate::platform::run_on_main;
use anyhow::{Context, Result};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSFloatingWindowLevel, NSNormalWindowLevel,
    NSScreen, NSScreenSaverWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowSharingType,
    NSWindowStyleMask,
};
use objc2_core_graphics::CGShieldingWindowLevel;
use objc2_foundation::{NSPoint, NSProcessInfo, NSRect, NSSize, NSString};

/// Window level of a sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// `NSNormalWindowLevel` — the beacon.
    Normal,
    /// `NSFloatingWindowLevel` — an ordinary panel above documents.
    Floating,
    /// `NSScreenSaverWindowLevel - 1` — what the annotation overlay uses.
    Overlay,
    /// `CGShieldingWindowLevel()` — what a privacy screen would use.
    Shielding,
}

/// How a sentinel window is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShieldStyle {
    /// `sharingType = NSWindowSharingNone` — the agent's shipping exclusion.
    pub sharing_none: bool,
    pub level: Level,
    /// `CanJoinAllSpaces | FullScreenAuxiliary | Stationary | IgnoresCycle`, as the overlay.
    pub all_spaces: bool,
}

pub(super) fn beacon_style() -> ShieldStyle {
    ShieldStyle {
        sharing_none: false,
        level: Level::Normal,
        all_spaces: false,
    }
}

pub(super) fn variants() -> Vec<Variant> {
    let floating = |sharing_none| ShieldStyle {
        sharing_none,
        level: Level::Floating,
        all_spaces: false,
    };
    vec![
        Variant {
            name: "control",
            description: "plain window (sharingType readOnly); must be in the capture",
            expect: Expect::Visible,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: floating(false),
        },
        Variant {
            name: "sharing-none",
            description: "sharingType none, created after the stream started (the agent's own windows)",
            expect: Expect::Hidden,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: floating(true),
        },
        Variant {
            name: "sharing-none-before-stream",
            description: "sharingType none, created before the stream started",
            expect: Expect::Hidden,
            before_stream: true,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: floating(true),
        },
        Variant {
            name: "sharing-none-stream-rebuild",
            description: "sharingType none; the stream is stopped and re-created with the window up (select_display)",
            expect: Expect::Hidden,
            before_stream: false,
            rebuild_stream: true,
            exclude_in_filter: false,
            style: floating(true),
        },
        Variant {
            name: "overlay-level-none",
            description: "sharingType none at NSScreenSaverWindowLevel-1 on all Spaces (the annotation overlay recipe)",
            expect: Expect::Hidden,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: ShieldStyle {
                sharing_none: true,
                level: Level::Overlay,
                all_spaces: true,
            },
        },
        Variant {
            name: "shield-level-none",
            description: "sharingType none at CGShieldingWindowLevel on all Spaces (a privacy screen)",
            expect: Expect::Hidden,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: ShieldStyle {
                sharing_none: true,
                level: Level::Shielding,
                all_spaces: true,
            },
        },
        Variant {
            name: "shield-level-readonly",
            description: "sharingType readOnly at CGShieldingWindowLevel: does the capture see shielding-level windows at all?",
            expect: Expect::Observe,
            before_stream: false,
            rebuild_stream: false,
            exclude_in_filter: false,
            style: ShieldStyle {
                sharing_none: false,
                level: Level::Shielding,
                all_spaces: true,
            },
        },
        Variant {
            name: "filter-exclude-only",
            description: "sharingType readOnly, but the stream's SCContentFilter excludes the window (the documented mechanism)",
            expect: Expect::Hidden,
            before_stream: true,
            rebuild_stream: false,
            exclude_in_filter: true,
            style: floating(false),
        },
        Variant {
            name: "filter-exclude-plus-none",
            description: "sharingType none and excluded through the SCContentFilter (belt and braces)",
            expect: Expect::Hidden,
            before_stream: true,
            rebuild_stream: false,
            exclude_in_filter: true,
            style: floating(true),
        },
    ]
}

pub(super) fn environment() -> Vec<(String, String)> {
    let version = NSProcessInfo::processInfo()
        .operatingSystemVersionString()
        .to_string();
    vec![
        ("macOS".to_string(), version),
        (
            "shielding level".to_string(),
            format!(
                "{} (overlay level {})",
                CGShieldingWindowLevel(),
                NSScreenSaverWindowLevel - 1
            ),
        ),
    ]
}

/// A borderless, opaque, solid-colour window.
pub(super) struct Shield {
    /// Raw retained `NSWindow` (released on drop, on the main thread).
    addr: usize,
    id: u64,
}

// SAFETY: the pointer is only ever dereferenced on the main thread (`run_on_main`).
unsafe impl Send for Shield {}

impl Shield {
    pub(super) fn create(rect: LogicalRect, colour: Rgb, style: ShieldStyle) -> Result<Self> {
        let (addr, id) = run_on_main(move || -> Result<(usize, u64)> {
            let mtm = MainThreadMarker::new().context("not on the main thread")?;
            let frame = appkit_frame(mtm, rect)?;
            // SAFETY: standard NSWindow construction on the main thread with valid arguments.
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    frame,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            // SAFETY: we keep our own retain and release it on drop.
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str("privacy probe"));
            window.setOpaque(true);
            window.setHasShadow(false);
            window.setIgnoresMouseEvents(true);
            window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
                colour.r as f64 / 255.0,
                colour.g as f64 / 255.0,
                colour.b as f64 / 255.0,
                1.0,
            )));
            window.setLevel(match style.level {
                Level::Normal => NSNormalWindowLevel,
                Level::Floating => NSFloatingWindowLevel,
                Level::Overlay => NSScreenSaverWindowLevel - 1,
                Level::Shielding => CGShieldingWindowLevel() as isize,
            });
            if style.all_spaces {
                window.setCollectionBehavior(
                    NSWindowCollectionBehavior::CanJoinAllSpaces
                        | NSWindowCollectionBehavior::FullScreenAuxiliary
                        | NSWindowCollectionBehavior::Stationary
                        | NSWindowCollectionBehavior::IgnoresCycle,
                );
            }
            if style.sharing_none {
                window.setSharingType(NSWindowSharingType::None);
            }
            window.orderFrontRegardless();
            let id = window.windowNumber() as u64;
            Ok((Retained::into_raw(window) as usize, id))
        })??;
        Ok(Self { addr, id })
    }

    /// CoreGraphics window number (what `SCWindow.windowID` reports).
    pub(super) fn window_id(&self) -> u64 {
        self.id
    }

    pub(super) fn set_visible(&self, visible: bool) {
        let addr = self.addr;
        let _ = run_on_main(move || {
            // SAFETY: `addr` is our retained window; main thread.
            let window: &NSWindow = unsafe { &*(addr as *const NSWindow) };
            if visible {
                window.orderFrontRegardless();
            } else {
                window.orderOut(None);
            }
        });
    }
}

impl Drop for Shield {
    fn drop(&mut self) {
        let addr = self.addr;
        let _ = run_on_main(move || {
            // SAFETY: this is the retain taken in `create`; released exactly once, main thread.
            if let Some(window) = unsafe { Retained::from_raw(addr as *mut NSWindow) } {
                window.orderOut(None);
                window.close();
            }
        });
    }
}

/// Platform-specific facts about a sentinel worth putting in the note (none on macOS: the
/// window server has no readable per-window exclusion state we would trust).
pub(super) fn shield_note(_shield: &Shield) -> Option<String> {
    None
}

/// The capture with the sentinel excluded through the content filter.
pub(super) fn create_capturer_excluding(
    cfg: &CaptureConfig,
    window_id: u64,
) -> Result<Box<dyn Capturer>> {
    crate::capture::macos::create_excluding(cfg, &[window_id as u32])
}

/// Visible windows of this process with their frames in global logical coordinates.
pub(super) fn app_windows() -> Vec<AppWindow> {
    run_on_main(|| {
        let Some(mtm) = MainThreadMarker::new() else {
            return Vec::new();
        };
        let Some(primary_h) = primary_height(mtm) else {
            return Vec::new();
        };
        NSApplication::sharedApplication(mtm)
            .windows()
            .iter()
            .filter(|w| w.isVisible())
            .map(|w| {
                let f = w.frame();
                AppWindow {
                    id: w.windowNumber() as u64,
                    title: w.title().to_string(),
                    excluded: w.sharingType() == NSWindowSharingType::None,
                    rect: LogicalRect {
                        x: f.origin.x,
                        y: primary_h - f.origin.y - f.size.height,
                        w: f.size.width,
                        h: f.size.height,
                    },
                }
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Height of the primary screen in points — AppKit's origin is its bottom-left corner.
fn primary_height(mtm: MainThreadMarker) -> Option<f64> {
    NSScreen::screens(mtm)
        .firstObject()
        .map(|s| s.frame().size.height)
}

/// Global logical rectangle (y down) → AppKit frame (y up from the primary screen's bottom).
fn appkit_frame(mtm: MainThreadMarker, r: LogicalRect) -> Result<NSRect> {
    let h = primary_height(mtm).context("no screens")?;
    Ok(NSRect::new(
        NSPoint::new(r.x, h - r.y - r.h),
        NSSize::new(r.w, r.h),
    ))
}
