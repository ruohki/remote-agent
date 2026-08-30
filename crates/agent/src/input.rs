//! Mouse and keyboard injection for [`protocol::channel::InputEvent`].
//!
//! * W3C `KeyboardEvent.code` values are translated by [`map_code`]: letters, digits and
//!   punctuation become *characters* (so the remote machine's keyboard layout decides which
//!   physical key is pressed), everything else maps to a named [`enigo::Key`].
//! * Mouse positions arrive in physical pixels of the currently selected display and are
//!   converted to global logical coordinates with [`to_global`].
//! * Every pressed key/button is tracked so [`InputHandler::release_all`] (sent by the
//!   browser on blur, and called on session teardown) never leaves keys stuck.

use anyhow::{anyhow, Context, Result};
#[cfg(target_os = "windows")]
use anyhow::bail;
use enigo::{Axis, Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
#[cfg(not(target_os = "windows"))]
use enigo::Coordinate;
use protocol::channel::{InputEvent, MouseButton};
use protocol::common::DisplayInfo;
use std::collections::{HashMap, HashSet};

/// Marker stamped on every injected event so the agent's own windows can recognise and drop
/// remote input (macOS: `kCGEventSourceUserData`; Windows: `dwExtraInfo`). "RMTINJ1".
pub const INJECTED_EVENT_MARKER: i64 = 0x52_4d_54_49_4e_4a_31;

/// Something that can apply input events (the real injector or a test double).
pub trait InputHandler: Send {
    /// Display the operator is looking at, and the size of the encoded picture the browser's
    /// mouse coordinates refer to (the encoder may downscale very large displays).
    fn set_display(&mut self, display: &DisplayInfo, video_size: (u32, u32));
    fn handle(&mut self, event: InputEvent) -> Result<()>;
    /// Release every key and button currently held.
    fn release_all(&mut self);
}

/// Result of mapping a W3C key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTarget {
    /// A named key (modifier, navigation, function key…).
    Named(Key),
    /// A character; the remote layout decides which physical key produces it.
    Char(char),
}

/// Translate a `KeyboardEvent.code` to something enigo can press. `None` for keys that have
/// no sensible equivalent on this platform.
pub fn map_code(code: &str) -> Option<KeyTarget> {
    use KeyTarget::*;

    if let Some(letter) = code.strip_prefix("Key") {
        let mut chars = letter.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_uppercase() {
                return Some(Char(c.to_ascii_lowercase()));
            }
        }
    }
    if let Some(digit) = code.strip_prefix("Digit") {
        let mut chars = digit.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if c.is_ascii_digit() {
                return Some(Char(c));
            }
        }
    }
    if let Some(n) = code.strip_prefix("Numpad") {
        let mut chars = n.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if let Some(d) = c.to_digit(10) {
                return Some(Named(numpad_key(d)));
            }
        }
    }
    if let Some(n) = code.strip_prefix('F') {
        if let Ok(num) = n.parse::<u8>() {
            return function_key(num).map(Named);
        }
    }

    let named = match code {
        "Minus" => return Some(Char('-')),
        "Equal" => return Some(Char('=')),
        "BracketLeft" => return Some(Char('[')),
        "BracketRight" => return Some(Char(']')),
        "Backslash" => return Some(Char('\\')),
        "Semicolon" => return Some(Char(';')),
        "Quote" => return Some(Char('\'')),
        "Backquote" => return Some(Char('`')),
        "Comma" => return Some(Char(',')),
        "Period" => return Some(Char('.')),
        "Slash" => return Some(Char('/')),
        "IntlBackslash" => return Some(Char('<')),
        "NumpadEqual" => return Some(Char('=')),

        "Space" => Key::Space,
        "Enter" | "NumpadEnter" => Key::Return,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "ArrowUp" => Key::UpArrow,
        "ArrowDown" => Key::DownArrow,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowRight" => Key::RightArrow,
        "CapsLock" => Key::CapsLock,
        "Help" => Key::Help,
        "ShiftLeft" => Key::LShift,
        "ShiftRight" => Key::RShift,
        "ControlLeft" => Key::LControl,
        "ControlRight" => Key::RControl,
        "AltLeft" => Key::Alt,
        "NumpadAdd" => Key::Add,
        "NumpadSubtract" => Key::Subtract,
        "NumpadMultiply" => Key::Multiply,
        "NumpadDivide" => Key::Divide,
        "NumpadDecimal" => Key::Decimal,
        "AudioVolumeUp" => Key::VolumeUp,
        "AudioVolumeDown" => Key::VolumeDown,
        "AudioVolumeMute" => Key::VolumeMute,
        "MediaTrackNext" => Key::MediaNextTrack,
        "MediaTrackPrevious" => Key::MediaPrevTrack,
        "MediaPlayPause" => Key::MediaPlayPause,

        #[cfg(target_os = "macos")]
        "AltRight" => Key::ROption,
        #[cfg(target_os = "windows")]
        "AltRight" => Key::RMenu,
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        "AltRight" => Key::Alt,

        #[cfg(target_os = "windows")]
        "MetaLeft" => Key::LWin,
        #[cfg(target_os = "windows")]
        "MetaRight" => Key::RWin,
        #[cfg(target_os = "macos")]
        "MetaRight" => Key::RCommand,
        #[cfg(not(target_os = "windows"))]
        "MetaLeft" => Key::Meta,
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        "MetaRight" => Key::Meta,

        #[cfg(target_os = "windows")]
        "ContextMenu" => Key::Apps,
        #[cfg(not(target_os = "macos"))]
        "NumLock" => Key::Numlock,
        #[cfg(not(target_os = "macos"))]
        "Insert" => Key::Insert,
        #[cfg(not(target_os = "macos"))]
        "PrintScreen" => Key::PrintScr,
        #[cfg(not(target_os = "macos"))]
        "Pause" => Key::Pause,
        #[cfg(all(unix, not(target_os = "macos")))]
        "ScrollLock" => Key::ScrollLock,
        #[cfg(target_os = "windows")]
        "BrowserBack" => Key::BrowserBack,
        #[cfg(target_os = "windows")]
        "BrowserForward" => Key::BrowserForward,
        #[cfg(target_os = "macos")]
        "Eject" => Key::Eject,
        #[cfg(target_os = "macos")]
        "Power" => Key::Power,
        #[cfg(target_os = "macos")]
        "Fn" => Key::Function,

        _ => return None,
    };
    Some(Named(named))
}

