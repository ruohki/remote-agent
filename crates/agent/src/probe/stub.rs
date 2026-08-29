//! Platforms without screen capture: the probe has nothing to measure.

use super::{AppWindow, LogicalRect, Rgb, Variant};
use crate::capture::{CaptureConfig, Capturer};
use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShieldStyle;

pub(super) fn beacon_style() -> ShieldStyle {
    ShieldStyle
}

pub(super) fn variants() -> Vec<Variant> {
    Vec::new()
}

pub(super) fn environment() -> Vec<(String, String)> {
    vec![(
        "platform".to_string(),
        "screen capture is not supported here".to_string(),
    )]
}

pub(super) struct Shield;

impl Shield {
    pub(super) fn create(_rect: LogicalRect, _colour: Rgb, _style: ShieldStyle) -> Result<Self> {
        bail!("not supported on this platform")
    }

    pub(super) fn window_id(&self) -> u64 {
        0
    }

    pub(super) fn set_visible(&self, _visible: bool) {}
}

pub(super) fn shield_note(_shield: &Shield) -> Option<String> {
    None
}

pub(super) fn create_capturer_excluding(
    _cfg: &CaptureConfig,
    _window_id: u64,
) -> Result<Box<dyn Capturer>> {
    bail!("not supported on this platform")
}

pub(super) fn app_windows() -> Vec<AppWindow> {
    Vec::new()
}
