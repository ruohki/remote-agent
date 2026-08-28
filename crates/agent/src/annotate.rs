//! Operator screen annotations ("draw on the user's screen to guide them").
//!
//! The session receives [`protocol::channel::ControlMessage`] annotation messages on the
//! control channel and forwards them to an [`AnnotationSink`]; the app implements the sink
//! with transparent click-through overlay windows (see `app::annotate`), tests use a
//! recording fake, and the headless service mode uses [`NoAnnotations`] (which makes the
//! session answer `AnnotationsDisabled`).
//!
//! Annotations are deliberately independent of the keyboard/mouse permission: they keep
//! working while remote control is disabled by policy or paused by the device user, because
//! guiding someone by pointing is exactly what those situations call for.

use std::sync::Arc;

/// One annotation instruction, in the physical pixel space of `display`.
#[derive(Debug, Clone, PartialEq)]
pub enum AnnotateEvent {
    /// Start or continue a stroke (`points` are appended in order).
    Stroke {
        id: u32,
        display: u32,
        color: String,
        width: f32,
        points: Vec<(f32, f32)>,
    },
    /// The stroke is finished; the fade-out timer starts.
    End { id: u32 },
    /// Laser pointer position; `None` hides it.
    Pointer {
        display: u32,
        point: Option<(f32, f32)>,
        color: String,
    },
    /// Remove everything on every display.
    Clear,
}

/// Where the session sends annotation instructions.
pub trait AnnotationSink: Send + Sync {
    /// Whether annotations can be shown at all (false without a UI loop).
    fn available(&self) -> bool;
    fn apply(&self, event: AnnotateEvent);
    /// The session ended (or annotations were disabled): remove overlays.
    fn session_ended(&self);
}

/// Sink for headless runs: nothing can be drawn.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAnnotations;

impl AnnotationSink for NoAnnotations {
    fn available(&self) -> bool {
        false
    }

    fn apply(&self, _event: AnnotateEvent) {}

    fn session_ended(&self) {}
}

/// Convenience alias used by [`crate::session::SessionDeps`].
pub type SharedSink = Arc<dyn AnnotationSink>;

/// Clamp a CSS colour to something safe to interpolate into the overlay page: `#rgb`,
/// `#rrggbb`, `#rrggbbaa` or a few named colours; anything else falls back to red.
pub fn sanitize_color(color: &str) -> String {
    let c = color.trim();
    let hex_ok = c.starts_with('#')
        && matches!(c.len(), 4 | 7 | 9)
        && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if hex_ok {
        return c.to_ascii_lowercase();
    }
    match c.to_ascii_lowercase().as_str() {
        "red" | "green" | "blue" | "yellow" | "orange" | "white" | "black" | "magenta" | "cyan" => {
            c.to_ascii_lowercase()
        }
        _ => "#ff3b30".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_color_accepts_hex_and_names_only() {
        assert_eq!(sanitize_color("#FF0000"), "#ff0000");
        assert_eq!(sanitize_color("#f00"), "#f00");
        assert_eq!(sanitize_color("#ff000080"), "#ff000080");
        assert_eq!(sanitize_color("Yellow"), "yellow");
        assert_eq!(sanitize_color("url(javascript:alert(1))"), "#ff3b30");
        assert_eq!(sanitize_color("#zzz"), "#ff3b30");
    }

    #[test]
    fn no_annotations_is_unavailable() {
        assert!(!NoAnnotations.available());
    }
}