fn numpad_key(d: u32) -> Key {
    match d {
        0 => Key::Numpad0,
        1 => Key::Numpad1,
        2 => Key::Numpad2,
        3 => Key::Numpad3,
        4 => Key::Numpad4,
        5 => Key::Numpad5,
        6 => Key::Numpad6,
        7 => Key::Numpad7,
        8 => Key::Numpad8,
        _ => Key::Numpad9,
    }
}

fn function_key(n: u8) -> Option<Key> {
    Some(match n {
        1 => Key::F1,
        2 => Key::F2,
        3 => Key::F3,
        4 => Key::F4,
        5 => Key::F5,
        6 => Key::F6,
        7 => Key::F7,
        8 => Key::F8,
        9 => Key::F9,
        10 => Key::F10,
        11 => Key::F11,
        12 => Key::F12,
        13 => Key::F13,
        14 => Key::F14,
        15 => Key::F15,
        16 => Key::F16,
        17 => Key::F17,
        18 => Key::F18,
        19 => Key::F19,
        20 => Key::F20,
        #[cfg(not(target_os = "macos"))]
        21 => Key::F21,
        #[cfg(not(target_os = "macos"))]
        22 => Key::F22,
        #[cfg(not(target_os = "macos"))]
        23 => Key::F23,
        #[cfg(not(target_os = "macos"))]
        24 => Key::F24,
        _ => return None,
    })
}

/// Convert a position in pixels of the encoded picture (`video_size`) shown for `display`
/// to global logical coordinates: rescale to the display's physical pixels, add the display
/// origin and divide by the backing scale.
pub fn to_global(display: &DisplayInfo, video_size: (u32, u32), x: i32, y: i32) -> (i32, i32) {
    let scale = if display.scale > 0.0 {
        display.scale
    } else {
        1.0
    };
    let (vw, vh) = video_size;
    let sx = if vw > 0 && display.width > 0 {
        display.width as f32 / vw as f32
    } else {
        1.0
    };
    let sy = if vh > 0 && display.height > 0 {
        display.height as f32 / vh as f32
    } else {
        1.0
    };
    let lx = display.x as f32 + (x as f32 * sx) / scale;
    let ly = display.y as f32 + (y as f32 * sy) / scale;
    (lx.round() as i32, ly.round() as i32)
}

