//! Runtime branding: what the person at the device sees (product name, accent, logo, support
//! text) and the icons derived from it.
//!
//! Precedence, highest first:
//! 1. **console** — `GET {server_url}/api/branding` (public JSON `Branding`), fetched on
//!    connect, every [`REFRESH_INTERVAL`] and whenever the hub asks ([`request_refresh`]);
//!    cached in the config dir (`branding.json` + `logo.png`) so it survives restarts and
//!    offline starts;
//! 2. **baked** — the signed trailer / bundle sidecar ([`crate::baked`]);
//! 3. **built-in default** — "Remote Support", blue accent, the console's monitor glyph.
//!
//! Consumers never hold a copy: they call [`current`] / [`product_name`] / … when they need
//! the values (banner, approval dialog) or subscribe with [`on_change`] (the app window, the
//! tray). The icon helpers turn the logo (or the default glyph) into the RGBA bitmaps the
//! dock, tray/menu-bar and window need, including a monochrome template image for the macOS
//! menu bar that follows the light/dark appearance automatically.

use crate::config::Paths;
use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use protocol::bakery::Branding;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Notify;

/// Product name when nothing is branded.
pub const DEFAULT_PRODUCT: &str = "Remote Support";
/// Accent when nothing is branded (`#3b82f6`).
pub const DEFAULT_ACCENT: &str = "#3b82f6";
/// How often the console branding is re-fetched while connected.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(600);
/// Cache file name inside the config dir.
pub const CACHE_FILE: &str = "branding.json";
/// Decoded logo written next to the cache for convenience / inspection.
pub const LOGO_FILE: &str = "logo.png";

/// Where the active branding came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrandingSource {
    Default,
    Baked,
    Console,
}

struct State {
    branding: Branding,
    source: BrandingSource,
    /// Decoded logo (cached; `None` when there is no logo or it failed to decode).
    logo: Option<Rgba>,
}

type Hook = Box<dyn Fn(&Branding) + Send + Sync>;

static STATE: OnceLock<RwLock<State>> = OnceLock::new();
static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());
static REFRESH: Notify = Notify::const_new();

fn state() -> &'static RwLock<State> {
    STATE.get_or_init(|| {
        // Lazily fall back to baked/default when `init` was not called (tests, tools).
        let (branding, source) = baked_or_default();
        let logo = decode_logo(&branding);
        RwLock::new(State {
            branding,
            source,
            logo,
        })
    })
}

fn baked_or_default() -> (Branding, BrandingSource) {
    match crate::baked::get() {
        Some(b)
            if !b.branding().product_name.is_empty() || b.branding().logo_png_base64.is_some() =>
        {
            (b.branding().clone(), BrandingSource::Baked)
        }
        _ => (Branding::default(), BrandingSource::Default),
    }
}

fn decode_logo(b: &Branding) -> Option<Rgba> {
    let b64 = b.logo_png_base64.as_deref()?;
    match decode_logo_b64(b64) {
        Ok(img) => Some(img),
        Err(e) => {
            tracing::warn!("branding logo is not a usable PNG: {e:#}");
            None
        }
    }
}

fn decode_logo_b64(b64: &str) -> Result<Rgba> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("logo base64")?;
    decode_png(&bytes)
}

/// Load the cached console branding (if any) so the UI is branded before the first fetch.
/// Call once at startup with the resolved config dir.
pub fn init(paths: &Paths) {
    let cache = paths.dir.join(CACHE_FILE);
    let cached: Option<Branding> = std::fs::read(&cache)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let (branding, source) = match cached {
        Some(b) => (b, BrandingSource::Console),
        None => baked_or_default(),
    };
    let logo = decode_logo(&branding);
    let st = STATE.get_or_init(|| {
        RwLock::new(State {
            branding: branding.clone(),
            source,
            logo: None,
        })
    });
    let mut w = st.write();
    w.branding = branding;
    w.source = source;
    w.logo = logo;
}

