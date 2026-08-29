//! Device clipboard: change detection (text, images, file lists) and placement of content
//! received from the operator.
//!
//! Detection polls every 500 ms but only inspects the contents when the platform's change
//! counter moved (`NSPasteboard.changeCount` / `GetClipboardSequenceNumber`), so reading
//! large images stays cheap. Content the agent itself just placed is remembered and not
//! echoed back to the operator.

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// What is on the device clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    /// PNG-encoded image.
    Image {
        png: Vec<u8>,
        width: u32,
        height: u32,
    },
    Files(Vec<PathBuf>),
}

impl ClipboardContent {
    /// Fingerprint used for change detection / echo suppression.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        match self {
            ClipboardContent::Text(t) => {
                h.update(b"text:");
                h.update(t.as_bytes());
            }
            ClipboardContent::Image { png, .. } => {
                h.update(b"image:");
                h.update(png);
            }
            ClipboardContent::Files(paths) => {
                h.update(b"files:");
                for p in paths {
                    h.update(p.display().to_string().as_bytes());
                    h.update(b"\0");
                }
            }
        }
        h.finalize().into()
    }

    pub fn total_bytes(&self) -> u64 {
        match self {
            ClipboardContent::Text(t) => t.len() as u64,
            ClipboardContent::Image { png, .. } => png.len() as u64,
            ClipboardContent::Files(paths) => paths
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .sum(),
        }
    }
}

/// Handle to the polling thread.
pub struct ClipboardWatch {
    pub rx: mpsc::UnboundedReceiver<ClipboardContent>,
    stop: Arc<AtomicBool>,
    own: Arc<Mutex<Option<[u8; 32]>>>,
}

impl ClipboardWatch {
    /// Tell the watcher that `content` was placed by us (do not report it back).
    pub fn mark_own(&self, content: &ClipboardContent) {
        if let Ok(mut own) = self.own.lock() {
            *own = Some(content.fingerprint());
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for ClipboardWatch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Where clipboard reads/writes go: the system clipboard, or a fake in tests.
pub trait ClipboardBackend: Send + Sync + 'static {
    fn start_watch(&self, rich: bool) -> ClipboardWatch;
    fn set_text(&self, text: &str) -> Result<()>;
    /// Decode the PNG at `path` and place it on the clipboard; returns (width, height).
    fn set_image_from_png(&self, path: &Path) -> Result<(u32, u32)>;
    fn set_files(&self, paths: &[PathBuf]) -> Result<()>;
}

/// The real system clipboard.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClipboard;

impl ClipboardBackend for SystemClipboard {
    fn start_watch(&self, rich: bool) -> ClipboardWatch {
        start_watch(rich)
    }
    fn set_text(&self, text: &str) -> Result<()> {
        set_text(text)
    }
    fn set_image_from_png(&self, path: &Path) -> Result<(u32, u32)> {
        set_image_from_png(path)
    }
    fn set_files(&self, paths: &[PathBuf]) -> Result<()> {
        set_files(paths)
    }
}

impl ClipboardWatch {
    /// A watcher fed by the caller (tests).
    pub fn manual() -> (Self, mpsc::UnboundedSender<ClipboardContent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                rx,
                stop: Arc::new(AtomicBool::new(false)),
                own: Arc::new(Mutex::new(None)),
            },
            tx,
        )
    }
}

/// Start watching the system clipboard. `rich` enables image and file-list detection.
pub fn start_watch(rich: bool) -> ClipboardWatch {
    let (tx, rx) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let own: Arc<Mutex<Option<[u8; 32]>>> = Arc::new(Mutex::new(None));
    let stop_flag = Arc::clone(&stop);
    let own_flag = Arc::clone(&own);
    let spawned = std::thread::Builder::new()
        .name("clipboard-watch".into())
        .spawn(move || {
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("clipboard unavailable: {e}");
                    return;
                }
            };
            // Don't replay whatever is on the clipboard when the session starts.
            let mut last_seq = crate::platform::clipboard_sequence();
            let mut last_fp = read_content(&mut clipboard, rich).map(|c| c.fingerprint());
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                let seq = crate::platform::clipboard_sequence();
                if seq.is_some() && seq == last_seq {
                    continue;
                }
                last_seq = seq;
                let Some(content) = read_content(&mut clipboard, rich) else {
                    continue;
                };
                let fp = content.fingerprint();
                if last_fp == Some(fp) {
                    continue;
                }
                last_fp = Some(fp);
                let is_own = own_flag
                    .lock()
                    .ok()
                    .map(|o| *o == Some(fp))
                    .unwrap_or(false);
                if is_own {
                    continue;
                }
                if tx.send(content).is_err() {
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("clipboard watcher thread: {e}");
    }
    ClipboardWatch { rx, stop, own }
}

fn read_content(cb: &mut arboard::Clipboard, rich: bool) -> Option<ClipboardContent> {
    if rich {
        let files = crate::platform::clipboard_files();
        if !files.is_empty() {
            return Some(ClipboardContent::Files(files));
        }
        if let Ok(img) = cb.get_image() {
            if let Ok(png) = encode_png(&img) {
                return Some(ClipboardContent::Image {
                    png,
                    width: img.width as u32,
                    height: img.height as u32,
                });
            }
        }
    }
    cb.get_text().ok().map(ClipboardContent::Text)
}

pub fn set_text(text: &str) -> Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set_text(text.to_owned())?;
    Ok(())
}

/// Decode a PNG file and put it on the clipboard as an image.
pub fn set_image_from_png(path: &Path) -> Result<(u32, u32)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (rgba, w, h) = decode_png(&bytes)?;
    let mut cb = arboard::Clipboard::new()?;
    cb.set_image(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: rgba.into(),
    })?;
    Ok((w, h))
}

/// Put a list of files on the clipboard (as file references).
pub fn set_files(paths: &[PathBuf]) -> Result<()> {
    crate::platform::set_clipboard_files(paths)
}

pub fn encode_png(img: &arboard::ImageData<'_>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width as u32, img.height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| anyhow!("png header: {e}"))?;
        w.write_image_data(&img.bytes)
            .map_err(|e| anyhow!("png data: {e}"))?;
    }
    Ok(out)
}

/// Returns (RGBA bytes, width, height).
pub fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().map_err(|e| anyhow!("png: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size().context("png buffer size")?];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| anyhow!("png frame: {e}"))?;
    let (w, h) = (info.width, info.height);
    let data = &buf[..info.buffer_size()];
    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => data.to_vec(),
        (png::ColorType::Rgb, png::BitDepth::Eight) => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        (png::ColorType::Grayscale, png::BitDepth::Eight) => {
            data.iter().flat_map(|&g| [g, g, g, 255]).collect()
        }
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        (ct, bd) => anyhow::bail!("unsupported PNG format {ct:?}/{bd:?}"),
    };
    Ok((rgba, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip() {
        let rgba: Vec<u8> = (0..16 * 8).flat_map(|i| [i as u8, 10, 20, 255]).collect();
        let img = arboard::ImageData {
            width: 16,
            height: 8,
            bytes: rgba.clone().into(),
        };
        let png = encode_png(&img).unwrap();
        let (back, w, h) = decode_png(&png).unwrap();
        assert_eq!((w, h), (16, 8));
        assert_eq!(back, rgba);
    }

    #[test]
    fn fingerprints_differ_by_kind() {
        let a = ClipboardContent::Text("x".into());
        let b = ClipboardContent::Files(vec![PathBuf::from("x")]);
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(
            a.fingerprint(),
            ClipboardContent::Text("x".into()).fingerprint()
        );
    }
}