/// Normalise a virtual-desktop point to the 0..65535 space `SendInput` expects with
/// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`. `rect` is the virtual desktop as
/// `(left, top, width, height)`; points outside it are clamped to its edge.
///
/// This exists because `enigo`'s absolute move normalises against the *primary* monitor and
/// omits `MOUSEEVENTF_VIRTUALDESK`, so a coordinate on any other monitor lands clamped to the
/// primary — the pointer sticks to an edge instead of following the operator.
pub fn virtual_desktop_absolute(x: i32, y: i32, rect: (i32, i32, i32, i32)) -> (i32, i32) {
    let (left, top, width, height) = rect;
    let axis = |v: i32, origin: i32, span: i32| -> i32 {
        let span = (span - 1).max(1) as i64;
        let offset = (v - origin) as i64;
        let scaled = (offset * 65535 + span / 2) / span;
        scaled.clamp(0, 65535) as i32
    };
    (axis(x, left, width), axis(y, top, height))
}

/// Move the pointer to a point in *virtual desktop* pixels. Windows only: every other platform
/// is served correctly by `enigo`.
#[cfg(target_os = "windows")]
fn move_pointer_absolute(x: i32, y: i32) -> Result<()> {
    use ::windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
    };
    use ::windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    // SAFETY: plain Win32 metric queries.
    let rect = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if rect.2 <= 0 || rect.3 <= 0 {
        bail!("virtual desktop has no size");
    }
    let (nx, ny) = virtual_desktop_absolute(x, y, rect);
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: nx,
                dy: ny,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                // The agent's own windows drop input carrying this marker.
                dwExtraInfo: INJECTED_EVENT_MARKER as usize,
            },
        },
    };
    // SAFETY: one fully initialised INPUT record.
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    if sent == 0 {
        bail!("SendInput moved no pointer");
    }
    Ok(())
}

fn map_button(b: MouseButton) -> Button {
    match b {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
        MouseButton::Back => Button::Back,
        MouseButton::Forward => Button::Forward,
    }
}

/// The real injector built on `enigo`.
pub struct Injector {
    enigo: Enigo,
    display: Option<DisplayInfo>,
    video_size: (u32, u32),
    keys_down: HashMap<String, KeyTarget>,
    buttons_down: HashSet<MouseButton>,
    wheel_accum: (f32, f32),
    enabled: bool,
}

impl Injector {
    pub fn new() -> Result<Self> {
        let settings = Settings {
            release_keys_when_dropped: true,
            // Tag injected events so `platform` can drop remote input aimed at our own
            // windows (the operator must never be able to click the chat/banner).
            event_source_user_data: Some(INJECTED_EVENT_MARKER),
            windows_dw_extra_info: Some(INJECTED_EVENT_MARKER as usize),
            ..Settings::default()
        };
        let enigo =
            Enigo::new(&settings).map_err(|e| anyhow!("initialising input injection: {e}"))?;
        Ok(Self {
            enigo,
            display: None,
            video_size: (0, 0),
            keys_down: HashMap::new(),
            buttons_down: HashSet::new(),
            wheel_accum: (0.0, 0.0),
            enabled: true,
        })
    }

    /// Gate for `AgentConfig.allow_input`; when disabled events are silently ignored.
    pub fn set_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.release_all();
        }
        self.enabled = enabled;
    }

    fn press(&mut self, target: KeyTarget, direction: Direction) -> Result<()> {
        let key = match target {
            KeyTarget::Named(k) => k,
            KeyTarget::Char(c) => Key::Unicode(c),
        };
        self.enigo
            .key(key, direction)
            .map_err(|e| anyhow!("key {key:?} {direction:?}: {e}"))
    }
}

impl InputHandler for Injector {
    fn set_display(&mut self, display: &DisplayInfo, video_size: (u32, u32)) {
        self.display = Some(display.clone());
        self.video_size = video_size;
    }

