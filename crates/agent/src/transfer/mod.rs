//! Resumable file transfers, the remote file browser and rich-clipboard transports over the
//! `files` data channel — the agent side of [`protocol::files`].
//!
//! The manager is transport-agnostic: it talks to the browser through a [`FilesSink`]
//! (implemented over a WebRTC data channel by the session) and reports what happened
//! through [`TransferNotice`]s. Outgoing transfers stream from a tokio task each; incoming
//! chunks are written straight into `<name>.part` with a JSON sidecar so an interrupted
//! transfer resumes from the last byte stored (see [`sidecar`]).

pub mod browse;
pub mod codec;
pub mod sidecar;

use anyhow::{anyhow, bail, Context, Result};
use bytes::{Bytes, BytesMut};
use protocol::files::{
    decode_chunk_any, encode_chunk_header, encode_chunk_header_v2, ChunkCodec, FileMessage,
    TransferDirection, TransferKind, ACK_INTERVAL_BYTES, CHUNK_HEADER_LEN, CHUNK_HEADER_V2_LEN,
    MAX_CHUNK_BYTES,
};
use sha2::{Digest, Sha256};
use sidecar::Sidecar;
use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A sender that sees no ack progress for this long rewinds to the last acked offset.
pub const ACK_STALE: Duration = Duration::from_secs(15);

/// Outbound side of the `files` channel.
#[async_trait::async_trait]
pub trait FilesSink: Send + Sync + 'static {
    async fn send_message(&self, msg: &FileMessage) -> Result<()>;
    /// Send one binary chunk frame; may block while the channel applies back-pressure.
    async fn send_chunk(&self, frame: BytesMut) -> Result<()>;
}

/// What the session should know about (events, clipboard placement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferNotice {
    Started {
        token: String,
        name: String,
        size: u64,
        kind: TransferKind,
        direction: TransferDirection,
        offset: u64,
    },
    Completed {
        token: String,
        name: String,
        size: u64,
        kind: TransferKind,
        direction: TransferDirection,
        path: Option<PathBuf>,
    },
    Failed {
        token: String,
        name: String,
        reason: String,
    },
    /// A clipboard image from the operator was stored here; place it on the clipboard.
    ClipboardImage(PathBuf),
    /// A clipboard file group from the operator is complete; place these on the clipboard.
    ClipboardFiles(Vec<PathBuf>),
}

#[derive(Debug, Clone)]
pub struct TransferConfig {
    pub allow_files: bool,
    pub allow_clipboard: bool,
    pub dir: PathBuf,
}

impl TransferConfig {
    /// Default transfer directory: `<home>/Downloads/RemoteAgent`.
    pub fn default_dir() -> PathBuf {
        browse::home_dir()
            .map(|h| h.join("Downloads").join("RemoteAgent"))
            .unwrap_or_else(|| std::env::temp_dir().join("RemoteAgent"))
    }
}

/// Data an outgoing transfer reads from.
#[derive(Clone)]
pub enum OutSource {
    File(PathBuf),
    Bytes(Bytes),
}

struct OutShared {
    acked: AtomicU64,
    sent: AtomicU64,
    /// Bytes handed to the channel (headers + possibly compressed payloads) since the last accept.
    wire: AtomicU64,
    last_ack: parking_lot::Mutex<Instant>,
    rewind: AtomicBool,
    cancel: AtomicBool,
    /// `complete` was sent and no rewind happened since.
    finished: AtomicBool,
}

struct Outgoing {
    token: String,
    name: String,
    size: u64,
    kind: TransferKind,
    source: OutSource,
    shared: Arc<OutShared>,
    task: Option<JoinHandle<()>>,
    started_at: Instant,
    accepted: bool,
    /// The receiver advertised DEFLATE chunks in its accept.
    deflate: bool,
}

struct Incoming {
    token: String,
    name: String,
    size: u64,
    kind: TransferKind,
    group: Option<String>,
    dir: PathBuf,
    part: PathBuf,
    /// `None` once closed for hashing/renaming.
    file: Option<std::fs::File>,
    received: u64,
    last_ack: u64,
    last_sidecar: u64,
    /// Bytes received on the wire (headers + possibly compressed payloads) this session.
    wire_bytes: u64,
}

pub struct TransferManager {
    cfg: TransferConfig,
    sink: Arc<dyn FilesSink>,
    notices: mpsc::UnboundedSender<TransferNotice>,
    incoming: HashMap<u32, Incoming>,
    outgoing: HashMap<u32, Outgoing>,
    next_id: u32,
    clipboard_groups: HashMap<String, Vec<PathBuf>>,
}

impl TransferManager {
    pub fn new(
        cfg: TransferConfig,
        sink: Arc<dyn FilesSink>,
        notices: mpsc::UnboundedSender<TransferNotice>,
    ) -> Self {
        if cfg.allow_files {
            if let Err(e) = std::fs::create_dir_all(&cfg.dir) {
                tracing::warn!("creating transfer dir {}: {e}", cfg.dir.display());
            }
            let removed = sidecar::cleanup_stale(&cfg.dir, sidecar::STALE_AFTER);
            if removed > 0 {
                tracing::info!(removed, "removed stale partial transfers");
            }
        }
        Self {
            cfg,
            sink,
            notices,
            incoming: HashMap::new(),
            outgoing: HashMap::new(),
            next_id: 2,
            clipboard_groups: HashMap::new(),
        }
    }

