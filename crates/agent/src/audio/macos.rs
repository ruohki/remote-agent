//! System audio via ScreenCaptureKit (`capturesAudio`), independent of the video streams.
//!
//! A dedicated, tiny `SCStream` (32×32 @ 1 fps, no screen output registered) is used so the
//! audio lifetime is decoupled from display switching. Sample buffers arrive as
//! `SCStreamOutputType::Audio` with 48 kHz float PCM (interleaved or planar — both handled).

use super::{AudioFormat, AudioSource};
use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, AudioBuffer, AudioBufferList,
    AudioStreamBasicDescription,
};
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
};
use objc2_core_video::kCVPixelFormatType_32BGRA;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType, SCWindow,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SCK_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;

struct State {
    tx: Sender<Vec<f32>>,
    rx: Receiver<Vec<f32>>,
    error: Mutex<Option<String>>,
}

impl State {
    fn push(&self, pcm: Vec<f32>) {
        match self.tx.try_send(pcm) {
            Ok(()) => {}
            Err(TrySendError::Full(pcm)) => {
                // Drop the oldest block; audio must never back up.
                let _ = self.rx.try_recv();
                let _ = self.tx.try_send(pcm);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

struct Ivars {
    state: Arc<State>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "RemoteAgentAudioOutput"]
    #[ivars = Ivars]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        #[allow(non_snake_case)]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            // SAFETY: the sample buffer is valid for the duration of the callback.
            match unsafe { extract_pcm(sample_buffer) } {
                Ok(Some(pcm)) => self.ivars().state.push(pcm),
                Ok(None) => {}
                Err(e) => tracing::debug!("audio sample: {e:#}"),
            }
        }
    }

    unsafe impl SCStreamDelegate for AudioOutput {
        #[unsafe(method(stream:didStopWithError:))]
        #[allow(non_snake_case)]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            let msg = format!("audio stream stopped: {}", error.localizedDescription());
            tracing::warn!("ScreenCaptureKit {msg}");
            if let Ok(mut e) = self.ivars().state.error.lock() {
                e.get_or_insert(msg);
            }
        }
    }
);

impl AudioOutput {
    fn new(state: Arc<State>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { state });
        // SAFETY: NSObject init.
        unsafe { msg_send![super(this), init] }
    }
}

/// Pull interleaved f32 stereo samples out of an audio sample buffer.
///
/// # Safety
/// `sb` must be a valid audio `CMSampleBuffer`.
unsafe fn extract_pcm(sb: &CMSampleBuffer) -> Result<Option<Vec<f32>>> {
    let desc = sb.format_description().context("no format description")?;
    let asbd_ptr = CMAudioFormatDescriptionGetStreamBasicDescription(&desc);
    if asbd_ptr.is_null() {
        bail!("no stream basic description");
    }
    let asbd: AudioStreamBasicDescription = *asbd_ptr;
    let is_float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
    let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    if !is_float || asbd.mBitsPerChannel != 32 {
        bail!(
            "unsupported audio format: float={is_float} bits={}",
            asbd.mBitsPerChannel
        );
    }

    // Size the AudioBufferList.
    let mut needed: usize = 0;
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = sb.audio_buffer_list_with_retained_block_buffer(
        &mut needed,
        std::ptr::null_mut(),
        0,
        None,
        None,
        0,
        &mut block,
    );
    if needed == 0 {
        bail!("AudioBufferList size query failed (status {status})");
    }
    let mut storage = vec![0u64; needed.div_ceil(8) + 1];
    let list = storage.as_mut_ptr() as *mut AudioBufferList;
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = sb.audio_buffer_list_with_retained_block_buffer(
        std::ptr::null_mut(),
        list,
        needed,
        None,
        None,
        0,
        &mut block,
    );
    if status != 0 {
        bail!("CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer failed: {status}");
    }
    // The block buffer was retained for us; release it when done.
    let _block = if block.is_null() {
        None
    } else {
        Some(objc2_core_foundation::CFRetained::from_raw(
            std::ptr::NonNull::new_unchecked(block),
        ))
    };

    let count = (*list).mNumberBuffers as usize;
    let first: *const AudioBuffer = std::ptr::addr_of!((*list).mBuffers) as *const AudioBuffer;
    let buffers: Vec<&[f32]> = (0..count)
        .map(|i| {
            let b = &*first.add(i);
            let n = b.mDataByteSize as usize / 4;
            if b.mData.is_null() || n == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(b.mData as *const f32, n)
            }
        })
        .collect();
    if buffers.is_empty() {
        return Ok(None);
    }

    let out: Vec<f32> = if planar {
        // One buffer per channel; interleave the first two.
        let l = buffers[0];
        let r = buffers.get(1).copied().unwrap_or(l);
        let frames = l.len().min(r.len());
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            v.push(l[i]);
            v.push(r[i]);
        }
        v
    } else {
        // Single interleaved buffer with `channels` channels; keep the first two.
        let data = buffers[0];
        let frames = data.len() / channels;
        let mut v = Vec::with_capacity(frames * 2);
        for f in 0..frames {
            let base = f * channels;
            v.push(data[base]);
            v.push(if channels > 1 {
                data[base + 1]
            } else {
                data[base]
            });
        }
        v
    };
    Ok(Some(out))
}

struct SckAudio {
    stream: Retained<SCStream>,
    output: Retained<AudioOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    state: Arc<State>,
    stopped: bool,
}

// SAFETY: SCK objects are thread-safe; we only touch them from the audio thread.
unsafe impl Send for SckAudio {}