/// The active branding (cloned).
pub fn current() -> Branding {
    state().read().branding.clone()
}

pub fn source() -> BrandingSource {
    state().read().source
}

/// Product name shown everywhere, never empty.
pub fn product_name() -> String {
    let b = state().read();
    if b.branding.product_name.trim().is_empty() {
        DEFAULT_PRODUCT.to_string()
    } else {
        b.branding.product_name.trim().to_string()
    }
}

/// Accent colour as `#rrggbb` (validated, defaulting to [`DEFAULT_ACCENT`]).
pub fn accent() -> String {
    let b = state().read();
    normalize_accent(&b.branding.accent)
}

fn normalize_accent(s: &str) -> String {
    let s = s.trim();
    let ok = s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit());
    if ok {
        s.to_ascii_lowercase()
    } else {
        DEFAULT_ACCENT.to_string()
    }
}

pub fn accent_rgb() -> (u8, u8, u8) {
    parse_hex(&accent())
}

fn parse_hex(s: &str) -> (u8, u8, u8) {
    let p = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0);
    (p(1), p(3), p(5))
}

/// The decoded logo, if the branding has one.
pub fn logo() -> Option<Rgba> {
    state().read().logo.clone()
}

/// Register a hook invoked (on the caller's thread of `apply`) whenever the branding changes.
pub fn on_change(hook: impl Fn(&Branding) + Send + Sync + 'static) {
    HOOKS.lock().push(Box::new(hook));
}

/// JSON handed to the app page (`window.__app.setBranding`).
pub fn page_json() -> String {
    let b = current();
    serde_json::json!({
        "product_name": product_name(),
        "accent": accent(),
        "organization": b.organization,
        "support_text": b.support_text,
        "logo": b.logo_png_base64,
    })
    .to_string()
}

/// Apply branding received from the console: persist it, make it current and notify hooks.
/// Returns whether anything changed.
pub fn apply_console(branding: Branding, paths: &Paths) -> bool {
    {
        let st = state().read();
        if st.source == BrandingSource::Console && st.branding == branding {
            return false;
        }
    }
    if let Err(e) = persist(&branding, paths) {
        tracing::warn!("caching branding: {e:#}");
    }
    let logo = decode_logo(&branding);
    {
        let mut w = state().write();
        w.branding = branding.clone();
        w.source = BrandingSource::Console;
        w.logo = logo;
    }
    tracing::info!(product = %product_name(), "branding updated from console");
    for hook in HOOKS.lock().iter() {
        hook(&branding);
    }
    true
}

fn persist(branding: &Branding, paths: &Paths) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)?;
    let json = serde_json::to_vec_pretty(branding)?;
    let file = paths.dir.join(CACHE_FILE);
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &file)?;
    let logo_path = paths.dir.join(LOGO_FILE);
    match branding.logo_png_base64.as_deref() {
        Some(b64) => {
            use base64::Engine;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
                std::fs::write(&logo_path, bytes)?;
            }
        }
        None => {
            let _ = std::fs::remove_file(&logo_path);
        }
    }
    Ok(())
}

/// Ask the refresh loop to fetch now (e.g. after `config_update` or a reconnect).
pub fn request_refresh() {
    REFRESH.notify_one();
}

