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

impl AnnotateEvent {
    /// The display an event addresses, if any.
    pub fn display(&self) -> Option<u32> {
        match self {
            AnnotateEvent::Stroke { display, .. } | AnnotateEvent::Pointer { display, .. } => {
                Some(*display)
            }
            AnnotateEvent::End { .. } | AnnotateEvent::Clear => None,
        }
    }

    /// Map points from **encoded-picture pixels** (what the browser draws against, i.e. the
    /// video's `videoWidth × videoHeight`) to the display's physical pixels the overlay uses.
    /// The picture is smaller than the display when viewport-size encoding is active; the
    /// stroke width scales with the horizontal factor. A zero/unknown `encoded` size means
    /// "no scaling".
    pub fn scaled_to_display(self, display_px: (u32, u32), encoded: (u32, u32)) -> Self {
        let (fx, fy) = scale_factors(display_px, encoded);
        if (fx - 1.0).abs() < f32::EPSILON && (fy - 1.0).abs() < f32::EPSILON {
            return self;
        }
        match self {
            AnnotateEvent::Stroke {
                id,
                display,
                color,
                width,
                points,
            } => AnnotateEvent::Stroke {
                id,
                display,
                color,
                width: width * fx,
                points: points.into_iter().map(|(x, y)| (x * fx, y * fy)).collect(),
            },
            AnnotateEvent::Pointer {
                display,
                point,
                color,
            } => AnnotateEvent::Pointer {
                display,
                point: point.map(|(x, y)| (x * fx, y * fy)),
                color,
            },
            other => other,
        }
    }
}

/// `display / encoded` per axis; 1.0 when either size is unknown.
pub fn scale_factors(display_px: (u32, u32), encoded: (u32, u32)) -> (f32, f32) {
    if display_px.0 == 0 || display_px.1 == 0 || encoded.0 == 0 || encoded.1 == 0 {
        return (1.0, 1.0);
    }
    (
        display_px.0 as f32 / encoded.0 as f32,
        display_px.1 as f32 / encoded.1 as f32,
    )
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

#[cfg(test)]
mod scale_tests {
    use super::*;

    #[test]
    fn encoded_points_map_to_display_pixels() {
        // 1920×1080 display encoded at 960×540: (480, 270) → (960, 540).
        let ev = AnnotateEvent::Stroke {
            id: 1,
            display: 0,
            color: "#f00".into(),
            width: 3.0,
            points: vec![(480.0, 270.0)],
        };
        let AnnotateEvent::Stroke { points, width, .. } =
            ev.scaled_to_display((1920, 1080), (960, 540))
        else {
            unreachable!()
        };
        assert_eq!(points, vec![(960.0, 540.0)]);
        assert_eq!(width, 6.0);

        // 2× Retina: 5120×2160 physical, encoded 2560×1080: (1280, 540) → (2560, 1080) physical,
        // which the overlay's physical-pixel canvas draws at CSS (1280, 540) in a 2560×1080-point window.
        let ev = AnnotateEvent::Pointer {
            display: 0,
            point: Some((1280.0, 540.0)),
            color: "#0f0".into(),
        };
        let AnnotateEvent::Pointer { point, .. } = ev.scaled_to_display((5120, 2160), (2560, 1080))
        else {
            unreachable!()
        };
        assert_eq!(point, Some((2560.0, 1080.0)));
        let scale = 2.0f32;
        assert_eq!((2560.0 / scale, 1080.0 / scale), (1280.0, 540.0));

        // Full-resolution encoding or unknown size: unchanged.
        let ev = AnnotateEvent::Pointer {
            display: 0,
            point: Some((10.0, 20.0)),
            color: "#00f".into(),
        };
        let same = ev.clone().scaled_to_display((1920, 1080), (1920, 1080));
        assert_eq!(same, ev);
        let same = ev.clone().scaled_to_display((1920, 1080), (0, 0));
        assert_eq!(same, ev);
    }
}