impl AudioSource for SckAudio {
    fn format(&self) -> AudioFormat {
        AudioFormat {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
        }
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<f32>>> {
        if let Some(e) = self.state.error.lock().ok().and_then(|e| e.clone()) {
            bail!("{e}");
        }
        match self.state.rx.recv_timeout(timeout) {
            Ok(pcm) => Ok(Some(pcm)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(None),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => bail!("audio channel closed"),
        }
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let (tx, rx) = crossbeam_channel::bounded::<()>(1);
        let block = RcBlock::new(move |_err: *mut NSError| {
            let _ = tx.try_send(());
        });
        // SAFETY: valid stream; SCK copies the block.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&block)) };
        let _ = rx.recv_timeout(SCK_CALL_TIMEOUT);
        // SAFETY: the output was added with the same type.
        let _ = unsafe {
            self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.output),
                SCStreamOutputType::Audio,
            )
        };
        tracing::debug!("ScreenCaptureKit audio stream stopped");
    }
}

impl Drop for SckAudio {
    fn drop(&mut self) {
        self.stop();
    }
}

fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = crossbeam_channel::bounded::<Result<usize, String>>(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if !error.is_null() {
                // SAFETY: valid NSError from SCK.
                Err(unsafe { &*error }.localizedDescription().to_string())
            } else if content.is_null() {
                Err("no shareable content".to_string())
            } else {
                // SAFETY: +0 pointer from SCK; retain it and pass the address across the channel.
                match unsafe { Retained::retain(content) } {
                    Some(c) => Ok(Retained::into_raw(c) as usize),
                    None => Err("retain failed".into()),
                }
            };
            let _ = tx.try_send(result);
        },
    );
    // SAFETY: SCK copies the block.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &block,
        );
    }
    match rx.recv_timeout(SCK_CALL_TIMEOUT) {
        // SAFETY: address produced by `into_raw` above, converted back exactly once.
        Ok(Ok(addr)) => unsafe { Retained::from_raw(addr as *mut SCShareableContent) }
            .context("null shareable content"),
        Ok(Err(m)) => bail!("SCShareableContent failed: {m}"),
        Err(_) => bail!("timed out waiting for SCShareableContent"),
    }
}

pub fn create() -> Result<Box<dyn AudioSource>> {
    if !CGPreflightScreenCaptureAccess() {
        bail!("Screen Recording permission not granted (required for system audio)");
    }
    let content = shareable_content()?;
    // SAFETY: plain accessor.
    let displays = unsafe { content.displays() };
    let display = displays
        .iter()
        .next()
        .context("no display to attach audio to")?;
    // SAFETY: constructing SCK objects with valid arguments.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::<SCWindow>::new(),
        )
    };
    // SAFETY: setters on a fresh configuration.
    let config = unsafe {
        let c = SCStreamConfiguration::new();
        c.setWidth(32);
        c.setHeight(32);
        c.setPixelFormat(kCVPixelFormatType_32BGRA);
        c.setMinimumFrameInterval(CMTime::new(1, 1));
        c.setShowsCursor(false);
        c.setQueueDepth(2);
        c.setCapturesAudio(true);
        c.setSampleRate(SAMPLE_RATE as isize);
        c.setChannelCount(CHANNELS as isize);
        c.setExcludesCurrentProcessAudio(true);
        c
    };
    let (tx, rx) = crossbeam_channel::bounded::<Vec<f32>>(16);
    let state = Arc::new(State {
        tx,
        rx,
        error: Mutex::new(None),
    });
    let output = AudioOutput::new(state.clone());
    let queue = DispatchQueue::new("com.remoteagent.audio", DispatchQueueAttr::SERIAL);
    // SAFETY: valid filter/config; delegate retained by us.
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        )
    };
    // SAFETY: output retained for the stream's lifetime.
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Audio,
            Some(&queue),
        )
    }
    .map_err(|e| {
        anyhow!(
            "addStreamOutput(audio) failed: {}",
            e.localizedDescription()
        )
    })?;

    let (stx, srx) = crossbeam_channel::bounded::<Option<String>>(1);
    let block = RcBlock::new(move |error: *mut NSError| {
        let msg = if error.is_null() {
            None
        } else {
            // SAFETY: valid NSError from SCK.
            Some(unsafe { &*error }.localizedDescription().to_string())
        };
        let _ = stx.try_send(msg);
    });
    // SAFETY: valid stream; SCK copies the block.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
    match srx.recv_timeout(SCK_CALL_TIMEOUT) {
        Ok(None) => {}
        Ok(Some(m)) => bail!("SCStream startCapture (audio) failed: {m}"),
        Err(_) => bail!("timed out starting audio capture"),
    }
    tracing::info!(
        sample_rate = SAMPLE_RATE,
        channels = CHANNELS,
        "system audio capture started"
    );
    Ok(Box::new(SckAudio {
        stream,
        output,
        _queue: queue,
        state,
        stopped: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs Screen Recording permission; plays nothing, so silence is acceptable — the
    /// point is that SCK delivers audio sample buffers at all.
    #[test]
    #[ignore]
    fn macos_audio_yields_samples() {
        let mut src = create().expect("audio source");
        assert_eq!(src.format().sample_rate, 48_000);
        let mut total = 0usize;
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if let Some(pcm) = src.read(Duration::from_millis(200)).unwrap() {
                total += pcm.len();
            }
        }
        eprintln!("received {total} interleaved samples in 3 s");
        assert!(
            total > 48_000,
            "expected at least ~0.5 s of audio, got {total} samples"
        );
        src.stop();
    }
}
