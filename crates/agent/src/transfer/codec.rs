//! Per-chunk DEFLATE compression for the `files` channel (mirrors the web viewer's
//! `features/files/codec.ts`).
//!
//! Every chunk is compressed on its own as a **raw** DEFLATE stream (RFC 1951, no zlib or gzip
//! wrapper) so offsets, acks and resume keep counting uncompressed bytes. Compression is only
//! used once the receiver advertised [`ChunkCodec::Deflate`] in its `accept`; frames then use
//! the version-2 header (codec byte) whether or not a particular chunk ended up compressed.
//! A [`CompressionGate`] stops wasting CPU on data that does not shrink and probes now and
//! then so a compressible tail of an otherwise packed file is still caught.

use anyhow::{bail, Context, Result};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use protocol::files::{CHUNK_HEADER_V2_LEN, MAX_CHUNK_BYTES};
use std::io::{Read, Write};

/// Largest *uncompressed* payload per version-2 frame: header + payload never exceeds 64 KiB.
pub const MAX_PAYLOAD_V2: usize = 64 * 1024 - CHUNK_HEADER_V2_LEN;
/// Chunks smaller than this are never compressed (no room for a saving).
const MIN_COMPRESS_BYTES: usize = 128;
/// Consecutive chunks without a worthwhile saving before the gate backs off.
const BACKOFF_AFTER: u32 = 8;
/// While backed off, one chunk in this many is still tried.
const PROBE_EVERY: u32 = 16;

/// A compressed chunk replaces the raw one only when it is at least 1/16 (≈ 6 %) smaller —
/// the same rule the browser applies, so both directions behave alike.
pub fn worth_it(raw: usize, compressed: usize) -> bool {
    compressed * 16 < raw * 15
}

/// Compress one chunk; `None` when it does not pay off.
pub fn deflate_chunk(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < MIN_COMPRESS_BYTES {
        return None;
    }
    let mut enc = DeflateEncoder::new(Vec::with_capacity(raw.len() / 2), Compression::fast());
    enc.write_all(raw).ok()?;
    let out = enc.finish().ok()?;
    worth_it(raw.len(), out.len()).then_some(out)
}

/// Decompress one chunk, refusing outputs above `limit` (never more than a chunk) so a hostile
/// frame cannot make the agent allocate unboundedly.
pub fn inflate_chunk(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let max = limit.min(MAX_CHUNK_BYTES);
    let mut out = Vec::with_capacity(max.min(64 * 1024));
    let mut dec = flate2::read::DeflateDecoder::new(data).take(max as u64 + 1);
    dec.read_to_end(&mut out).context("inflating chunk")?;
    if out.len() > max {
        bail!("chunk inflates beyond {max} bytes");
    }
    Ok(out)
}

/// Decides per chunk whether compressing is worth a try.
#[derive(Debug, Clone)]
pub struct CompressionGate {
    misses: u32,
    since_probe: u32,
}

impl CompressionGate {
    /// `hint_incompressible` starts the gate backed off (extension/magic said "packed").
    pub fn new(hint_incompressible: bool) -> Self {
        Self {
            misses: if hint_incompressible {
                BACKOFF_AFTER
            } else {
                0
            },
            since_probe: 0,
        }
    }

    pub fn should_try(&mut self) -> bool {
        if self.misses < BACKOFF_AFTER {
            return true;
        }
        self.since_probe += 1;
        if self.since_probe >= PROBE_EVERY {
            self.since_probe = 0;
            return true;
        }
        false
    }

    pub fn record(&mut self, compressed: bool) {
        if compressed {
            self.misses = 0;
            self.since_probe = 0;
        } else {
            self.misses += 1;
        }
    }

    pub fn backed_off(&self) -> bool {
        self.misses >= BACKOFF_AFTER
    }
}

const INCOMPRESSIBLE_EXT: &[&str] = &[
    "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "lz4", "br", "jpg", "jpeg", "png", "gif",
    "webp", "heic", "heif", "avif", "mp4", "m4v", "mkv", "mov", "avi", "webm", "mp3", "m4a", "aac",
    "ogg", "opus", "flac", "docx", "xlsx", "pptx", "odt", "ods", "odp", "apk", "jar", "war", "ipa",
    "dmg", "pkg", "woff", "woff2", "pdf", "epub", "crx", "xpi",
];