/// Fetch `GET {server_url}/api/branding` once and apply it.
pub async fn fetch_once(server_url: &str, paths: &Paths) -> Result<bool> {
    let url = format!("{}/api/branding", server_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(format!("remote-agent/{}", crate::AGENT_VERSION))
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    let branding: Branding = resp.json().await.context("parsing branding")?;
    Ok(apply_console(branding, paths))
}

/// Background task: fetch on start, then every [`REFRESH_INTERVAL`] or when
/// [`request_refresh`] is called. Runs until the runtime shuts down.
pub async fn refresh_loop(server_url: String, paths: Paths) {
    loop {
        match fetch_once(&server_url, &paths).await {
            Ok(changed) => tracing::debug!(changed, "branding refreshed"),
            Err(e) => tracing::debug!("branding fetch failed: {e:#}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(REFRESH_INTERVAL) => {}
            _ = REFRESH.notified() => {}
        }
    }
}

// ─── bitmaps ─────────────────────────────────────────────────────────────────────────────

/// Straight (non-premultiplied) 8-bit RGBA bitmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Rgba {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; (width * height * 4) as usize],
        }
    }

    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }

    fn set(&mut self, x: u32, y: u32, p: [u8; 4]) {
        let i = ((y * self.width + x) * 4) as usize;
        self.data[i..i + 4].copy_from_slice(&p);
    }

    /// Fraction of pixels that are (partially) transparent.
    fn transparent_fraction(&self) -> f32 {
        let n = (self.width * self.height).max(1) as f32;
        let t = self.data.chunks(4).filter(|p| p[3] < 250).count() as f32;
        t / n
    }
}

/// Decode a PNG of any colour type / bit depth to RGBA8.
pub fn decode_png(bytes: &[u8]) -> Result<Rgba> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().context("png header")?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).context("png frame")?;
    let (w, h) = (info.width, info.height);
    let src = &buf[..info.buffer_size()];
    let mut out = Rgba::new(w, h);
    match info.color_type {
        png::ColorType::Rgba => out.data.copy_from_slice(src),
        png::ColorType::Rgb => {
            for (i, p) in src.chunks(3).enumerate() {
                out.data[i * 4..i * 4 + 3].copy_from_slice(p);
                out.data[i * 4 + 3] = 255;
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for (i, p) in src.chunks(2).enumerate() {
                out.data[i * 4..i * 4 + 4].copy_from_slice(&[p[0], p[0], p[0], p[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for (i, &g) in src.iter().enumerate() {
                out.data[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, 255]);
            }
        }
        other => anyhow::bail!("unsupported PNG colour type {other:?}"),
    }
    Ok(out)
}

/// Encode RGBA8 as PNG.
pub fn encode_png(img: &Rgba) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width, img.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().expect("png header");
        w.write_image_data(&img.data).expect("png data");
    }
    out
}

/// Resample to `w`×`h` with box filtering (area average; fine for icons in both directions).
pub fn resize(src: &Rgba, w: u32, h: u32) -> Rgba {
    if src.width == w && src.height == h {
        return src.clone();
    }
    let mut out = Rgba::new(w, h);
    let sx = src.width as f32 / w as f32;
    let sy = src.height as f32 / h as f32;
    for y in 0..h {
        let y0 = (y as f32 * sy).floor() as u32;
        let y1 = (((y + 1) as f32 * sy).ceil() as u32)
            .min(src.height)
            .max(y0 + 1);
        for x in 0..w {
            let x0 = (x as f32 * sx).floor() as u32;
            let x1 = (((x + 1) as f32 * sx).ceil() as u32)
                .min(src.width)
                .max(x0 + 1);
            // Premultiplied average so transparent pixels do not bleed colour.
            let (mut r, mut g, mut b, mut a, mut n) = (0f32, 0f32, 0f32, 0f32, 0f32);
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let p = src.px(xx, yy);
                    let pa = p[3] as f32 / 255.0;
                    r += p[0] as f32 * pa;
                    g += p[1] as f32 * pa;
                    b += p[2] as f32 * pa;
                    a += pa;
                    n += 1.0;
                }
            }
            let px = if a > 0.0 {
                [
                    (r / a).round() as u8,
                    (g / a).round() as u8,
                    (b / a).round() as u8,
                    (a / n * 255.0).round() as u8,
                ]
            } else {
                [0, 0, 0, 0]
            };
            out.set(x, y, px);
        }
    }
    out
}

