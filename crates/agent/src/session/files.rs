//! Glue between the `files` data channel and [`crate::transfer::TransferManager`].

use crate::transfer::FilesSink;
use anyhow::{anyhow, Result};
use bytes::BytesMut;
use protocol::files::FileMessage;
use std::sync::Arc;
use webrtc::data_channel::DataChannel;

/// Sends control JSON as text frames and chunks as binary frames.
pub struct DataChannelSink {
    dc: Arc<dyn DataChannel>,
}

impl DataChannelSink {
    pub fn new(dc: Arc<dyn DataChannel>) -> Self {
        Self { dc }
    }

    pub fn shared(dc: Arc<dyn DataChannel>) -> Arc<dyn FilesSink> {
        Arc::new(Self::new(dc))
    }
}

#[async_trait::async_trait]
impl FilesSink for DataChannelSink {
    async fn send_message(&self, msg: &FileMessage) -> Result<()> {
        let text = serde_json::to_string(msg)?;
        self.dc
            .send_text(&text)
            .await
            .map_err(|e| anyhow!("files channel send_text: {e}"))
    }

    async fn send_chunk(&self, frame: BytesMut) -> Result<()> {
        self.dc
            .send(frame)
            .await
            .map_err(|e| anyhow!("files channel send: {e}"))
    }
}