    fn handle(&mut self, event: InputEvent) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        match event {
            InputEvent::MouseMove { x, y } => {
                let display = self
                    .display
                    .as_ref()
                    .context("no display selected for input")?;
                let (gx, gy) = to_global(display, self.video_size, x, y);
                #[cfg(target_os = "windows")]
                move_pointer_absolute(gx, gy)?;
                #[cfg(not(target_os = "windows"))]
                self.enigo
                    .move_mouse(gx, gy, Coordinate::Abs)
                    .map_err(|e| anyhow!("move_mouse: {e}"))?;
            }
            InputEvent::MouseDown { button } => {
                self.enigo
                    .button(map_button(button), Direction::Press)
                    .map_err(|e| anyhow!("button press: {e}"))?;
                self.buttons_down.insert(button);
            }
            InputEvent::MouseUp { button } => {
                self.enigo
                    .button(map_button(button), Direction::Release)
                    .map_err(|e| anyhow!("button release: {e}"))?;
                self.buttons_down.remove(&button);
            }
            InputEvent::MouseWheel { dx, dy } => {
                self.wheel_accum.0 += dx;
                self.wheel_accum.1 += dy;
                let steps_x = self.wheel_accum.0.trunc() as i32;
                let steps_y = self.wheel_accum.1.trunc() as i32;
                if steps_y != 0 {
                    self.enigo
                        .scroll(steps_y, Axis::Vertical)
                        .map_err(|e| anyhow!("scroll: {e}"))?;
                    self.wheel_accum.1 -= steps_y as f32;
                }
                if steps_x != 0 {
                    self.enigo
                        .scroll(steps_x, Axis::Horizontal)
                        .map_err(|e| anyhow!("scroll: {e}"))?;
                    self.wheel_accum.0 -= steps_x as f32;
                }
            }
            InputEvent::KeyDown { code } => {
                if let Some(target) = map_code(&code) {
                    self.press(target, Direction::Press)?;
                    self.keys_down.insert(code, target);
                } else {
                    tracing::debug!("unmapped key code {code}");
                }
            }
            InputEvent::KeyUp { code } => {
                let target = self.keys_down.remove(&code).or_else(|| map_code(&code));
                if let Some(target) = target {
                    self.press(target, Direction::Release)?;
                }
            }
            InputEvent::Text { text } => {
                self.enigo.text(&text).map_err(|e| anyhow!("text: {e}"))?;
            }
            InputEvent::ReleaseAll => self.release_all(),
        }
        Ok(())
    }

    fn release_all(&mut self) {
        let keys: Vec<KeyTarget> = self.keys_down.drain().map(|(_, t)| t).collect();
        for target in keys {
            if let Err(e) = self.press(target, Direction::Release) {
                tracing::debug!("release_all key: {e:#}");
            }
        }
        let buttons: Vec<MouseButton> = self.buttons_down.drain().collect();
        for b in buttons {
            if let Err(e) = self.enigo.button(map_button(b), Direction::Release) {
                tracing::debug!("release_all button: {e}");
            }
        }
        self.wheel_accum = (0.0, 0.0);
    }
}

/// Last-wins slot for pointer moves arriving on the unreliable `input-fast` channel: the
/// applier takes the newest position at the injector rate, stale ones are dropped, and the
/// reliable channel drains it before a button event so clicks land where the operator sees
/// the pointer.
#[derive(Debug, Default)]
pub struct LatestMove {
    slot: parking_lot::Mutex<Option<(i32, i32)>>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

impl LatestMove {
    pub fn push(&self, x: i32, y: i32) {
        *self.slot.lock() = Some((x, y));
        self.notify.notify_one();
    }

    /// Take the newest pending position, if any.
    pub fn take(&self) -> Option<(i32, i32)> {
        self.slot.lock().take()
    }

    pub fn notify(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.notify)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_desktop_absolute_spans_every_monitor() {
        // Two 1920x1080 monitors side by side, primary on the left.
        let rect = (0, 0, 3840, 1080);
        assert_eq!(virtual_desktop_absolute(0, 0, rect), (0, 0));
        assert_eq!(virtual_desktop_absolute(3839, 1079, rect), (65535, 65535));
        // A point on the *second* monitor lands in the upper half of the range. Normalising
        // against the primary alone (what enigo does) would overflow past 65535 and clamp the
        // pointer to the right edge of the primary instead.
        // 2880 is three quarters across the desktop, i.e. the middle of the second monitor.
        assert_eq!(virtual_desktop_absolute(2880, 540, rect), (49164, 32798));

        // A monitor left of and above the primary gives the desktop a negative origin.
        let rect = (-1920, -120, 3840, 1200);
        assert_eq!(virtual_desktop_absolute(-1920, -120, rect), (0, 0));
        assert_eq!(virtual_desktop_absolute(0, 0, rect), (32776, 6559));
        // Anything outside the desktop is clamped rather than wrapping to the far side.
        assert_eq!(virtual_desktop_absolute(-5000, -5000, rect), (0, 0));
        assert_eq!(virtual_desktop_absolute(99_999, 99_999, rect), (65535, 65535));
    }