/// Fit `src` into a `size`×`size` transparent square (keeps aspect, centred, `pad` px margin).
pub fn fit_square(src: &Rgba, size: u32, pad: u32) -> Rgba {
    let inner = size.saturating_sub(pad * 2).max(1);
    let scale = (inner as f32 / src.width as f32).min(inner as f32 / src.height as f32);
    let w = ((src.width as f32 * scale).round() as u32).max(1);
    let h = ((src.height as f32 * scale).round() as u32).max(1);
    let scaled = resize(src, w, h);
    let mut out = Rgba::new(size, size);
    let ox = (size - w) / 2;
    let oy = (size - h) / 2;
    for y in 0..h {
        for x in 0..w {
            out.set(ox + x, oy + y, scaled.px(x, y));
        }
    }
    out
}

/// The built-in mark (same drawing as the web console favicon: a rounded square, a monitor
/// outline with a stand and a status dot), rendered at `size` px with 3×3 supersampling.
/// `bg = None` draws only the glyph (transparent background); `fg` colours the glyph.
pub fn default_glyph(
    size: u32,
    bg: Option<(u8, u8, u8)>,
    fg: (u8, u8, u8),
    dot: (u8, u8, u8),
) -> Rgba {
    let mut out = Rgba::new(size, size);
    let s = 32.0 / size as f32; // px → favicon units (viewBox 0 0 32 32)
    let ss = 3u32;
    for y in 0..size {
        for x in 0..size {
            let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
            for sy in 0..ss {
                for sx in 0..ss {
                    let ux = (x as f32 + (sx as f32 + 0.5) / ss as f32) * s;
                    let uy = (y as f32 + (sy as f32 + 0.5) / ss as f32) * s;
                    let c = glyph_sample(ux, uy, bg, fg, dot);
                    if let Some((cr, cg, cb)) = c {
                        r += cr as f32;
                        g += cg as f32;
                        b += cb as f32;
                        a += 1.0;
                    }
                }
            }
            let n = (ss * ss) as f32;
            if a > 0.0 {
                out.set(
                    x,
                    y,
                    [
                        (r / a).round() as u8,
                        (g / a).round() as u8,
                        (b / a).round() as u8,
                        (a / n * 255.0).round() as u8,
                    ],
                );
            }
        }
    }
    out
}

/// Colour at a point of the 32×32 favicon design, or `None` for transparent.
fn glyph_sample(
    x: f32,
    y: f32,
    bg: Option<(u8, u8, u8)>,
    fg: (u8, u8, u8),
    dot: (u8, u8, u8),
) -> Option<(u8, u8, u8)> {
    // status dot (drawn on top of the monitor outline)
    if (x - 22.0).powi(2) + (y - 12.0).powi(2) <= 2.0f32.powi(2) {
        return Some(dot);
    }
    // monitor outline: rounded rect 6..26 × 8..21, radius 2, stroke 2 (centred on the edge)
    if rounded_rect(x, y, 5.0, 7.0, 27.0, 22.0, 3.0)
        && !rounded_rect(x, y, 7.0, 9.0, 25.0, 20.0, 1.0)
    {
        return Some(fg);
    }
    // stand: segment (12,25)-(20,25), stroke 2, round caps
    if seg_dist(x, y, 12.0, 25.0, 20.0, 25.0) <= 1.0 {
        return Some(fg);
    }
    // background: rounded square radius 7
    match bg {
        Some(c) if rounded_rect(x, y, 0.0, 0.0, 32.0, 32.0, 7.0) => Some(c),
        _ => None,
    }
}

