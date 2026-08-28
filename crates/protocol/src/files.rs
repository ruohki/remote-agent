//! File transfer, remote file browsing and rich clipboard over the `files` data channel.
//!
//! The `files` channel is created by the browser (ordered, reliable). It carries two kinds of
//! frames:
//!
//! * **text frames**: one JSON-encoded [`FileMessage`] each (control plane);
//! * **binary frames**: file chunks with a fixed 13-byte little-endian header, see
//!   [`CHUNK_HEADER_LEN`]:
//!   `[version: u8 = 1][transfer_id: u32][offset: u64][payload …]`.
//!
//! ## Transfer lifecycle (either direction)
//!
//! 1. Sender → `offer { transfer_id, token, name, size, kind, … }`. `transfer_id` is a
//!    per-session counter chosen by the sender (odd ids from the browser, even ids from the
//!    agent so they never collide); `token` is a random string that identifies the *logical*
//!    transfer across sessions and is what makes resumption possible.
//! 2. Receiver → `accept { transfer_id, offset }` where `offset` is how many bytes it already
//!    holds for that `token` (0 for a fresh transfer). The receiver keeps partial data in
//!    `<name>.part` + a small JSON sidecar keyed by `token`, so a transfer interrupted by a
//!    dropped session continues from `offset` when the sender re-offers it in a new session.
//! 3. Sender streams binary chunks from `offset` (≤ [`MAX_CHUNK_BYTES`] each), respecting the
//!    channel's buffered amount. Receiver → `ack { transfer_id, offset }` every
//!    [`ACK_INTERVAL_BYTES`]; a sender that sees no ack for 15 s re-sends from the last ack.
//! 4. Sender → `complete { transfer_id, sha256 }`. Receiver verifies the hash of the whole
//!    file, renames `.part` into place and replies `done { transfer_id, ok, error? }`.
//! 5. Either side may send `cancel` at any time; the receiver keeps the partial file so a
//!    later offer with the same `token` resumes.
//!
//! Downloads (device → operator) start with `request { transfer_id, path }` from the browser;
//! the agent answers with an `offer` that **reuses the request's `transfer_id`** and the flow
//! above applies with roles swapped. Offers the agent initiates on its own (clipboard content)
//! use even ids.
//!
//! ## Clipboard
//!
//! Text stays on the control channel. Images and file lists ride on this channel as transfers
//! with `kind = clipboard_image` / `clipboard_files` (see [`TransferKind`]); the control
//! channel only announces availability (`ControlMessage::ClipboardAvailable`).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Bytes of the binary chunk header (`u8` version + `u32` id + `u64` offset, little-endian).
pub const CHUNK_HEADER_LEN: usize = 13;
/// Header version currently emitted.
pub const CHUNK_VERSION: u8 = 1;
/// Maximum *payload* bytes per binary frame. Header + payload never exceeds 64 KiB (65,536
/// bytes): that is the largest SCTP message every WebRTC stack involved accepts.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024 - CHUNK_HEADER_LEN;
/// Receiver acknowledges progress at least every this many bytes.
pub const ACK_INTERVAL_BYTES: u64 = 1024 * 1024;
/// Sender pauses when the data channel's buffered amount exceeds this many bytes.
pub const BUFFERED_HIGH_WATER: u64 = 4 * 1024 * 1024;
/// … and resumes once it drops below this.
pub const BUFFERED_LOW_WATER: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TransferKind {
    /// Regular file into the transfer directory (or `dest_dir`).
    File,
    /// PNG image to place on / taken from the clipboard.
    ClipboardImage,
    /// One of the files of a clipboard file list (`group` ties them together).
    ClipboardFiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TransferDirection {
    /// Browser → device.
    ToDevice,
    /// Device → browser.
    ToOperator,
}

/// Directory listing entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub modified_ms: Option<u64>,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
#[ts(export)]
pub enum FileMessage {
    // ── transfers (both directions) ─────────────────────────────────────────────
    Offer {
        transfer_id: u32,
        token: String,
        name: String,
        size: u64,
        kind: TransferKind,
        direction: TransferDirection,
        /// Device directory to store into (`ToDevice`, `File` only); default transfer dir.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        dest_dir: Option<String>,
        /// Ties the files of one clipboard file list together.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        group: Option<String>,
        /// Known up front when the sender already hashed the file (optional; `complete` always carries it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        sha256: Option<String>,
    },
    Accept {
        transfer_id: u32,
        /// Bytes the receiver already has (resume point).
        offset: u64,
    },
    Reject {
        transfer_id: u32,
        reason: String,
    },
    /// Receiver progress; `offset` = contiguous bytes stored so far.
    Ack {
        transfer_id: u32,
        offset: u64,
    },
    Complete {
        transfer_id: u32,
        sha256: String,
    },
    Done {
        transfer_id: u32,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        error: Option<String>,
        /// Final path on the device (`ToDevice`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        path: Option<String>,
    },
    Cancel {
        transfer_id: u32,
        reason: String,
    },
    /// Browser asks the device to send a file (agent replies with `offer`).
    Request {
        transfer_id: u32,
        path: String,
    },

    // ── remote file browser (browser → agent, agent replies) ─────────────────────
    /// List a directory; `path = None` lists the well-known roots (home, transfer dir, volumes).
    List {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        path: Option<String>,
    },
    Listing {
        path: String,
        entries: Vec<FileEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        error: Option<String>,
    },
    Mkdir {
        path: String,
    },
    Delete {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    /// Result of `mkdir` / `delete` / `rename`.
    OpResult {
        op: String,
        path: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        error: Option<String>,
    },

    // ── clipboard (browser → agent) ───────────────────────────────────────────────
    /// Ask the agent to send whatever image / files are on the device clipboard.
    RequestClipboard,
    /// Sent after the last `done` of a `clipboard_files` group so the agent can place the
    /// whole list on the device clipboard at once.
    ClipboardGroupComplete {
        group: String,
    },
}

/// Encode a chunk header. `buf` must have room for [`CHUNK_HEADER_LEN`] bytes.
pub fn encode_chunk_header(transfer_id: u32, offset: u64, buf: &mut [u8]) {
    buf[0] = CHUNK_VERSION;
    buf[1..5].copy_from_slice(&transfer_id.to_le_bytes());
    buf[5..13].copy_from_slice(&offset.to_le_bytes());
}

/// Decode a chunk frame into `(transfer_id, offset, payload)`.
pub fn decode_chunk(frame: &[u8]) -> Option<(u32, u64, &[u8])> {
    if frame.len() < CHUNK_HEADER_LEN || frame[0] != CHUNK_VERSION {
        return None;
    }
    let id = u32::from_le_bytes(frame[1..5].try_into().ok()?);
    let offset = u64::from_le_bytes(frame[5..13].try_into().ok()?);
    Some((id, offset, &frame[CHUNK_HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_header_roundtrip() {
        let mut buf = vec![0u8; CHUNK_HEADER_LEN + 3];
        encode_chunk_header(7, 1 << 40, &mut buf);
        buf[CHUNK_HEADER_LEN..].copy_from_slice(b"abc");
        let (id, off, payload) = decode_chunk(&buf).unwrap();
        assert_eq!((id, off, payload), (7, 1 << 40, &b"abc"[..]));
        assert!(decode_chunk(&buf[..5]).is_none());
    }
}