    pub fn config(&self) -> &TransferConfig {
        &self.cfg
    }

    /// Number of transfers currently in flight (both directions).
    pub fn active(&self) -> usize {
        self.incoming.len() + self.outgoing.len()
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(2).max(2);
        id
    }

    fn notify(&self, n: TransferNotice) {
        let _ = self.notices.send(n);
    }

    async fn send(&self, msg: FileMessage) {
        if let Err(e) = self.sink.send_message(&msg).await {
            tracing::debug!("files channel send: {e:#}");
        }
    }

    // ── agent-initiated offers (downloads, clipboard) ───────────────────────────────

    /// Offer a device file to the operator. `transfer_id = None` allocates an even id
    /// (unsolicited, e.g. clipboard); a `request` reuses the browser's id.
    pub async fn offer_file(
        &mut self,
        path: &Path,
        kind: TransferKind,
        group: Option<String>,
        transfer_id: Option<u32>,
    ) -> Result<u32> {
        let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let token = file_token(path, &meta);
        self.offer(
            OutSource::File(path.to_path_buf()),
            name,
            meta.len(),
            token,
            kind,
            group,
            transfer_id,
        )
        .await
    }

    /// Offer in-memory bytes (clipboard image).
    pub async fn offer_bytes(
        &mut self,
        name: String,
        bytes: Bytes,
        kind: TransferKind,
        group: Option<String>,
    ) -> Result<u32> {
        let token = hex::encode(&Sha256::digest(&bytes)[..16]);
        let size = bytes.len() as u64;
        self.offer(
            OutSource::Bytes(bytes),
            name,
            size,
            token,
            kind,
            group,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn offer(
        &mut self,
        source: OutSource,
        name: String,
        size: u64,
        token: String,
        kind: TransferKind,
        group: Option<String>,
        transfer_id: Option<u32>,
    ) -> Result<u32> {
        let id = match transfer_id {
            Some(id) => id,
            None => self.alloc_id(),
        };
        let shared = Arc::new(OutShared {
            acked: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            wire: AtomicU64::new(0),
            last_ack: parking_lot::Mutex::new(Instant::now()),
            rewind: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        self.outgoing.insert(
            id,
            Outgoing {
                token: token.clone(),
                name: name.clone(),
                size,
                kind,
                source,
                shared,
                task: None,
                started_at: Instant::now(),
                accepted: false,
                deflate: false,
            },
        );
        self.send(FileMessage::Offer {
            transfer_id: id,
            token,
            name,
            size,
            kind,
            direction: TransferDirection::ToOperator,
            dest_dir: None,
            group,
            sha256: None,
        })
        .await;
        Ok(id)
    }

    fn start_sender(&mut self, id: u32, offset: u64) {
        let Some(out) = self.outgoing.get_mut(&id) else {
            return;
        };
        let deflate = out.deflate;
        out.accepted = true;
        out.shared.acked.store(offset, Ordering::Relaxed);
        out.shared.sent.store(offset, Ordering::Relaxed);
        out.shared.wire.store(0, Ordering::Relaxed);
        *out.shared.last_ack.lock() = Instant::now();
        out.shared.finished.store(false, Ordering::Relaxed);
        out.shared.rewind.store(false, Ordering::Relaxed);
        if let Some(t) = out.task.take() {
            t.abort();
        }
        let notice = TransferNotice::Started {
            token: out.token.clone(),
            name: out.name.clone(),
            size: out.size,
            kind: out.kind,
            direction: TransferDirection::ToOperator,
            offset,
        };
        let shared = Arc::clone(&out.shared);
        let source = out.source.clone();
        let size = out.size;
        let name = out.name.clone();
        self.notify(notice);
        let sink = Arc::clone(&self.sink);
        let task = tokio::spawn(async move {
            let opts = SendOptions { deflate, name };
            if let Err(e) = run_sender(id, source, size, offset, sink.clone(), shared, opts).await {
                tracing::warn!(transfer = id, "sending: {e:#}");
                let _ = sink
                    .send_message(&FileMessage::Cancel {
                        transfer_id: id,
                        reason: format!("{e:#}"),
                    })
                    .await;
            }
        });
        if let Some(out) = self.outgoing.get_mut(&id) {
            out.task = Some(task);
        }
    }

    // ── inbound messages ────────────────────────────────────────────────────────────

    pub async fn handle_message(&mut self, msg: FileMessage) {
        match msg {
            FileMessage::Offer {
                transfer_id,
                token,
                name,
                size,
                kind,
                direction,
                dest_dir,
                group,
                ..
            } => {
                if direction != TransferDirection::ToDevice {
                    self.send(FileMessage::Reject {
                        transfer_id,
                        reason: "offer direction must be to_device".into(),
                    })
                    .await;
                    return;
                }
                match self.accept_offer(transfer_id, token, name, size, kind, dest_dir, group) {
                    Ok(offset) => {
                        self.send(FileMessage::Accept {
                            transfer_id,
                            offset,
                            codecs: Some(vec![ChunkCodec::Deflate]),
                        })
                        .await;
                    }
                    Err(e) => {
                        self.send(FileMessage::Reject {
                            transfer_id,
                            reason: format!("{e:#}"),
                        })
                        .await;
                    }
                }
            }
            FileMessage::Accept {
                transfer_id,
                offset,
                codecs,
            } => {
                let deflate = codecs
                    .as_deref()
                    .is_some_and(|c| c.contains(&ChunkCodec::Deflate));
                let size = self.outgoing.get_mut(&transfer_id).map(|o| {
                    o.deflate = deflate;
                    o.size
                });
                match size {
                    Some(size) if offset <= size => self.start_sender(transfer_id, offset),
                    Some(_) => {
                        self.fail_outgoing(transfer_id, "accept offset beyond file size")
                            .await
                    }
                    None => tracing::debug!(transfer = transfer_id, "accept for unknown transfer"),
                }
            }
            FileMessage::Reject {
                transfer_id,
                reason,
            } => {
                self.fail_outgoing(transfer_id, &format!("rejected: {reason}"))
                    .await;
            }
            FileMessage::Ack {
                transfer_id,
                offset,
            } => {
                if let Some(out) = self.outgoing.get(&transfer_id) {
                    let prev = out.shared.acked.load(Ordering::Relaxed);
                    if offset > prev {
                        out.shared.acked.store(offset, Ordering::Relaxed);
                        *out.shared.last_ack.lock() = Instant::now();
                    } else if offset < out.shared.sent.load(Ordering::Relaxed)
                        && out.shared.finished.load(Ordering::Relaxed)
                    {
                        // The receiver reports a gap after we finished: resend from there.
                        out.shared.acked.store(offset, Ordering::Relaxed);
                        self.start_sender(transfer_id, offset);
                    }
                }
            }
            FileMessage::Complete {
                transfer_id,
                sha256,
            } => self.complete_incoming(transfer_id, sha256).await,
            FileMessage::Done {
                transfer_id,
                ok,
                error,
                ..
            } => {
                if let Some(out) = self.outgoing.remove(&transfer_id) {
                    if let Some(t) = out.task {
                        t.abort();
                    }
                    if ok {
                        tracing::info!(
                            transfer = transfer_id,
                            name = %out.name,
                            size = out.size,
                            secs = out.started_at.elapsed().as_secs_f32(),
                            "download completed"
                        );
                        self.notify(TransferNotice::Completed {
                            token: out.token,
                            name: out.name,
                            size: out.size,
                            kind: out.kind,
                            direction: TransferDirection::ToOperator,
                            path: None,
                        });
                    } else {
                        self.notify(TransferNotice::Failed {
                            token: out.token,
                            name: out.name,
                            reason: error.unwrap_or_else(|| "receiver reported failure".into()),
                        });
                    }
                }
            }
            FileMessage::Cancel {
                transfer_id,
                reason,
            } => {
                if let Some(inc) = self.incoming.remove(&transfer_id) {
                    // Keep the partial for a later resume.
                    self.persist_sidecar(&inc);
                    self.notify(TransferNotice::Failed {
                        token: inc.token,
                        name: inc.name,
                        reason: format!("cancelled: {reason}"),
                    });
                } else {
                    self.fail_outgoing(transfer_id, &format!("cancelled: {reason}"))
                        .await;
                }
            }
            FileMessage::Request { transfer_id, path } => {
                if !self.cfg.allow_files {
                    self.send(FileMessage::Reject {
                        transfer_id,
                        reason: "file transfer is disabled on this device".into(),
                    })
                    .await;
                    return;
                }
                let result = match browse::resolve(&path) {
                    Ok(p) => {
                        self.offer_file(&p, TransferKind::File, None, Some(transfer_id))
                            .await
                    }
                    Err(e) => Err(e),
                };
                if let Err(e) = result {
                    self.send(FileMessage::Reject {
                        transfer_id,
                        reason: format!("{e:#}"),
                    })
                    .await;
                }
            }
            FileMessage::List { path } => {
                if !self.cfg.allow_files {
                    self.send(FileMessage::Listing {
                        path: path.unwrap_or_default(),
                        entries: vec![],
                        error: Some("file transfer is disabled on this device".into()),
                    })
                    .await;
                    return;
                }
                let reply = match path {
                    None => FileMessage::Listing {
                        path: String::new(),
                        entries: browse::roots(&self.cfg.dir),
                        error: None,
                    },
                    Some(p) => match browse::resolve(&p) {
                        Ok(dir) => {
                            let d = dir.clone();
                            let listed = tokio::task::spawn_blocking(move || browse::list(&d))
                                .await
                                .map_err(|e| anyhow!("{e}"))
                                .and_then(|r| r);
                            match listed {
                                Ok(entries) => FileMessage::Listing {
                                    path: dir.display().to_string(),
                                    entries,
                                    error: None,
                                },
                                Err(e) => FileMessage::Listing {
                                    path: p,
                                    entries: vec![],
                                    error: Some(format!("{e:#}")),
                                },
                            }
                        }
                        Err(e) => FileMessage::Listing {
                            path: p,
                            entries: vec![],
                            error: Some(format!("{e:#}")),
                        },
                    },
                };
                self.send(reply).await;
            }
            FileMessage::Mkdir { path } => self.file_op("mkdir", &path, browse::mkdir).await,
            FileMessage::Delete { path } => self.file_op("delete", &path, browse::delete).await,
            FileMessage::Rename { from, to } => {
                let result = if !self.cfg.allow_files {
                    Err(anyhow!("file transfer is disabled on this device"))
                } else {
                    browse::resolve(&from)
                        .and_then(|f| browse::resolve(&to).map(|t| (f, t)))
                        .and_then(|(f, t)| browse::rename(&f, &t))
                };
                self.send(FileMessage::OpResult {
                    op: "rename".into(),
                    path: from,
                    ok: result.is_ok(),
                    error: result.err().map(|e| format!("{e:#}")),
                })
                .await;
            }
            FileMessage::ClipboardGroupComplete { group } => {
                if let Some(paths) = self.clipboard_groups.remove(&group) {
                    self.notify(TransferNotice::ClipboardFiles(paths));
                }
            }
            // Handled by the session (it owns the pending clipboard content).
            FileMessage::RequestClipboard => {}
            // agent → browser only
            FileMessage::Listing { .. } | FileMessage::OpResult { .. } => {}
        }
    }

    async fn file_op(&mut self, op: &str, path: &str, f: fn(&Path) -> Result<()>) {
        let result = if !self.cfg.allow_files {
            Err(anyhow!("file transfer is disabled on this device"))
        } else {
            browse::resolve(path).and_then(|p| f(&p))
        };
        self.send(FileMessage::OpResult {
            op: op.into(),
            path: path.to_string(),
            ok: result.is_ok(),
            error: result.err().map(|e| format!("{e:#}")),
        })
        .await;
    }

    async fn fail_outgoing(&mut self, id: u32, reason: &str) {
        if let Some(out) = self.outgoing.remove(&id) {
            out.shared.cancel.store(true, Ordering::Relaxed);
            if let Some(t) = out.task {
                t.abort();
            }
            self.notify(TransferNotice::Failed {
                token: out.token,
                name: out.name,
                reason: reason.to_string(),
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_offer(
        &mut self,
        transfer_id: u32,
        token: String,
        name: String,
        size: u64,
        kind: TransferKind,
        dest_dir: Option<String>,
        group: Option<String>,
    ) -> Result<u64> {
        if !self.cfg.allow_files {
            bail!("file transfer is disabled on this device");
        }
        if matches!(
            kind,
            TransferKind::ClipboardImage | TransferKind::ClipboardFiles
        ) && !self.cfg.allow_clipboard
        {
            bail!("clipboard sync is disabled on this device");
        }
        if self.incoming.contains_key(&transfer_id) {
            bail!("transfer id {transfer_id} already in use");
        }
        if token.is_empty() || token.len() > 128 {
            bail!("invalid transfer token");
        }
        let dir = match kind {
            TransferKind::File => match dest_dir {
                Some(d) if !d.trim().is_empty() => {
                    let p = browse::resolve(&d)?;
                    if !p.is_dir() {
                        bail!("destination {} is not a directory", p.display());
                    }
                    p
                }
                _ => self.cfg.dir.clone(),
            },
            TransferKind::ClipboardImage => self.cfg.dir.join("Clipboard"),
            TransferKind::ClipboardFiles => {
                let g = group.clone().unwrap_or_else(|| "group".into());
                self.cfg.dir.join("Clipboard").join(sidecar::safe_name(&g))
            }
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let name = sidecar::safe_name(&name);

        // Resume?
        let (part, received) = match sidecar::find_resume(&dir, &token) {
            Some((part, sc)) if sc.size == size => (part, sc.received),
            _ => {
                let final_path = sidecar::unique_path(&dir, &name);
                let mut p = final_path.as_os_str().to_owned();
                p.push(sidecar::PART_SUFFIX);
                (PathBuf::from(p), 0)
            }
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&part)
            .with_context(|| format!("opening {}", part.display()))?;
        file.set_len(received)
            .with_context(|| format!("truncating {}", part.display()))?;
        file.seek(SeekFrom::Start(received))?;
        let inc = Incoming {
            token: token.clone(),
            name: name.clone(),
            size,
            kind,
            group,
            dir,
            part,
            file: Some(file),
            received,
            last_ack: received,
            last_sidecar: received,
            wire_bytes: 0,
        };
        self.persist_sidecar(&inc);
        self.notify(TransferNotice::Started {
            token,
            name,
            size,
            kind,
            direction: TransferDirection::ToDevice,
            offset: received,
        });
        self.incoming.insert(transfer_id, inc);
        Ok(received)
    }

    fn persist_sidecar(&self, inc: &Incoming) {
        let sc = Sidecar {
            token: inc.token.clone(),
            name: inc.name.clone(),
            size: inc.size,
            received: inc.received,
        };
        if let Err(e) = sc.write(&inc.part) {
            tracing::warn!("writing sidecar for {}: {e:#}", inc.part.display());
        }
    }

    pub async fn handle_chunk(&mut self, frame: &[u8]) {
        let Some(chunk) = decode_chunk_any(frame) else {
            tracing::debug!("malformed chunk frame ({} bytes)", frame.len());
            return;
        };
        let (id, offset) = (chunk.transfer_id, chunk.offset);
        let Some(inc) = self.incoming.get_mut(&id) else {
            return;
        };
        if offset < inc.received {
            return; // duplicate
        }
        inc.wire_bytes += frame.len() as u64;
        // Inflate DEFLATE payloads; the size cap bounds what a hostile frame can make us allocate.
        let inflated;
        let payload: &[u8] = match chunk.codec {
            ChunkCodec::Raw => chunk.payload,
            ChunkCodec::Deflate => {
                let limit = (inc.size - inc.received).min(MAX_CHUNK_BYTES as u64) as usize;
                match codec::inflate_chunk(chunk.payload, limit) {
                    Ok(b) => {
                        inflated = b;
                        &inflated
                    }
                    Err(e) => {
                        let reason = format!("bad compressed chunk: {e:#}");
                        let inc = self.incoming.remove(&id).expect("present");
                        self.persist_sidecar(&inc);
                        self.send(FileMessage::Cancel {
                            transfer_id: id,
                            reason: reason.clone(),
                        })
                        .await;
                        self.notify(TransferNotice::Failed {
                            token: inc.token,
                            name: inc.name,
                            reason,
                        });
                        return;
                    }
                }
            }
        };
        if offset > inc.received {
            // Gap: tell the sender where we are; it rewinds when acks stall.
            let received = inc.received;
            self.send(FileMessage::Ack {
                transfer_id: id,
                offset: received,
            })
            .await;
            return;
        }
        if inc.received + payload.len() as u64 > inc.size {
            let reason = "chunk exceeds announced size".to_string();
            let inc = self.incoming.remove(&id).expect("present");
            let _ = std::fs::remove_file(&inc.part);
            Sidecar::remove(&inc.part);
            self.send(FileMessage::Cancel {
                transfer_id: id,
                reason: reason.clone(),
            })
            .await;
            self.notify(TransferNotice::Failed {
                token: inc.token,
                name: inc.name,
                reason,
            });
            return;
        }
        let write = match inc.file.as_mut() {
            Some(f) => f.write_all(payload),
            None => Err(std::io::Error::other("file closed")),
        };
        if let Err(e) = write {
            let reason = format!("writing {}: {e}", inc.part.display());
            let inc = self.incoming.remove(&id).expect("present");
            self.send(FileMessage::Cancel {
                transfer_id: id,
                reason: reason.clone(),
            })
            .await;
            self.notify(TransferNotice::Failed {
                token: inc.token,
                name: inc.name,
                reason,
            });
            return;
        }
        inc.received += payload.len() as u64;
        if inc.received - inc.last_ack >= ACK_INTERVAL_BYTES || inc.received == inc.size {
            inc.last_ack = inc.received;
            let received = inc.received;
            if inc.received - inc.last_sidecar >= ACK_INTERVAL_BYTES {
                if let Some(f) = inc.file.as_mut() {
                    let _ = f.flush();
                }
                inc.last_sidecar = inc.received;
                let sc = Sidecar {
                    token: inc.token.clone(),
                    name: inc.name.clone(),
                    size: inc.size,
                    received: inc.received,
                };
                if let Err(e) = sc.write(&inc.part) {
                    tracing::debug!("sidecar: {e:#}");
                }
            }
            self.send(FileMessage::Ack {
                transfer_id: id,
                offset: received,
            })
            .await;
        }
    }

    async fn complete_incoming(&mut self, id: u32, sha256: String) {
        let Some(mut inc) = self.incoming.remove(&id) else {
            return;
        };
        if inc.received != inc.size {
            self.persist_sidecar(&inc);
            let received = inc.received;
            self.send(FileMessage::Done {
                transfer_id: id,
                ok: false,
                error: Some(format!(
                    "incomplete: {received} of {} bytes received",
                    inc.size
                )),
                path: None,
            })
            .await;
            self.notify(TransferNotice::Failed {
                token: inc.token,
                name: inc.name,
                reason: "incomplete".into(),
            });
            return;
        }
        // Close the handle before hashing/renaming (Windows refuses to rename open files).
        if let Some(mut f) = inc.file.take() {
            let _ = f.flush();
            let _ = f.sync_all();
        }
        let part = inc.part.clone();
        let hashed = tokio::task::spawn_blocking(move || hash_file(&part)).await;
        let actual = match hashed {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                self.finish_failed(id, inc, &format!("hashing: {e:#}"), false)
                    .await;
                return;
            }
            Err(e) => {
                self.finish_failed(id, inc, &format!("hash task: {e}"), false)
                    .await;
                return;
            }
        };
        if !actual.eq_ignore_ascii_case(sha256.trim()) {
            self.finish_failed(id, inc, "checksum mismatch", true).await;
            return;
        }
        let final_path = sidecar::final_path_for(&inc.part, &inc.dir, &inc.name);
        if let Err(e) = std::fs::rename(&inc.part, &final_path) {
            self.finish_failed(id, inc, &format!("renaming: {e}"), false)
                .await;
            return;
        }
        Sidecar::remove(&inc.part);
        tracing::info!(transfer = id, path = %final_path.display(), size = inc.size, "upload completed");
        tracing::info!(
            target: "perf",
            transfer = id,
            direction = "to_device",
            payload_bytes = inc.size,
            wire_bytes = inc.wire_bytes,
            ratio = format_args!("{:.2}", ratio(inc.size, inc.wire_bytes)),
            "transfer bytes"
        );
        self.send(FileMessage::Done {
            transfer_id: id,
            ok: true,
            error: None,
            path: Some(final_path.display().to_string()),
        })
        .await;
        self.notify(TransferNotice::Completed {
            token: inc.token.clone(),
            name: inc.name.clone(),
            size: inc.size,
            kind: inc.kind,
            direction: TransferDirection::ToDevice,
            path: Some(final_path.clone()),
        });
        match inc.kind {
            TransferKind::ClipboardImage => {
                self.notify(TransferNotice::ClipboardImage(final_path));
            }
            TransferKind::ClipboardFiles => {
                let group = inc.group.clone().unwrap_or_else(|| "group".into());
                self.clipboard_groups
                    .entry(group)
                    .or_default()
                    .push(final_path);
            }
            TransferKind::File => {}
        }
    }

    async fn finish_failed(&mut self, id: u32, inc: Incoming, reason: &str, discard: bool) {
        if discard {
            let _ = std::fs::remove_file(&inc.part);
            Sidecar::remove(&inc.part);
        } else {
            self.persist_sidecar(&inc);
        }
        self.send(FileMessage::Done {
            transfer_id: id,
            ok: false,
            error: Some(reason.to_string()),
            path: None,
        })
        .await;
        self.notify(TransferNotice::Failed {
            token: inc.token,
            name: inc.name,
            reason: reason.to_string(),
        });
    }

    /// Periodic maintenance: rewind stalled senders.
    pub fn tick(&mut self) {
        let mut restart = Vec::new();
        for (id, out) in &self.outgoing {
            if !out.accepted {
                continue;
            }
            let acked = out.shared.acked.load(Ordering::Relaxed);
            let sent = out.shared.sent.load(Ordering::Relaxed);
            let stale = out.shared.last_ack.lock().elapsed() > ACK_STALE;
            if acked < sent && stale && acked < out.size {
                tracing::warn!(transfer = id, acked, sent, "no ack progress; rewinding");
                restart.push((*id, acked));
            }
        }
        for (id, acked) in restart {
            self.start_sender(id, acked);
        }
    }

    /// Abort everything (session teardown). Partials are kept for resumption.
    pub async fn cancel_all(&mut self) {
        for (_, out) in self.outgoing.drain() {
            out.shared.cancel.store(true, Ordering::Relaxed);
            if let Some(t) = out.task {
                t.abort();
            }
        }
        let incoming: Vec<Incoming> = self.incoming.drain().map(|(_, v)| v).collect();
        for inc in incoming {
            if let Some(f) = inc.file.as_ref() {
                let _ = f.sync_all();
            }
            self.persist_sidecar(&inc);
        }
    }
}

/// Stable token for a device file: sha256(path|size|mtime) so the browser can resume it.
fn file_token(path: &Path, meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(path.display().to_string().as_bytes());
    h.update(meta.len().to_le_bytes());
    h.update(mtime.to_le_bytes());
    hex::encode(&h.finalize()[..16])
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Options negotiated for one sender run.
struct SendOptions {
    /// The receiver advertised DEFLATE: use version-2 frames and compress when it pays off.
    deflate: bool,
    /// File name, for the incompressibility hint.
    name: String,
}

/// `wire / payload` as a fraction (1.0 = no saving); `0.0` when nothing was sent.
fn ratio(payload: u64, wire: u64) -> f64 {
    if payload == 0 {
        0.0
    } else {
        wire as f64 / payload as f64
    }
}

/// Stream `source` from `offset`, then send `complete`.
async fn run_sender(
    id: u32,
    source: OutSource,
    size: u64,
    offset: u64,
    sink: Arc<dyn FilesSink>,
    shared: Arc<OutShared>,
    opts: SendOptions,
) -> Result<()> {
    let mut pos = offset;
    let mut file = match &source {
        OutSource::File(p) => Some(
            tokio::fs::File::open(p)
                .await
                .with_context(|| format!("opening {}", p.display()))?,
        ),
        OutSource::Bytes(_) => None,
    };
    // Incompressibility hint from the name and the first bytes (seek back afterwards).
    let hint = {
        let mut head = [0u8; 16];
        let n = match (&source, file.as_mut()) {
            (OutSource::Bytes(b), _) => {
                let n = b.len().min(16);
                head[..n].copy_from_slice(&b[..n]);
                n
            }
            (OutSource::File(_), Some(f)) => f.read(&mut head).await.unwrap_or(0),
            _ => 0,
        };
        codec::likely_incompressible(&opts.name, &head[..n])
    };
    if let Some(f) = file.as_mut() {
        f.seek(SeekFrom::Start(pos)).await?;
    }
    let v2 = opts.deflate;
    let header_len = if v2 {
        CHUNK_HEADER_V2_LEN
    } else {
        CHUNK_HEADER_LEN
    };
    // Whole frame (header + payload) stays ≤ 64 KiB: SCTP rejects larger messages.
    let max_payload = if v2 {
        codec::MAX_PAYLOAD_V2
    } else {
        MAX_CHUNK_BYTES - CHUNK_HEADER_LEN
    };
    let mut buf = vec![0u8; header_len + max_payload];
    let mut gate = codec::CompressionGate::new(hint);
    let mut payload_total = 0u64;
    let mut compressed_chunks = 0u32;
    while pos < size {
        if shared.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if shared.rewind.swap(false, Ordering::Relaxed) {
            pos = shared.acked.load(Ordering::Relaxed);
            if let Some(f) = file.as_mut() {
                f.seek(SeekFrom::Start(pos)).await?;
            }
        }
        let want = ((size - pos) as usize).min(max_payload);
        let n = match (&source, file.as_mut()) {
            (OutSource::Bytes(b), _) => {
                let end = (pos as usize + want).min(b.len());
                let n = end - pos as usize;
                buf[header_len..header_len + n].copy_from_slice(&b[pos as usize..end]);
                n
            }
            (OutSource::File(_), Some(f)) => {
                let n = f.read(&mut buf[header_len..header_len + want]).await?;
                if n == 0 {
                    bail!("file shrank while sending");
                }
                n
            }
            _ => unreachable!(),
        };
        let frame = if v2 {
            let raw = &buf[header_len..header_len + n];
            let packed = if gate.should_try() {
                let packed = codec::deflate_chunk(raw);
                gate.record(packed.is_some());
                packed
            } else {
                None
            };
            match packed {
                Some(p) => {
                    compressed_chunks += 1;
                    let mut f = BytesMut::with_capacity(CHUNK_HEADER_V2_LEN + p.len());
                    f.resize(CHUNK_HEADER_V2_LEN, 0);
                    encode_chunk_header_v2(id, pos, ChunkCodec::Deflate, &mut f[..]);
                    f.extend_from_slice(&p);
                    f
                }
                None => {
                    encode_chunk_header_v2(id, pos, ChunkCodec::Raw, &mut buf[..header_len]);
                    BytesMut::from(&buf[..header_len + n])
                }
            }
        } else {
            encode_chunk_header(id, pos, &mut buf[..header_len]);
            BytesMut::from(&buf[..header_len + n])
        };
        shared.wire.fetch_add(frame.len() as u64, Ordering::Relaxed);
        payload_total += n as u64;
        sink.send_chunk(frame).await.context("sending chunk")?;
        pos += n as u64;
        shared.sent.store(pos, Ordering::Relaxed);
    }
    let sha = match &source {
        OutSource::File(p) => {
            let p = p.clone();
            tokio::task::spawn_blocking(move || hash_file(&p))
                .await
                .map_err(|e| anyhow!("{e}"))??
        }
        OutSource::Bytes(b) => hex::encode(Sha256::digest(b)),
    };
    let wire = shared.wire.load(Ordering::Relaxed);
    tracing::info!(
        target: "perf",
        transfer = id,
        direction = "to_operator",
        payload_bytes = payload_total,
        wire_bytes = wire,
        ratio = format_args!("{:.2}", ratio(payload_total, wire)),
        compressed_chunks,
        deflate = v2,
        "transfer bytes"
    );
    shared.finished.store(true, Ordering::Relaxed);
    sink.send_message(&FileMessage::Complete {
        transfer_id: id,
        sha256: sha,
    })
    .await
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use protocol::files::decode_chunk;

    /// Sink that records messages and loops chunks back for inspection.
    struct RecSink {
        msgs: Mutex<Vec<FileMessage>>,
        chunks: Mutex<Vec<BytesMut>>,
    }

    #[async_trait::async_trait]
    impl FilesSink for RecSink {
        async fn send_message(&self, msg: &FileMessage) -> Result<()> {
            self.msgs.lock().push(msg.clone());
            Ok(())
        }
        async fn send_chunk(&self, frame: BytesMut) -> Result<()> {
            self.chunks.lock().push(frame);
            Ok(())
        }
    }

    fn manager(
        dir: &Path,
    ) -> (
        TransferManager,
        Arc<RecSink>,
        mpsc::UnboundedReceiver<TransferNotice>,
    ) {
        let sink = Arc::new(RecSink {
            msgs: Mutex::new(vec![]),
            chunks: Mutex::new(vec![]),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let m = TransferManager::new(
            TransferConfig {
                allow_files: true,
                allow_clipboard: true,
                dir: dir.to_path_buf(),
            },
            sink.clone(),
            tx,
        );
        (m, sink, rx)
    }

    fn chunk(id: u32, offset: u64, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; CHUNK_HEADER_LEN];
        encode_chunk_header(id, offset, &mut f);
        f.extend_from_slice(payload);
        f
    }

    #[tokio::test]
    async fn upload_resume_and_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut m, sink, mut notices) = manager(tmp.path());
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let sha = hex::encode(Sha256::digest(&data));

        // First session: offer, send first 3 chunks, then "disconnect".
        m.handle_message(FileMessage::Offer {
            transfer_id: 1,
            token: "tok1".into(),
            name: "../data.bin".into(),
            size: data.len() as u64,
            kind: TransferKind::File,
            direction: TransferDirection::ToDevice,
            dest_dir: None,
            group: None,
            sha256: None,
        })
        .await;
        assert!(matches!(
            sink.msgs.lock().last(),
            Some(FileMessage::Accept {
                transfer_id: 1,
                offset: 0,
                codecs: Some(_),
            })
        ));
        let mut off = 0u64;
        for _ in 0..3 {
            let end = (off as usize + MAX_CHUNK_BYTES).min(data.len());
            m.handle_chunk(&chunk(1, off, &data[off as usize..end]))
                .await;
            off = end as u64;
        }
        m.cancel_all().await;
        let part = tmp.path().join("data.bin.part");
        assert!(part.exists());
        let sc = Sidecar::read(&part).unwrap();
        assert_eq!(sc.received, off);
        assert!(matches!(
            notices.try_recv(),
            Ok(TransferNotice::Started { offset: 0, .. })
        ));

        // Second session: re-offer with the same token → resume from `off`.
        let (mut m2, sink2, mut notices2) = manager(tmp.path());
        m2.handle_message(FileMessage::Offer {
            transfer_id: 5,
            token: "tok1".into(),
            name: "data.bin".into(),
            size: data.len() as u64,
            kind: TransferKind::File,
            direction: TransferDirection::ToDevice,
            dest_dir: None,
            group: None,
            sha256: None,
        })
        .await;
        match sink2.msgs.lock().last() {
            Some(FileMessage::Accept { offset, .. }) => assert_eq!(*offset, off),
            other => panic!("expected accept, got {other:?}"),
        }
        assert!(
            matches!(notices2.try_recv(), Ok(TransferNotice::Started { offset, .. }) if offset == off)
        );
        // Duplicate and gap chunks are tolerated.
        m2.handle_chunk(&chunk(5, 0, &data[..10])).await;
        m2.handle_chunk(&chunk(5, off + 100, &data[..10])).await;
        assert!(matches!(
            sink2.msgs.lock().last(),
            Some(FileMessage::Ack { offset, .. }) if *offset == off
        ));
        while (off as usize) < data.len() {
            let end = (off as usize + MAX_CHUNK_BYTES).min(data.len());
            m2.handle_chunk(&chunk(5, off, &data[off as usize..end]))
                .await;
            off = end as u64;
        }
        m2.handle_message(FileMessage::Complete {
            transfer_id: 5,
            sha256: sha,
        })
        .await;
        let final_path = tmp.path().join("data.bin");
        assert!(final_path.exists(), "final file present");
        assert!(!part.exists(), "partial removed");
        assert_eq!(std::fs::read(&final_path).unwrap(), data);
        assert!(matches!(
            sink2.msgs.lock().last(),
            Some(FileMessage::Done { ok: true, .. })
        ));
        let mut completed = false;
        while let Ok(n) = notices2.try_recv() {
            if matches!(n, TransferNotice::Completed { .. }) {
                completed = true;
            }
        }
        assert!(completed);
    }

    #[tokio::test]
    async fn checksum_mismatch_discards_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut m, sink, _n) = manager(tmp.path());
        m.handle_message(FileMessage::Offer {
            transfer_id: 3,
            token: "t".into(),
            name: "x.bin".into(),
            size: 4,
            kind: TransferKind::File,
            direction: TransferDirection::ToDevice,
            dest_dir: None,
            group: None,
            sha256: None,
        })
        .await;
        m.handle_chunk(&chunk(3, 0, b"abcd")).await;
        m.handle_message(FileMessage::Complete {
            transfer_id: 3,
            sha256: "00".repeat(32),
        })
        .await;
        assert!(matches!(
            sink.msgs.lock().last(),
            Some(FileMessage::Done { ok: false, .. })
        ));
        assert!(!tmp.path().join("x.bin").exists());
        assert!(!tmp.path().join("x.bin.part").exists());
    }

    #[tokio::test]
    async fn rejects_when_disabled_and_bad_dest() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut m, sink, _n) = manager(tmp.path());
        m.handle_message(FileMessage::Offer {
            transfer_id: 7,
            token: "t".into(),
            name: "x".into(),
            size: 1,
            kind: TransferKind::File,
            direction: TransferDirection::ToDevice,
            dest_dir: Some("relative/dir".into()),
            group: None,
            sha256: None,
        })
        .await;
        assert!(matches!(
            sink.msgs.lock().last(),
            Some(FileMessage::Reject { .. })
        ));
        m.handle_message(FileMessage::Request {
            transfer_id: 9,
            path: "../../etc/passwd".into(),
        })
        .await;
        assert!(matches!(
            sink.msgs.lock().last(),
            Some(FileMessage::Reject { transfer_id: 9, .. })
        ));
    }

    #[tokio::test]
    async fn download_streams_chunks_and_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut m, sink, mut notices) = manager(tmp.path());
        let data: Vec<u8> = (0..150_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let src = tmp.path().join("src.bin");
        std::fs::write(&src, &data).unwrap();
        m.handle_message(FileMessage::Request {
            transfer_id: 11,
            path: src.display().to_string(),
        })
        .await;
        let offer = sink.msgs.lock().last().cloned();
        let token = match offer {
            Some(FileMessage::Offer {
                transfer_id: 11,
                token,
                size,
                ..
            }) => {
                assert_eq!(size, data.len() as u64);
                token
            }
            other => panic!("expected offer, got {other:?}"),
        };
        // Browser accepts from an offset (resume).
        let resume = 70_000u64;
        m.handle_message(FileMessage::Accept {
            transfer_id: 11,
            offset: resume,
            codecs: None,
        })
        .await;
        // Wait for the sender task.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let done = matches!(sink.msgs.lock().last(), Some(FileMessage::Complete { .. }));
            if done || Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let complete = { sink.msgs.lock().last().cloned() };
        let Some(FileMessage::Complete { sha256, .. }) = complete else {
            panic!("no complete");
        };
        assert_eq!(sha256, hex::encode(Sha256::digest(&data)));
        let chunks: Vec<BytesMut> = { sink.chunks.lock().clone() };
        let mut reassembled = vec![0u8; data.len()];
        let mut first_offset = None;
        for c in chunks.iter() {
            let (id, off, payload) = decode_chunk(c).unwrap();
            assert_eq!(id, 11);
            first_offset.get_or_insert(off);
            reassembled[off as usize..off as usize + payload.len()].copy_from_slice(payload);
        }
        assert_eq!(first_offset, Some(resume));
        assert_eq!(&reassembled[resume as usize..], &data[resume as usize..]);
        m.handle_message(FileMessage::Done {
            transfer_id: 11,
            ok: true,
            error: None,
            path: None,
        })
        .await;
        let mut seen = vec![];
        while let Ok(n) = notices.try_recv() {
            seen.push(n);
        }
        assert!(seen
            .iter()
            .any(|n| matches!(n, TransferNotice::Started { offset, .. } if *offset == resume)));
        assert!(seen
            .iter()
            .any(|n| matches!(n, TransferNotice::Completed { token: t, .. } if *t == token)));
        assert_eq!(m.active(), 0);
    }
}