fn rounded_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn seg_dist(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (cx, cy) = (ax + t * dx, ay + t * dy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// Full-colour application icon (dock / taskbar / window): the logo fitted into a square, or
/// the default mark on the accent colour.
pub fn app_icon(size: u32) -> Rgba {
    match logo() {
        Some(l) => fit_square(&l, size, 0),
        None => default_glyph(size, Some(accent_rgb()), (255, 255, 255), (52, 211, 153)),
    }
}

/// Dock / application icon in the macOS style: transparent margins around a rounded-rect
/// tile (radius ≈ 22 % of the tile) — a full-bleed square renders with a hard dark seam.
pub fn dock_icon(size: u32) -> Rgba {
    let margin = (size as f32 * 0.09).round() as u32;
    let tile = size.saturating_sub(margin * 2).max(1);
    let content = app_icon(tile);
    let radius = tile as f32 * 0.225;
    let mut out = Rgba::new(size, size);
    for y in 0..size {
        for x in 0..size {
            let (ix, iy) = (x as i64 - margin as i64, y as i64 - margin as i64);
            if ix < 0 || iy < 0 || ix >= tile as i64 || iy >= tile as i64 {
                continue;
            }
            // Coverage of the rounded-rect mask, anti-aliased with a 4×4 supersample.
            let mut inside = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = ix as f32 + (sx as f32 + 0.5) / 4.0;
                    let py = iy as f32 + (sy as f32 + 0.5) / 4.0;
                    let dx = (px - radius)
                        .min(0.0)
                        .abs()
                        .max((px - (tile as f32 - radius)).max(0.0));
                    let dy = (py - radius)
                        .min(0.0)
                        .abs()
                        .max((py - (tile as f32 - radius)).max(0.0));
                    if dx * dx + dy * dy <= radius * radius {
                        inside += 1;
                    }
                }
            }
            if inside == 0 {
                continue;
            }
            let cov = inside as f32 / 16.0;
            let p = content.px(ix as u32, iy as u32);
            out.set(x, y, [p[0], p[1], p[2], (p[3] as f32 * cov).round() as u8]);
        }
    }
    out
}

/// Monochrome "template" image (black glyph + alpha) for the macOS menu bar: the system tints it
/// for light/dark appearance. Derived from the logo when there is one — its alpha channel when
/// the logo has transparency, otherwise dark pixels count as ink — else the default mark.
pub fn template_icon(size: u32) -> Rgba {
    match logo() {
        Some(l) => {
            let fitted = fit_square(&l, size, 0);
            let use_alpha = fitted.transparent_fraction() > 0.05;
            let mut out = Rgba::new(size, size);
            for y in 0..size {
                for x in 0..size {
                    let p = fitted.px(x, y);
                    let a = p[3] as f32 / 255.0;
                    let lum = (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32)
                        / 255.0;
                    let cov = if use_alpha { a } else { 1.0 - lum };
                    let cov = ((cov - 0.1) / 0.8).clamp(0.0, 1.0);
                    out.set(x, y, [0, 0, 0, (cov * 255.0).round() as u8]);
                }
            }
            out
        }
        None => default_glyph(size, None, (0, 0, 0), (0, 0, 0)),
    }
}