/// Starting hint from the file name and its first bytes; the gate's probes correct it when the
/// hint is wrong (e.g. an uncompressed `.zip` store or a text file named `.dat`).
pub fn likely_incompressible(name: &str, first_bytes: &[u8]) -> bool {
    if let Some(ext) = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()) {
        if !ext.is_empty() && INCOMPRESSIBLE_EXT.contains(&ext.as_str()) {
            return true;
        }
    }
    let b = first_bytes;
    let starts = |m: &[u8]| b.len() >= m.len() && &b[..m.len()] == m;
    starts(b"PK\x03\x04")            // zip / office / jar
        || starts(&[0x1f, 0x8b])       // gzip
        || starts(b"7z\xbc\xaf\x27\x1c") // 7z
        || starts(b"Rar!")             // rar
        || starts(b"\x89PNG")          // png
        || starts(&[0xff, 0xd8, 0xff]) // jpeg
        || starts(b"GIF8")             // gif
        || (starts(b"RIFF") && b.len() >= 12 && &b[8..12] == b"WEBP")
        || starts(b"%PDF")             // pdf (mostly deflated streams)
        || starts(&[0x28, 0xb5, 0x2f, 0xfd]) // zstd
        || starts(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) // xz
        || starts(b"BZh")              // bzip2
        || starts(b"OggS")             // ogg
        || starts(b"fLaC")             // flac
        || starts(b"ID3")              // mp3 with tags
        || (b.len() >= 12 && &b[4..8] == b"ftyp") // mp4 / mov / heic
        || starts(b"\x1a\x45\xdf\xa3") // mkv / webm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_backs_off_after_misses_and_probes() {
        let mut g = CompressionGate::new(false);
        for _ in 0..BACKOFF_AFTER {
            assert!(g.should_try());
            g.record(false);
        }
        assert!(g.backed_off());
        let mut tries = 0;
        for _ in 0..PROBE_EVERY * 2 {
            if g.should_try() {
                tries += 1;
                g.record(false);
            }
        }
        assert_eq!(
            tries, 2,
            "one probe per PROBE_EVERY chunks while backed off"
        );
        g.record(true);
        assert!(!g.backed_off());
        assert!(CompressionGate::new(true).backed_off());
    }

    #[test]
    fn likely_incompressible_by_extension_and_magic() {
        assert!(likely_incompressible("photo.JPG", b""));
        assert!(likely_incompressible("archive.zip", b""));
        assert!(!likely_incompressible("notes.txt", b"hello"));
        assert!(!likely_incompressible("Makefile", b"all:"));
        assert!(likely_incompressible("blob.dat", b"PK\x03\x04rest"));
        assert!(likely_incompressible("blob.dat", &[0x1f, 0x8b, 8, 0]));
        assert!(likely_incompressible("clip.dat", b"\x89PNG\r\n\x1a\n"));
        assert!(likely_incompressible("movie.dat", b"\0\0\0\x18ftypisom"));
        assert!(likely_incompressible("img.dat", b"RIFF\0\0\0\0WEBPVP8 "));
        assert!(!likely_incompressible("data.dat", b"RIFF\0\0\0\0WAVEfmt "));
    }

    #[test]
    fn deflate_round_trip_and_incompressible_fallback() {
        let text: Vec<u8> = (0..60_000).map(|i| b"lorem ipsum "[i % 12]).collect();
        let packed = deflate_chunk(&text).expect("text compresses");
        assert!(packed.len() * 16 < text.len() * 15);
        assert_eq!(inflate_chunk(&packed, MAX_CHUNK_BYTES).unwrap(), text);

        // xorshift noise: incompressible.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let noise: Vec<u8> = (0..60_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect();
        assert!(
            deflate_chunk(&noise).is_none(),
            "noise does not shrink → raw"
        );
        assert!(
            deflate_chunk(&text[..100]).is_none(),
            "tiny chunks are never compressed"
        );
    }

    #[test]
    fn inflate_refuses_oversized_output() {
        let big = vec![0u8; MAX_CHUNK_BYTES + 10];
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(&big).unwrap();
        let packed = enc.finish().unwrap();
        assert!(inflate_chunk(&packed, MAX_CHUNK_BYTES).is_err());
        assert!(inflate_chunk(&packed, 1024).is_err());
        assert!(
            inflate_chunk(b"\xff\xff\xff", 1024).is_err(),
            "garbage is an error"
        );
    }
}
