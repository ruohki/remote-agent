//! Length-prefixed JSON frames: a 4-byte big-endian length, then that many bytes of JSON.

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Largest frame accepted. Signaling and settings are a few KiB; anything near this is a bug
/// or a hostile peer.
pub const MAX_FRAME: usize = 4 * 1024 * 1024;

/// Write one frame.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(msg).context("encoding link frame")?;
    if body.len() > MAX_FRAME {
        bail!("link frame too large ({} bytes)", body.len());
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await.context("writing link frame")?;
    w.flush().await.context("flushing link frame")?;
    Ok(())
}

/// Read one frame. `Ok(None)` at a clean end of stream (peer closed between frames).
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("reading link frame length"),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("link frame too large ({len} bytes)");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .context("reading link frame body")?;
    serde_json::from_slice(&body)
        .map(Some)
        .with_context(|| format!("decoding link frame: {}", String::from_utf8_lossy(&body)))
}

/// Read the next frame that decodes as `T`, skipping (and logging) frames that do not: the
/// framing is intact either way, and a message this build does not understand must not cost
/// the whole link. `Ok(None)` at a clean end of stream.
pub async fn read_message<R, T>(r: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    loop {
        match read_frame::<R, serde_json::Value>(r).await? {
            None => return Ok(None),
            Some(v) => match serde_json::from_value::<T>(v.clone()) {
                Ok(msg) => return Ok(Some(msg)),
                Err(e) => {
                    let kind = v
                        .get("msg")
                        .or_else(|| v.get("type"))
                        .and_then(|k| k.as_str())
                        .unwrap_or("?")
                        .to_string();
                    tracing::warn!(%kind, "skipping a link message this build does not understand: {e}");
                }
            },
        }
    }
}

/// Spawn a task that writes every message sent on the returned channel, in order, until the
/// channel closes or a write fails. The task's handle resolves with the write error, if any.
pub fn spawn_writer<W, T>(
    mut w: W,
) -> (
    mpsc::UnboundedSender<T>,
    tokio::task::JoinHandle<Result<()>>,
)
where
    W: AsyncWrite + Unpin + Send + 'static,
    T: Serialize + Send + Sync + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<T>();
    let task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            write_frame(&mut w, &msg).await?;
        }
        let _ = w.shutdown().await;
        Ok(())
    });
    (tx, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip_and_eof_is_clean() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &serde_json::json!({"x": 1}))
            .await
            .unwrap();
        write_frame(&mut a, &"second").await.unwrap();
        drop(a);
        let first: serde_json::Value = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(first["x"], 1);
        let second: String = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(second, "second");
        let end: Option<String> = read_frame(&mut b).await.unwrap();
        assert!(end.is_none());
    }

    #[tokio::test]
    async fn oversized_frames_are_refused() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&((MAX_FRAME as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        let r: Result<Option<String>> = read_frame(&mut b).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn writer_task_preserves_order() {
        let (a, mut b) = tokio::io::duplex(1 << 16);
        let (tx, task) = spawn_writer::<_, u32>(a);
        for i in 0..100u32 {
            tx.send(i).unwrap();
        }
        drop(tx);
        task.await.unwrap().unwrap();
        for i in 0..100u32 {
            let v: u32 = read_frame(&mut b).await.unwrap().unwrap();
            assert_eq!(v, i);
        }
    }
}