/// Tray icon for platforms without template images (Windows): the logo in colour, or the
/// default mark drawn light-on-dark when the taskbar is dark and dark-on-light otherwise.
pub fn tray_icon_colored(size: u32, dark_theme: bool) -> Rgba {
    match logo() {
        Some(l) => fit_square(&l, size, 0),
        None => {
            let fg = if dark_theme {
                (255, 255, 255)
            } else {
                (17, 24, 39)
            };
            default_glyph(size, None, fg, (52, 211, 153))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_icon_has_transparent_margins_and_rounded_corners() {
        let icon = dock_icon(128);
        assert_eq!(icon.px(0, 0)[3], 0, "corner must be transparent");
        assert_eq!(icon.px(64, 5)[3], 0, "margin must be transparent");
        assert_eq!(icon.px(13, 13)[3], 0, "tile corner is rounded");
        assert!(icon.px(64, 64)[3] > 200, "centre is opaque");
        assert!(
            icon.px(64, 12)[3] > 200,
            "tile edge inside the margin is opaque"
        );
    }

    /// The branding state is process-global; tests that touch it run one at a time.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn tmp_paths() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        let p = Paths {
            dir: t.path().join("cfg"),
        };
        (t, p)
    }

    #[test]
    fn default_glyph_has_background_and_ink() {
        let img = default_glyph(32, Some((59, 130, 246)), (255, 255, 255), (52, 211, 153));
        // corner is transparent (rounded), centre of the monitor is background, edge is white
        assert_eq!(img.px(0, 0)[3], 0);
        assert_eq!(img.px(16, 15), [59, 130, 246, 255]);
        let edge = img.px(6, 14);
        assert!(
            edge[0] > 200 && edge[3] == 255,
            "monitor outline should be white: {edge:?}"
        );
        // template variant: no background, only alpha
        let t = default_glyph(32, None, (0, 0, 0), (0, 0, 0));
        assert_eq!(t.px(16, 15)[3], 0);
        assert!(t.px(6, 14)[3] > 200);
    }

    #[test]
    fn png_roundtrip_and_resize() {
        let mut img = Rgba::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                img.set(x, y, [x as u8 * 60, y as u8 * 60, 9, 255]);
            }
        }
        let png = encode_png(&img);
        let back = decode_png(&png).unwrap();
        assert_eq!(back, img);
        let small = resize(&img, 2, 2);
        assert_eq!(small.width, 2);
        assert_eq!(small.px(0, 0)[3], 255);
        let fitted = fit_square(&resize(&img, 4, 2), 8, 1);
        assert_eq!((fitted.width, fitted.height), (8, 8));
        assert_eq!(fitted.px(0, 0)[3], 0); // padding stays transparent
    }

    #[test]
    fn template_from_opaque_logo_uses_dark_pixels_as_ink() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Opaque logo: white background with a black square in the middle.
        let mut logo = Rgba::new(8, 8);
        for y in 0..8 {
            for x in 0..8 {
                let ink = (2..6).contains(&x) && (2..6).contains(&y);
                logo.set(
                    x,
                    y,
                    if ink {
                        [0, 0, 0, 255]
                    } else {
                        [255, 255, 255, 255]
                    },
                );
            }
        }
        let png = encode_png(&logo);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let (_t, paths) = tmp_paths();
        let b = Branding {
            product_name: "Acme".into(),
            accent: "#e0562f".into(),
            logo_png_base64: Some(b64),
            ..Default::default()
        };
        assert!(apply_console(b, &paths));
        assert_eq!(product_name(), "Acme");
        assert_eq!(accent_rgb(), (0xe0, 0x56, 0x2f));
        let t = template_icon(8);
        assert!(t.px(3, 3)[3] > 200, "ink where the logo is dark");
        assert_eq!(t.px(0, 0)[3], 0, "transparent where the logo is white");
        // cache written
        assert!(paths.dir.join(CACHE_FILE).exists());
        assert!(paths.dir.join(LOGO_FILE).exists());
        // second apply with identical branding reports no change
        assert!(!apply_console(current(), &paths));
    }

    #[test]
    fn accent_is_validated() {
        assert_eq!(normalize_accent("#ABCDEF"), "#abcdef");
        assert_eq!(normalize_accent("red"), DEFAULT_ACCENT);
        assert_eq!(normalize_accent(""), DEFAULT_ACCENT);
    }

    #[test]
    fn init_prefers_cached_console_branding() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_t, paths) = tmp_paths();
        std::fs::create_dir_all(&paths.dir).unwrap();
        let b = Branding {
            product_name: "Cached Co".into(),
            ..Default::default()
        };
        std::fs::write(paths.dir.join(CACHE_FILE), serde_json::to_vec(&b).unwrap()).unwrap();
        init(&paths);
        assert_eq!(source(), BrandingSource::Console);
        assert_eq!(product_name(), "Cached Co");
    }
}
