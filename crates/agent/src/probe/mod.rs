//! `remote-agent privacy-probe`: do this agent's own windows — and a would-be privacy
//! screen — stay out of the capture the operator sees, on *this* machine and OS build?
//!
//! Every exclusion mechanism the agent relies on is either undocumented for the capture API in
//! use (Windows: `WDA_EXCLUDEFROMCAPTURE` versus DXGI desktop duplication, whose documentation
//! never mentions display affinity) or documented as legacy (macOS: Apple now describes
//! `NSWindowSharingNone` as a constant the system no longer uses). API return values do not
//! settle the question — `GetWindowDisplayAffinity` is known to report the affinity while the
//! window is captured anyway. Frames do.
//!
//! So the probe paints a solid magenta *sentinel* window on a display, runs the agent's real
//! capture pipeline for that display, and measures what fraction of the sentinel's pixels arrive
//! in the captured frames. Each test self-validates with a cyan *beacon* window placed inside
//! the sentinel's rectangle at a lower window level: on the physical screen the sentinel covers
//! it, so in the capture exactly one of the two colours shows up — magenta means the sentinel
//! leaked, cyan means it was excluded and the capture is live. Neither means the capture did
//! not composite this process's windows at all (a sandbox, a different session), which is
//! reported as *inconclusive* rather than as success. Variants cover the mechanisms the agent
//! ships (created after the stream started, at the overlay's window level), the ones a privacy
//! screen would add (shield level, filter-based exclusion, stream rebuilds) and, unless
//! `--skip-app`, the real session bar and annotation overlay of the running app.
//!
//! The report is printed as a table (or JSON with `--json`) and always written to the log
//! directory. Windows are shown for a second or two per test; nothing persists.

use crate::capture::{self, CaptureConfig, Capturer};
use crate::config::Paths;
use anyhow::{bail, Context, Result};
use protocol::common::DisplayInfo;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as platform;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod stub;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use stub as platform;

pub use platform::ShieldStyle;

/// Command line options of `privacy-probe`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeOptions {
    /// Only probe this display index.
    pub display: Option<u32>,
    /// Print JSON instead of the table.
    pub json: bool,
    /// Skip the session bar / annotation overlay tests.
    pub skip_app: bool,
}

/// A solid colour the probe paints and looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// The window under test.
pub const SENTINEL: Rgb = Rgb {
    r: 255,
    g: 0,
    b: 255,
};
/// The always-capturable change trigger underneath it.
pub const BEACON: Rgb = Rgb {
    r: 0,
    g: 255,
    b: 255,
};
/// Annotation stroke colour (same hue as the sentinel; drawn by the real overlay).
const STROKE: &str = "#ff00ff";

/// Per-channel tolerance when matching a colour (wide-gamut displays and colour-space
/// conversion shift pure colours slightly).
const TOLERANCE: u8 = 40;
/// Fraction of matching pixels above which a colour counts as present.
const PRESENT: f32 = 0.5;
/// Fraction below which a colour counts as absent.
const ABSENT: f32 = 0.05;

/// Let windows appear / the compositor catch up before measuring.
const SETTLE: Duration = Duration::from_millis(600);
/// How long to watch frames for one measurement.
const WATCH: Duration = Duration::from_millis(1800);
/// Pause between tests so the previous windows are gone from the screen.
const COOLDOWN: Duration = Duration::from_millis(400);
/// Time for the app's session bar / overlay page to load and paint.
const APP_SETTLE: Duration = Duration::from_millis(1800);

/// Rectangle in the capture's pixel space of one display (origin top-left, y down).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Rectangle in global logical coordinates — origin at the top-left of the primary display,
/// y down, the space `DisplayInfo.x` / `y` use. Platforms convert to their own window
/// coordinates (AppKit flips y).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl PixelRect {
    fn to_logical(self, d: &DisplayInfo) -> LogicalRect {
        let s = scale_of(d);
        LogicalRect {
            x: d.x as f64 + self.x as f64 / s,
            y: d.y as f64 + self.y as f64 / s,
            w: self.w as f64 / s,
            h: self.h as f64 / s,
        }
    }

    /// The part of a logical rectangle that lies on `d`, in `d`'s pixels; `None` if outside.
    fn from_logical(r: LogicalRect, d: &DisplayInfo) -> Option<PixelRect> {
        let s = scale_of(d);
        let x0 = ((r.x - d.x as f64) * s).round().max(0.0);
        let y0 = ((r.y - d.y as f64) * s).round().max(0.0);
        let x1 = ((r.x + r.w - d.x as f64) * s).round().min(d.width as f64);
        let y1 = ((r.y + r.h - d.y as f64) * s).round().min(d.height as f64);
        if x1 - x0 < 8.0 || y1 - y0 < 8.0 {
            return None;
        }
        Some(PixelRect {
            x: x0 as u32,
            y: y0 as u32,
            w: (x1 - x0) as u32,
            h: (y1 - y0) as u32,
        })
    }

    fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