    #[test]
    fn latest_move_keeps_only_the_newest_position() {
        let m = LatestMove::default();
        assert_eq!(m.take(), None);
        m.push(1, 1);
        m.push(2, 2);
        m.push(3, 3);
        assert_eq!(m.take(), Some((3, 3)));
        assert_eq!(m.take(), None);
    }

    #[test]
    fn letters_and_digits_become_chars() {
        assert_eq!(map_code("KeyA"), Some(KeyTarget::Char('a')));
        assert_eq!(map_code("KeyZ"), Some(KeyTarget::Char('z')));
        assert_eq!(map_code("Digit7"), Some(KeyTarget::Char('7')));
        assert_eq!(map_code("Semicolon"), Some(KeyTarget::Char(';')));
        assert_eq!(map_code("Key"), None);
        assert_eq!(map_code("KeyAB"), None);
    }

    #[test]
    fn named_keys() {
        assert_eq!(map_code("Enter"), Some(KeyTarget::Named(Key::Return)));
        assert_eq!(map_code("NumpadEnter"), Some(KeyTarget::Named(Key::Return)));
        assert_eq!(map_code("ShiftLeft"), Some(KeyTarget::Named(Key::LShift)));
        assert_eq!(map_code("F12"), Some(KeyTarget::Named(Key::F12)));
        assert_eq!(map_code("Numpad5"), Some(KeyTarget::Named(Key::Numpad5)));
        assert_eq!(
            map_code("ArrowLeft"),
            Some(KeyTarget::Named(Key::LeftArrow))
        );
        assert_eq!(map_code("Unidentified"), None);
        assert_eq!(map_code("F99"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_specific_modifiers() {
        assert_eq!(map_code("MetaLeft"), Some(KeyTarget::Named(Key::Meta)));
        assert_eq!(map_code("MetaRight"), Some(KeyTarget::Named(Key::RCommand)));
        assert_eq!(map_code("AltRight"), Some(KeyTarget::Named(Key::ROption)));
        assert_eq!(map_code("ContextMenu"), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_specific_modifiers() {
        assert_eq!(map_code("MetaLeft"), Some(KeyTarget::Named(Key::LWin)));
        assert_eq!(map_code("ContextMenu"), Some(KeyTarget::Named(Key::Apps)));
        assert_eq!(
            map_code("PrintScreen"),
            Some(KeyTarget::Named(Key::PrintScr))
        );
    }

    #[test]
    fn coordinates_add_origin_and_divide_by_scale() {
        let d = DisplayInfo {
            index: 1,
            name: "ext".into(),
            x: 1440,
            y: -200,
            width: 3840,
            height: 2160,
            scale: 2.0,
            primary: false,
        };
        let full = (3840, 2160);
        assert_eq!(to_global(&d, full, 0, 0), (1440, -200));
        assert_eq!(to_global(&d, full, 3840, 2160), (1440 + 1920, -200 + 1080));
        assert_eq!(to_global(&d, full, 101, 0), (1440 + 51, -200));
        // encoder downscaled the picture to half size: browser coordinates are doubled first
        assert_eq!(
            to_global(&d, (1920, 1080), 1920, 1080),
            (1440 + 1920, -200 + 1080)
        );
        // unknown video size → treated as 1:1
        assert_eq!(to_global(&d, (0, 0), 200, 0), (1440 + 100, -200));
        let d1 = DisplayInfo { scale: 0.0, ..d };
        assert_eq!(to_global(&d1, full, 10, 10), (1450, -190));
    }

    #[test]
    fn injection_marker_is_stable_and_nonzero() {
        // Both platform fields carry it: our own windows drop events whose marker matches
        // (macOS EVENT_SOURCE_USER_DATA / Windows dwExtraInfo). It must never be 0 (the default
        // for real user input) so local input is never mistaken for injected input.
        assert_ne!(INJECTED_EVENT_MARKER, 0);
        assert_eq!(INJECTED_EVENT_MARKER as usize as i64, INJECTED_EVENT_MARKER);
    }
}