fn scale_of(d: &DisplayInfo) -> f64 {
    if d.scale > 0.0 {
        d.scale as f64
    } else {
        1.0
    }
}

/// What a test expects to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Expect {
    /// The window must show up in the capture (harness control).
    Visible,
    /// The window must stay out of the capture.
    Hidden,
    /// No documented behaviour to hold it to; record what happens.
    Observe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Inconclusive,
    Skipped,
}

/// One test: how the sentinel window is set up and how the capture is driven around it.
pub(crate) struct Variant {
    pub name: &'static str,
    pub description: &'static str,
    pub expect: Expect,
    /// Create the sentinel before the capture stream starts (the agent's own windows appear
    /// after it, which is the default ordering).
    pub before_stream: bool,
    /// Stop and re-create the capture with the sentinel up — what `select_display` does — and
    /// report the second stream's result.
    pub rebuild_stream: bool,
    /// Build the capture with a content filter that excludes the sentinel's window (implies
    /// `before_stream`).
    pub exclude_in_filter: bool,
    pub style: ShieldStyle,
}

/// A visible window of this process, as the platform reports it.
pub(crate) struct AppWindow {
    pub id: u64,
    pub title: String,
    pub rect: LogicalRect,
    /// The platform's own exclusion flag on the window (`sharingType`, display affinity).
    pub excluded: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TestResult {
    pub display: u32,
    pub name: String,
    pub description: String,
    pub expect: Expect,
    /// Frames delivered by the capture during the measurement.
    pub frames: u32,
    /// Fraction of the measured area matching the sentinel in the last frame.
    pub sentinel: f32,
    /// Fraction of the beacon area matching the beacon in the last frame.
    pub beacon: f32,
    /// Frames in which the sentinel was present (transient leaks show up here).
    pub sentinel_frames: u32,
    /// `Some(true)` = the window was in the capture, `Some(false)` = it was not, `None` =
    /// could not tell.
    pub seen: Option<bool>,
    pub verdict: Verdict,
    pub note: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Summary {
    pub passed: u32,
    pub failed: u32,
    pub inconclusive: u32,
    pub observed: u32,
    pub skipped: u32,
}

impl Summary {
    /// 0 clean, 2 when a `Hidden` / `Visible` expectation failed, 3 when nothing could be
    /// measured (the capture never showed this process's windows).
    pub fn exit_code(&self) -> i32 {
        if self.failed > 0 {
            2
        } else if self.passed == 0 && self.observed == 0 && self.inconclusive > 0 {
            3
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub agent_version: String,
    pub os: String,
    pub environment: Vec<(String, String)>,
    pub displays: Vec<DisplayInfo>,
    pub results: Vec<TestResult>,
    pub summary: Summary,
}

/// Entry point from the CLI: run the probe behind the real application loop (the same
/// window / tray / main-thread environment `remote-agent run` has) and exit with its code.
pub fn run_in_app(paths: &Paths, opts: ProbeOptions) -> Result<()> {
    if std::env::var_os("REMOTE_AGENT_SHOW_WINDOWS").is_some() {
        bail!(
            "REMOTE_AGENT_SHOW_WINDOWS is set, which disables capture exclusion for every agent \
             window; the probe would only measure that. Unset it and run again"
        );
    }
    crate::app::set_state_dir(paths.dir.clone());
    let paths = paths.clone();
    let code = crate::app::run(
        move || match run(&paths, opts) {
            Ok(summary) => summary.exit_code(),
            Err(e) => {
                eprintln!("privacy-probe failed: {e:#}");
                tracing::error!("privacy-probe failed: {e:#}");
                1
            }
        },
        crate::app::AppOptions {
            show_on_start: false,
            installable: false,
        },
    );
    if code == 0 {
        Ok(())
    } else {
        bail!("privacy-probe exited with code {code}")
    }
}

/// Run every test, print the report, write it to the log directory.
pub fn run(paths: &Paths, opts: ProbeOptions) -> Result<Summary> {
    // The app loop starts the worker on its first event; give the main thread a moment so
    // `run_on_main` finds it pumping.
    std::thread::sleep(Duration::from_millis(300));

    let mut environment = vec![
        (
            "capture exclusion".to_string(),
            "enabled (REMOTE_AGENT_SHOW_WINDOWS unset)".to_string(),
        ),
        (
            "app loop".to_string(),
            if crate::platform::main_loop_running() {
                "running".to_string()
            } else {
                "NOT running — windows cannot be created".to_string()
            },
        ),
        (
            "screen recording".to_string(),
            if crate::platform::screen_capture_allowed() {
                "granted".to_string()
            } else {
                "NOT granted".to_string()
            },
        ),
        (
            "accessibility".to_string(),
            if crate::platform::accessibility_allowed() {
                "granted".to_string()
            } else {
                "not granted".to_string()
            },
        ),
    ];
    environment.extend(platform::environment());

    let displays = capture::list_displays().context("listing displays")?;
    if displays.is_empty() {
        bail!("no displays visible to this process (Screen Recording permission?)");
    }
    let targets: Vec<&DisplayInfo> = displays
        .iter()
        .filter(|d| opts.display.is_none_or(|i| d.index == i))
        .collect();
    if targets.is_empty() {
        bail!("display {} not found", opts.display.unwrap_or_default());
    }

    let mut results = Vec::new();
    for d in &targets {
        for v in platform::variants() {
            if crate::shutdown::requested() {
                break;
            }
            let started = Instant::now();
            let r = match run_variant(d, &v) {
                Ok(r) => r,
                Err(e) => TestResult {
                    display: d.index,
                    name: v.name.to_string(),
                    description: v.description.to_string(),
                    expect: v.expect,
                    frames: 0,
                    sentinel: 0.0,
                    beacon: 0.0,
                    sentinel_frames: 0,
                    seen: None,
                    verdict: Verdict::Inconclusive,
                    note: format!("error: {e:#}"),
                },
            };
            tracing::info!(
                display = d.index,
                test = v.name,
                verdict = ?r.verdict,
                seen = ?r.seen,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "probe"
            );
            results.push(r);
            std::thread::sleep(COOLDOWN);
        }
        if !opts.skip_app && !crate::shutdown::requested() {
            match run_session_bar(d) {
                Ok(rs) => results.extend(rs),
                Err(e) => results.push(skipped(d, "session-bar", &format!("error: {e:#}"))),
            }
            std::thread::sleep(COOLDOWN);
            match run_annotation_overlay(d) {
                Ok(r) => results.push(r),
                Err(e) => results.push(skipped(d, "annotation-overlay", &format!("error: {e:#}"))),
            }
            std::thread::sleep(COOLDOWN);
        }
    }

    let mut summary = Summary::default();
    for r in &results {
        match (r.verdict, r.expect) {
            (Verdict::Pass, Expect::Observe) => summary.observed += 1,
            (Verdict::Pass, _) => summary.passed += 1,
            (Verdict::Fail, _) => summary.failed += 1,
            (Verdict::Inconclusive, _) => summary.inconclusive += 1,
            (Verdict::Skipped, _) => summary.skipped += 1,
        }
    }
    let report = Report {
        agent_version: crate::AGENT_VERSION.to_string(),
        os: format!(
            "{:?} / {:?}",
            protocol::common::Os::current(),
            protocol::common::Arch::current()
        ),
        environment,
        displays: displays.clone(),
        results,
        summary: summary.clone(),
    };

    let json = serde_json::to_string_pretty(&report)?;
    if opts.json {
        println!("{json}");
    } else {
        print_table(&report);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = paths.log_dir().join(format!("privacy-probe-{stamp}.json"));
    match std::fs::create_dir_all(paths.log_dir()).and_then(|_| std::fs::write(&path, &json)) {
        Ok(()) => println!("report written to {}", path.display()),
        Err(e) => println!("report not written to {}: {e}", path.display()),
    }
    Ok(summary)
}

// ─── one sentinel test ───────────────────────────────────────────────────────────────────

/// Where the sentinel and the beacon go on a display: the middle third, with the beacon in
/// the sentinel's upper-left quarter.
fn layout(d: &DisplayInfo) -> (PixelRect, PixelRect) {
    let w = (d.width / 3).max(64);
    let h = (d.height / 3).max(64);
    let sentinel = PixelRect {
        x: (d.width - w) / 2,
        y: (d.height - h) / 2,
        w,
        h,
    };
    let beacon = PixelRect {
        x: sentinel.x + w / 8,
        y: sentinel.y + h / 8,
        w: (w / 4).max(16),
        h: (h / 4).max(16),
    };
    (sentinel, beacon)
}

fn capture_config(d: &DisplayInfo) -> CaptureConfig {
    CaptureConfig {
        display_index: d.index,
        max_fps: 30,
        show_cursor: false,
    }
}

fn run_variant(d: &DisplayInfo, v: &Variant) -> Result<TestResult> {
    let (sentinel_px, beacon_px) = layout(d);
    let cfg = capture_config(d);
    let before = v.before_stream || v.exclude_in_filter;

    let mut shield = if before {
        Some(platform::Shield::create(
            sentinel_px.to_logical(d),
            SENTINEL,
            v.style,
        )?)
    } else {
        None
    };
    let mut cap = if v.exclude_in_filter {
        let id = shield.as_ref().map(|s| s.window_id()).unwrap_or_default();
        platform::create_capturer_excluding(&cfg, id)?
    } else {
        capture::create_capturer(&cfg)?
    };
    if shield.is_none() {
        shield = Some(platform::Shield::create(
            sentinel_px.to_logical(d),
            SENTINEL,
            v.style,
        )?);
    }
    let beacon =
        platform::Shield::create(beacon_px.to_logical(d), BEACON, platform::beacon_style())?;
    std::thread::sleep(SETTLE);
    let mut m = measure(&mut cap, sentinel_px, beacon_px, WATCH);
    let mut note = String::new();

    if v.rebuild_stream {
        let first = m.clone();
        cap.stop();
        drop(cap);
        cap = capture::create_capturer(&cfg)?;
        beacon.set_visible(false);
        std::thread::sleep(Duration::from_millis(120));
        beacon.set_visible(true);
        std::thread::sleep(SETTLE);
        m = measure(&mut cap, sentinel_px, beacon_px, WATCH);
        note = format!(
            "first stream: sentinel {:.0}% / beacon {:.0}% in {} frames; ",
            first.sentinel * 100.0,
            first.beacon * 100.0,
            first.frames
        );
    }
    if let Some(extra) = platform::shield_note(shield.as_ref().expect("shield")) {
        note.push_str(&extra);
        note.push_str("; ");
    }
    if let Some(err) = &m.error {
        note.push_str(&format!("capture error: {err}; "));
    }

    drop(beacon);
    drop(shield);
    cap.stop();
    drop(cap);

    Ok(judge(d, v.name, v.description, v.expect, &m, note))
}

fn skipped(d: &DisplayInfo, name: &str, note: &str) -> TestResult {
    TestResult {
        display: d.index,
        name: name.to_string(),
        description: String::new(),
        expect: Expect::Hidden,
        frames: 0,
        sentinel: 0.0,
        beacon: 0.0,
        sentinel_frames: 0,
        seen: None,
        verdict: Verdict::Skipped,
        note: note.to_string(),
    }
}

#[derive(Debug, Clone, Default)]
struct Measurement {
    frames: u32,
    sentinel: f32,
    beacon: f32,
    sentinel_frames: u32,
    /// The last frame delivered, for follow-up diffs.
    last: Option<Bgra>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct Bgra {
    data: Vec<u8>,
    stride: usize,
    width: u32,
    height: u32,
}

/// Watch frames for up to `watch`, tracking the sentinel fraction over `sentinel_px` (minus
/// the beacon's area) and the beacon fraction over `beacon_px`. Stops early once a frame is
/// decisive either way.
fn measure(
    cap: &mut Box<dyn Capturer>,
    sentinel_px: PixelRect,
    beacon_px: PixelRect,
    watch: Duration,
) -> Measurement {
    let deadline = Instant::now() + watch;
    let mut m = Measurement::default();
    let mut decisive_since: Option<Instant> = None;
    while Instant::now() < deadline {
        match cap.next_frame(Duration::from_millis(150)) {
            Ok(Some(frame)) => {
                let Ok((data, stride)) = capture::to_bgra(&frame) else {
                    continue;
                };
                let bgra = Bgra {
                    data,
                    stride,
                    width: frame.width,
                    height: frame.height,
                };
                m.frames += 1;
                m.sentinel = colour_fraction(&bgra, sentinel_px, Some(beacon_px), SENTINEL);
                m.beacon = colour_fraction(&bgra, beacon_px, None, BEACON);
                if m.sentinel >= PRESENT {
                    m.sentinel_frames += 1;
                }
                m.last = Some(bgra);
                // Decisive: hold for a few more frames (or 300 ms) so a transient does not decide.
                if m.sentinel >= PRESENT || m.beacon >= PRESENT {
                    let since = *decisive_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_millis(300) {
                        break;
                    }
                } else {
                    decisive_since = None;
                }
            }
            Ok(None) => {}
            Err(e) => {
                m.error = Some(format!("{e:#}"));
                break;
            }
        }
    }
    m
}

/// Fraction of pixels in `rect` (excluding `hole`) within [`TOLERANCE`] of `colour`.
fn colour_fraction(f: &Bgra, rect: PixelRect, hole: Option<PixelRect>, colour: Rgb) -> f32 {
    let mut hit = 0u64;
    let mut total = 0u64;
    let x1 = (rect.x + rect.w).min(f.width);
    let y1 = (rect.y + rect.h).min(f.height);
    for y in rect.y..y1 {
        let row = &f.data[y as usize * f.stride..];
        for x in rect.x..x1 {
            if hole.is_some_and(|h| h.contains(x, y)) {
                continue;
            }
            let o = x as usize * 4;
            if o + 3 > row.len() {
                break;
            }
            let (b, g, r) = (row[o], row[o + 1], row[o + 2]);
            total += 1;
            if r.abs_diff(colour.r) <= TOLERANCE
                && g.abs_diff(colour.g) <= TOLERANCE
                && b.abs_diff(colour.b) <= TOLERANCE
            {
                hit += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        hit as f32 / total as f32
    }
}

/// Fraction of pixels in `rect` (excluding `hole`) that differ noticeably between two frames.
fn changed_fraction(a: &Bgra, b: &Bgra, rect: PixelRect, hole: Option<PixelRect>) -> f32 {
    if (a.width, a.height) != (b.width, b.height) {
        return 0.0;
    }
    let mut hit = 0u64;
    let mut total = 0u64;
    let x1 = (rect.x + rect.w).min(a.width);
    let y1 = (rect.y + rect.h).min(a.height);
    for y in rect.y..y1 {
        let ra = &a.data[y as usize * a.stride..];
        let rb = &b.data[y as usize * b.stride..];
        for x in rect.x..x1 {
            if hole.is_some_and(|h| h.contains(x, y)) {
                continue;
            }
            let o = x as usize * 4;
            if o + 3 > ra.len() || o + 3 > rb.len() {
                break;
            }
            total += 1;
            let delta = ra[o]
                .abs_diff(rb[o])
                .max(ra[o + 1].abs_diff(rb[o + 1]))
                .max(ra[o + 2].abs_diff(rb[o + 2]));
            if delta > 48 {
                hit += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        hit as f32 / total as f32
    }
}

fn judge(
    d: &DisplayInfo,
    name: &str,
    description: &str,
    expect: Expect,
    m: &Measurement,
    mut note: String,
) -> TestResult {
    let seen = if m.sentinel >= PRESENT {
        Some(true)
    } else if m.sentinel <= ABSENT && m.beacon >= PRESENT {
        Some(false)
    } else {
        None
    };
    let verdict = match (expect, seen) {
        (_, None) => {
            if m.frames == 0 {
                note.push_str("no frames delivered");
            } else if m.beacon < PRESENT && m.sentinel < PRESENT {
                note.push_str(
                    "neither window reached the capture: it does not composite this \
                     process's windows (sandbox / other session?)",
                );
            } else {
                note.push_str("partial match");
            }
            Verdict::Inconclusive
        }
        (Expect::Observe, Some(_)) => Verdict::Pass,
        (Expect::Visible, Some(true)) | (Expect::Hidden, Some(false)) => Verdict::Pass,
        (Expect::Visible, Some(false)) => {
            note.push_str("a plain window did not show up in the capture");
            Verdict::Fail
        }
        (Expect::Hidden, Some(true)) => {
            note.push_str("LEAK: the window is in the operator's stream");
            Verdict::Fail
        }
    };
    if seen == Some(false) && m.sentinel_frames > 0 {
        note.push_str(&format!(
            "; sentinel visible in {} of {} frames before exclusion took effect",
            m.sentinel_frames, m.frames
        ));
    }
    TestResult {
        display: d.index,
        name: name.to_string(),
        description: description.to_string(),
        expect,
        frames: m.frames,
        sentinel: m.sentinel,
        beacon: m.beacon,
        sentinel_frames: m.sentinel_frames,
        seen,
        verdict,
        note: note.trim_end_matches("; ").to_string(),
    }
}

// ─── the real app windows ────────────────────────────────────────────────────────────────

/// Show the shipping session bar (`AppEvent::SessionStarted`) and check that every visible
/// window of this process stays out of the capture, by diffing each window's area against a
/// frame taken before it appeared.
fn run_session_bar(d: &DisplayInfo) -> Result<Vec<TestResult>> {
    if !crate::app::is_running() {
        return Ok(vec![skipped(d, "session-bar", "app loop not running")]);
    }
    let (_, beacon_px) = layout(d);
    let cfg = capture_config(d);
    let mut cap = capture::create_capturer(&cfg)?;
    let beacon =
        platform::Shield::create(beacon_px.to_logical(d), BEACON, platform::beacon_style())?;
    std::thread::sleep(SETTLE);
    let before = measure(&mut cap, beacon_px, beacon_px, WATCH);
    let Some(baseline) = before.last.clone() else {
        return Ok(vec![skipped(d, "session-bar", "no baseline frame")]);
    };
    if before.beacon < PRESENT {
        return Ok(vec![skipped(
            d,
            "session-bar",
            "beacon not visible in the capture; cannot measure app windows",
        )]);
    }
    let ours: Vec<u64> = vec![beacon.window_id()];

    crate::app::post(crate::app::AppEvent::SessionStarted {
        operator: "Privacy probe".to_string(),
    });
    std::thread::sleep(APP_SETTLE);
    let windows: Vec<AppWindow> = platform::app_windows()
        .into_iter()
        .filter(|w| !ours.contains(&w.id))
        .collect();
    // Force a new frame with the bar up.
    beacon.set_visible(false);
    std::thread::sleep(Duration::from_millis(120));
    beacon.set_visible(true);
    std::thread::sleep(SETTLE);
    let after = measure(&mut cap, beacon_px, beacon_px, WATCH);
    crate::app::post(crate::app::AppEvent::SessionEnded);

    let mut out = Vec::new();
    let Some(frame) = after.last.as_ref() else {
        out.push(skipped(d, "session-bar", "no frame after showing the bar"));
        return Ok(out);
    };
    let mut any = false;
    for w in windows {
        let Some(rect) = PixelRect::from_logical(w.rect, d) else {
            continue;
        };
        any = true;
        let changed = changed_fraction(&baseline, frame, rect, Some(beacon_px));
        let seen = if changed >= 0.3 {
            Some(true)
        } else if changed <= ABSENT {
            Some(false)
        } else {
            None
        };
        let (verdict, note) = match seen {
            Some(true) => (
                Verdict::Fail,
                format!(
                    "LEAK: {:.0}% of the window's area changed in the capture (excluded flag: {})",
                    changed * 100.0,
                    w.excluded
                ),
            ),
            Some(false) => (
                Verdict::Pass,
                format!(
                    "{:.0}% changed (excluded flag: {})",
                    changed * 100.0,
                    w.excluded
                ),
            ),
            None => (
                Verdict::Inconclusive,
                format!("{:.0}% changed — partial", changed * 100.0),
            ),
        };
        out.push(TestResult {
            display: d.index,
            name: format!("app-window: {}", short_title(&w.title)),
            description: "a visible window of the running app (shown with the session bar)"
                .to_string(),
            expect: Expect::Hidden,
            frames: after.frames,
            sentinel: changed,
            beacon: after.beacon,
            sentinel_frames: 0,
            seen,
            verdict,
            note,
        });
    }
    if !any {
        out.push(skipped(
            d,
            "session-bar",
            "no visible app window on this display after SessionStarted",
        ));
    }
    drop(beacon);
    cap.stop();
    Ok(out)
}

/// Draw a thick magenta stroke through the real annotation overlay and check it stays out of
/// the capture.
fn run_annotation_overlay(d: &DisplayInfo) -> Result<TestResult> {
    use crate::annotate::AnnotateEvent;
    if !crate::app::is_running() {
        return Ok(skipped(d, "annotation-overlay", "app loop not running"));
    }
    let (sentinel_px, beacon_px) = layout(d);
    // A horizontal band across the middle third, 60 px thick.
    let band = PixelRect {
        x: sentinel_px.x,
        y: sentinel_px.y + sentinel_px.h / 2 - 30,
        w: sentinel_px.w,
        h: 60,
    };
    // Keep the beacon out of the band.
    let beacon_px = PixelRect {
        y: sentinel_px.y,
        ..beacon_px
    };
    let cfg = capture_config(d);
    let mut cap = capture::create_capturer(&cfg)?;
    let beacon =
        platform::Shield::create(beacon_px.to_logical(d), BEACON, platform::beacon_style())?;
    let yc = (band.y + 30) as f32;
    crate::app::post(crate::app::AppEvent::Annotate(AnnotateEvent::Stroke {
        id: 1,
        display: d.index,
        color: STROKE.to_string(),
        width: 60.0,
        points: vec![
            (band.x as f32 + 30.0, yc),
            ((band.x + band.w) as f32 - 30.0, yc),
        ],
    }));
    std::thread::sleep(APP_SETTLE);
    beacon.set_visible(false);
    std::thread::sleep(Duration::from_millis(120));
    beacon.set_visible(true);
    std::thread::sleep(SETTLE);
    // Measure the stroke's interior (inset from the round caps).
    let inner = PixelRect {
        x: band.x + 40,
        y: band.y + 12,
        w: band.w.saturating_sub(80).max(8),
        h: 36,
    };
    let m = measure(&mut cap, inner, beacon_px, WATCH);
    crate::app::post(crate::app::AppEvent::AnnotationsEnded);
    drop(beacon);
    cap.stop();
    Ok(judge(
        d,
        "annotation-overlay",
        "a stroke drawn by the real annotation overlay window",
        Expect::Hidden,
        &m,
        String::new(),
    ))
}

fn short_title(t: &str) -> String {
    let t = t.trim();
    if t.is_empty() {
        "(untitled)".to_string()
    } else {
        t.chars().take(40).collect()
    }
}

// ─── output ──────────────────────────────────────────────────────────────────────────────

fn print_table(r: &Report) {
    println!("remote-agent privacy-probe {} — {}", r.agent_version, r.os);
    for (k, v) in &r.environment {
        println!("  {k:<22} {v}");
    }
    for d in &r.displays {
        println!(
            "  display [{}]           {} {}x{} @{} ({},{}){}",
            d.index,
            d.name,
            d.width,
            d.height,
            d.scale,
            d.x,
            d.y,
            if d.primary { " primary" } else { "" }
        );
    }
    println!();
    println!(
        "{:<4} {:<38} {:<8} {:<12} {:>6} {:>8} {:>7}  note",
        "disp", "test", "expect", "verdict", "frames", "sentinel", "beacon"
    );
    for t in &r.results {
        let expect = match t.expect {
            Expect::Visible => "visible",
            Expect::Hidden => "hidden",
            Expect::Observe => "observe",
        };
        let verdict = match (t.verdict, t.seen) {
            (Verdict::Pass, Some(true)) if t.expect == Expect::Observe => "seen",
            (Verdict::Pass, Some(false)) if t.expect == Expect::Observe => "not seen",
            (Verdict::Pass, _) => "PASS",
            (Verdict::Fail, _) => "FAIL",
            (Verdict::Inconclusive, _) => "inconclusive",
            (Verdict::Skipped, _) => "skipped",
        };
        println!(
            "{:<4} {:<38} {:<8} {:<12} {:>6} {:>7.0}% {:>6.0}%  {}",
            t.display,
            t.name.chars().take(38).collect::<String>(),
            expect,
            verdict,
            t.frames,
            t.sentinel * 100.0,
            t.beacon * 100.0,
            t.note
        );
    }
    println!();
    let s = &r.summary;
    println!(
        "{} passed, {} failed, {} inconclusive, {} observed, {} skipped",
        s.passed, s.failed, s.inconclusive, s.observed, s.skipped
    );
    if s.failed > 0 {
        println!("FAILED: at least one window that must stay out of the capture was in it.");
    } else if s.exit_code() == 3 {
        println!(
            "INCONCLUSIVE: the capture never showed this process's windows. Run the probe \
             from a normal login session (Terminal), not from a sandbox or a remote shell."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32, fill: Rgb) -> Bgra {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for px in data.as_chunks_mut::<4>().0 {
            px[0] = fill.b;
            px[1] = fill.g;
            px[2] = fill.r;
            px[3] = 255;
        }
        Bgra {
            data,
            stride: (w * 4) as usize,
            width: w,
            height: h,
        }
    }

    fn paint(f: &mut Bgra, rect: PixelRect, c: Rgb) {
        for y in rect.y..rect.y + rect.h {
            for x in rect.x..rect.x + rect.w {
                let o = y as usize * f.stride + x as usize * 4;
                f.data[o] = c.b;
                f.data[o + 1] = c.g;
                f.data[o + 2] = c.r;
            }
        }
    }

    fn display() -> DisplayInfo {
        DisplayInfo {
            index: 0,
            name: "test".into(),
            x: 0,
            y: 0,
            width: 300,
            height: 300,
            scale: 2.0,
            primary: true,
        }
    }

    #[test]
    fn colour_fraction_respects_hole_and_tolerance() {
        let (sentinel, beacon) = layout(&display());
        let mut f = frame(
            300,
            300,
            Rgb {
                r: 10,
                g: 10,
                b: 10,
            },
        );
        paint(&mut f, sentinel, SENTINEL);
        paint(&mut f, beacon, BEACON);
        assert!(colour_fraction(&f, sentinel, Some(beacon), SENTINEL) > 0.99);
        assert!(colour_fraction(&f, beacon, None, BEACON) > 0.99);
        // Without the hole the beacon area counts against the sentinel.
        assert!(colour_fraction(&f, sentinel, None, SENTINEL) < 0.99);
        // A slightly off colour still matches.
        paint(
            &mut f,
            sentinel,
            Rgb {
                r: 230,
                g: 20,
                b: 240,
            },
        );
        assert!(colour_fraction(&f, sentinel, Some(beacon), SENTINEL) > 0.99);
        paint(
            &mut f,
            sentinel,
            Rgb {
                r: 180,
                g: 20,
                b: 240,
            },
        );
        assert!(colour_fraction(&f, sentinel, Some(beacon), SENTINEL) < 0.01);
    }

    #[test]
    fn changed_fraction_sees_a_window_appear() {
        let (sentinel, beacon) = layout(&display());
        let a = frame(
            300,
            300,
            Rgb {
                r: 10,
                g: 10,
                b: 10,
            },
        );
        let mut b = a.clone();
        assert_eq!(changed_fraction(&a, &b, sentinel, Some(beacon)), 0.0);
        paint(
            &mut b,
            sentinel,
            Rgb {
                r: 90,
                g: 90,
                b: 90,
            },
        );
        assert!(changed_fraction(&a, &b, sentinel, Some(beacon)) > 0.99);
    }

    #[test]
    fn logical_roundtrip_and_clipping() {
        let d = display();
        let px = PixelRect {
            x: 100,
            y: 50,
            w: 100,
            h: 100,
        };
        let lr = px.to_logical(&d);
        assert_eq!((lr.x, lr.y, lr.w, lr.h), (50.0, 25.0, 50.0, 50.0));
        assert_eq!(PixelRect::from_logical(lr, &d), Some(px));
        // Off-display rectangles are rejected, overlapping ones clipped.
        let off = LogicalRect {
            x: 500.0,
            y: 0.0,
            w: 40.0,
            h: 40.0,
        };
        assert_eq!(PixelRect::from_logical(off, &d), None);
        let edge = LogicalRect {
            x: 140.0,
            y: 140.0,
            w: 40.0,
            h: 40.0,
        };
        assert_eq!(
            PixelRect::from_logical(edge, &d),
            Some(PixelRect {
                x: 280,
                y: 280,
                w: 20,
                h: 20
            })
        );
    }

    #[test]
    fn verdicts() {
        let d = display();
        let m = |sentinel: f32, beacon: f32, frames: u32| Measurement {
            frames,
            sentinel,
            beacon,
            sentinel_frames: 0,
            last: None,
            error: None,
        };
        let j = |e, m: &Measurement| judge(&d, "t", "", e, m, String::new());
        assert_eq!(j(Expect::Hidden, &m(0.0, 0.98, 5)).verdict, Verdict::Pass);
        assert_eq!(j(Expect::Hidden, &m(0.97, 0.0, 5)).verdict, Verdict::Fail);
        assert_eq!(j(Expect::Visible, &m(0.97, 0.0, 5)).verdict, Verdict::Pass);
        assert_eq!(j(Expect::Visible, &m(0.0, 0.98, 5)).verdict, Verdict::Fail);
        assert_eq!(j(Expect::Observe, &m(0.97, 0.0, 5)).seen, Some(true));
        assert_eq!(
            j(Expect::Hidden, &m(0.0, 0.0, 5)).verdict,
            Verdict::Inconclusive
        );
        assert_eq!(
            j(Expect::Hidden, &m(0.0, 0.0, 0)).verdict,
            Verdict::Inconclusive
        );
        let mut s = Summary::default();
        assert_eq!(s.exit_code(), 0);
        s.inconclusive = 3;
        assert_eq!(s.exit_code(), 3);
        s.passed = 1;
        assert_eq!(s.exit_code(), 0);
        s.failed = 1;
        assert_eq!(s.exit_code(), 2);
    }
}
