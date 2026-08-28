//! macOS hardware encoding via VideoToolbox (`VTCompressionSession`).
//!
//! * [`supports`] consults `VTCopyVideoEncoderList` for a hardware-accelerated encoder of
//!   the codec (cached per codec).
//! * [`create`] opens a session that *requires* hardware acceleration, tuned for low
//!   latency (real-time, no frame reordering, no frame delay, 2 s keyframe interval).
//! * Frames captured by ScreenCaptureKit are `CVPixelBuffer`s and are submitted without
//!   copying; CPU `Bgra` frames are uploaded through a `CVPixelBufferPool`.
//! * The output callback converts VideoToolbox's AVCC/HVCC sample buffers to Annex-B,
//!   prepending VPS/SPS/PPS on every keyframe, and queues them for [`Encoder::encode`].

#[path = "videotoolbox/annexb.rs"]
mod annexb;

pub use annexb::{annexb_nals, h264_nal_type, hevc_nal_type};

use super::{EncodedFrame, Encoder, EncoderConfig};
use crate::capture::{Frame, FrameData};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};
use objc2_core_foundation::{
    kCFBooleanFalse, kCFBooleanTrue, CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained,
    CFString, CFType,
};
use objc2_core_media::{
    kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
    kCMFormatDescriptionTransferFunction_ITU_R_709_2, kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
    CMFormatDescription, CMSampleBuffer, CMTime, CMVideoCodecType,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AllowTemporalCompression, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_H264EntropyMode,
    kVTCompressionPropertyKey_MaxFrameDelayCount, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTH264EntropyMode_CABAC,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_H264_Main_AutoLevel,
    kVTProfileLevel_HEVC_Main_AutoLevel, kVTVideoEncoderList_CodecType,
    kVTVideoEncoderList_IsHardwareAccelerated,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTCopyVideoEncoderList, VTEncodeInfoFlags, VTSessionSetProperty,
};
use protocol::common::VideoCodec;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Timescale used for presentation timestamps handed to VideoToolbox.
const TIMESCALE: i32 = 1_000_000;
/// Queue depth between the VideoToolbox callback and `encode()`.
const OUTPUT_QUEUE: usize = 8;

/// Largest frame the hardware encoder accepts per codec (H.264 level 5.2, HEVC level 6.1).
/// Larger sources are scaled down by VideoToolbox to the session size.
pub fn max_output_size(codec: VideoCodec) -> (u32, u32) {
    match codec {
        VideoCodec::H264 => (4096, 2304),
        VideoCodec::H265 => (8192, 4320),
    }
}

/// Scale `(width, height)` down (aspect preserved, even dimensions) to fit `max`.
pub fn fit_size(width: u32, height: u32, max: (u32, u32)) -> (u32, u32) {
    if width <= max.0 && height <= max.1 {
        return (width & !1, height & !1);
    }
    let scale = (f64::from(max.0) / f64::from(width)).min(f64::from(max.1) / f64::from(height));
    let w = ((f64::from(width) * scale).floor() as u32) & !1;
    let h = ((f64::from(height) * scale).floor() as u32) & !1;
    (w.max(2), h.max(2))
}

fn codec_type(codec: VideoCodec) -> CMVideoCodecType {
    match codec {
        VideoCodec::H265 => kCMVideoCodecType_HEVC,
        VideoCodec::H264 => kCMVideoCodecType_H264,
    }
}

fn cf_bool(v: bool) -> &'static CFBoolean {
    // SAFETY: reading a CoreFoundation constant.
    let b = unsafe {
        if v {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        }
    };
    b.expect("kCFBooleanTrue/False are always present")
}

// ─── Hardware encoder detection ─────────────────────────────────────────────────────────

/// Whether a hardware-accelerated VideoToolbox encoder exists for `codec`.
pub fn supports(codec: VideoCodec) -> bool {
    static CACHE: OnceLock<(bool, bool)> = OnceLock::new();
    let (h265, h264) = *CACHE.get_or_init(|| {
        let list = match hardware_encoder_codecs() {
            Ok(list) => list,
            Err(err) => {
                tracing::warn!("VTCopyVideoEncoderList failed: {err:#}");
                Vec::new()
            }
        };
        let has = |c: VideoCodec| list.contains(&codec_type(c));
        (has(VideoCodec::H265), has(VideoCodec::H264))
    });
    match codec {
        VideoCodec::H265 => h265,
        VideoCodec::H264 => h264,
    }
}

/// Codec types (fourcc) that have a hardware-accelerated encoder.
fn hardware_encoder_codecs() -> Result<Vec<CMVideoCodecType>> {
    let mut raw: *const CFArray = std::ptr::null();
    // SAFETY: `raw` receives a +1 CFArray on success.
    let status = unsafe { VTCopyVideoEncoderList(None, NonNull::from(&mut raw)) };
    if status != 0 || raw.is_null() {
        bail!("status {status}");
    }
    // SAFETY: the array is owned by us (+1) and holds CFDictionaries.
    let list: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw as *mut _)) };
    let mut codecs = Vec::new();
    for i in 0..list.len() {
        let Some(entry) = list.get(i) else { continue };
        // SAFETY: reading CoreFoundation constants.
        let (codec_key, hw_key) = unsafe {
            (
                kVTVideoEncoderList_CodecType,
                kVTVideoEncoderList_IsHardwareAccelerated,
            )
        };
        let hardware = entry
            .get(hw_key)
            .and_then(|v| v.downcast::<CFBoolean>().ok())
            .map(|b| b.as_bool())
            .unwrap_or(false);
        let codec = entry
            .get(codec_key)
            .and_then(|v| v.downcast::<CFNumber>().ok())
            .and_then(|n| n.as_i64());
        if let (true, Some(codec)) = (hardware, codec) {
            codecs.push(codec as CMVideoCodecType);
        }
    }
    Ok(codecs)
}

// ─── Output callback state ──────────────────────────────────────────────────────────────

struct Shared {
    codec: VideoCodec,
    tx: Sender<EncodedFrame>,
    /// Last non-zero status reported by the callback.
    last_error: AtomicI32,
    dropped_by_encoder: AtomicU64,
    frames_out: AtomicU64,
}

/// VideoToolbox output callback: converts the sample to Annex-B and queues it.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: refcon is an `Arc<Shared>` raw pointer kept alive until the session is
    // invalidated (see `Drop for VtEncoder`); we only borrow it here.
    let shared: &Shared = unsafe { &*(refcon as *const Shared) };
    if status != 0 {
        shared.last_error.store(status, Ordering::Relaxed);
        return;
    }
    if flags.contains(VTEncodeInfoFlags::FrameDropped) || sample.is_null() {
        shared.dropped_by_encoder.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: VideoToolbox passes a valid sample buffer for the duration of the callback.
    let sample: &CMSampleBuffer = unsafe { &*sample };
    match sample_to_annexb(sample, shared.codec) {
        Ok(frame) => {
            shared.frames_out.fetch_add(1, Ordering::Relaxed);
            // Drop on full: `encode()` drains this on every call, so a full queue means the
            // consumer stalled; the next keyframe request will recover the stream.
            let _ = shared.tx.try_send(frame);
        }
        Err(err) => {
            tracing::warn!("VideoToolbox output conversion failed: {err:#}");
            shared.last_error.store(-1, Ordering::Relaxed);
        }
    }
}

/// Whether the sample is a sync sample (keyframe): `NotSync` attachment absent or false.
fn is_keyframe(sample: &CMSampleBuffer) -> bool {
    // SAFETY: plain accessor.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    // SAFETY: attachment arrays hold CFDictionaries keyed by CFString.
    let Some(dict) =
        (unsafe { attachments.cast_unchecked::<CFDictionary<CFString, CFType>>() }).get(0)
    else {
        return true;
    };
    // SAFETY: reading a CoreFoundation constant.
    let key = unsafe { kCMSampleAttachmentKey_NotSync };
    match dict.get(key).and_then(|v| v.downcast::<CFBoolean>().ok()) {
        Some(not_sync) => !not_sync.as_bool(),
        None => true,
    }
}

/// Collect all parameter sets (VPS/SPS/PPS or SPS/PPS) from the format description.
fn parameter_sets(desc: &CMFormatDescription, codec: VideoCodec) -> Result<(Vec<Vec<u8>>, usize)> {
    let mut sets = Vec::new();
    let mut count: usize = 0;
    let mut nal_len: i32 = 4;
    let mut index = 0;
    loop {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size: usize = 0;
        // SAFETY: out-pointers are valid; the returned parameter-set pointer is owned by
        // the format description, which outlives this loop.
        let status = unsafe {
            match codec {
                VideoCodec::H265 => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    desc,
                    index,
                    &mut ptr,
                    &mut size,
                    &mut count,
                    &mut nal_len,
                ),
                VideoCodec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    desc,
                    index,
                    &mut ptr,
                    &mut size,
                    &mut count,
                    &mut nal_len,
                ),
            }
        };
        if status != 0 {
            bail!("parameter set {index}: status {status}");
        }
        if ptr.is_null() || size == 0 {
            bail!("parameter set {index} is empty");
        }
        // SAFETY: ptr/size describe a valid buffer owned by `desc`.
        sets.push(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec());
        index += 1;
        if index >= count {
            break;
        }
    }
    if !(1..=4).contains(&nal_len) {
        bail!("unexpected NAL length prefix size {nal_len}");
    }
    Ok((sets, nal_len as usize))
}

/// Copy the sample's block buffer into a Vec.
fn sample_bytes(sample: &CMSampleBuffer) -> Result<Vec<u8>> {
    // SAFETY: plain accessor.
    let block = unsafe { sample.data_buffer() }.context("sample has no data buffer")?;
    // SAFETY: plain accessor.
    let len = unsafe { block.data_length() };
    let mut out = vec![0u8; len];
    if len > 0 {
        // SAFETY: `out` has exactly `len` bytes.
        let status = unsafe {
            block.copy_data_bytes(
                0,
                len,
                NonNull::new_unchecked(out.as_mut_ptr() as *mut c_void),
            )
        };
        if status != 0 {
            bail!("CMBlockBufferCopyDataBytes failed: {status}");
        }
    }
    Ok(out)
}

fn sample_to_annexb(sample: &CMSampleBuffer, codec: VideoCodec) -> Result<EncodedFrame> {
    let keyframe = is_keyframe(sample);
    // SAFETY: plain accessor.
    let desc =
        unsafe { sample.format_description() }.context("sample has no format description")?;
    let (sets, nal_len) = parameter_sets(&desc, codec)?;
    let data = sample_bytes(sample)?;
    let mut out = Vec::with_capacity(data.len() + 128);
    if keyframe {
        for set in &sets {
            annexb::push_nal(&mut out, set);
        }
    }
    let nals = annexb::lengths_to_annexb(&data, nal_len, &mut out);
    if nals == 0 {
        bail!("sample contained no NAL units");
    }
    // SAFETY: plain accessor.
    let pts_seconds = unsafe { sample.presentation_time_stamp().seconds() };
    let pts = if pts_seconds.is_finite() && pts_seconds >= 0.0 {
        Duration::from_secs_f64(pts_seconds)
    } else {
        Duration::ZERO
    };
    Ok(EncodedFrame {
        data: Bytes::from(out),
        keyframe,
        pts,
    })
}

// ─── Encoder ────────────────────────────────────────────────────────────────────────────

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    shared: Arc<Shared>,
    /// Extra strong reference handed to VideoToolbox as refcon; released after invalidate.
    refcon: *const Shared,
    rx: Receiver<EncodedFrame>,
    /// Source (capture) configuration; `cfg.width/height` is the input size.
    cfg: EncoderConfig,
    /// Encoded output size (≤ `max_output_size`), may be smaller than the input.
    output: (u32, u32),
    pool: Option<CFRetained<CVPixelBufferPool>>,
    started: Option<Instant>,
    frames_in: u64,
    invalidated: bool,
}

// SAFETY: the compression session is used from one thread at a time; VideoToolbox itself
// is thread-safe and the callback only touches atomics and a channel.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    /// Size of the encoded pictures (smaller than the source when it exceeds the codec limit).
    pub fn output_size(&self) -> (u32, u32) {
        self.output
    }

    fn set_property(&self, key: &CFString, value: &CFType, required: bool) -> Result<()> {
        // SAFETY: valid session and CF objects.
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if status != 0 {
            if required {
                bail!("VTSessionSetProperty({key}) failed: {status}");
            }
            tracing::debug!("VideoToolbox property {key} not applied (status {status})");
        }
        Ok(())
    }

    fn set_bool(&self, key: &CFString, v: bool, required: bool) -> Result<()> {
        self.set_property(key, cf_bool(v), required)
    }

    fn set_i32(&self, key: &CFString, v: i32, required: bool) -> Result<()> {
        self.set_property(key, &CFNumber::new_i32(v), required)
    }

    fn set_f64(&self, key: &CFString, v: f64, required: bool) -> Result<()> {
        self.set_property(key, &CFNumber::new_f64(v), required)
    }

    fn set_string(&self, key: &CFString, v: &CFString, required: bool) -> Result<()> {
        self.set_property(key, v, required)
    }

    fn apply_bitrate(&self, kbps: u32) -> Result<()> {
        let bps = i32::try_from(u64::from(kbps) * 1000).unwrap_or(i32::MAX);
        // SAFETY: reading CoreFoundation constants.
        let (avg_key, limits_key) = unsafe {
            (
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_DataRateLimits,
            )
        };
        self.set_i32(avg_key, bps, true)?;
        // Hard cap: 125 % of the average over a one second window.
        let bytes_per_second = (f64::from(bps) / 8.0 * 1.25).round();
        let limits = CFArray::from_retained_objects(&[
            CFNumber::new_f64(bytes_per_second),
            CFNumber::new_f64(1.0),
        ]);
        self.set_property(limits_key, limits.as_opaque(), false)
    }

    fn configure(&self) -> Result<()> {
        let fps = self.cfg.fps.max(1);
        // SAFETY: reading CoreFoundation constants.
        unsafe {
            self.set_bool(kVTCompressionPropertyKey_RealTime, true, true)?;
            self.set_bool(kVTCompressionPropertyKey_AllowFrameReordering, false, true)?;
            self.set_bool(
                kVTCompressionPropertyKey_AllowTemporalCompression,
                true,
                false,
            )?;
            self.set_i32(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                (fps * 2) as i32,
                false,
            )?;
            self.set_f64(
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                2.0,
                false,
            )?;
            self.set_f64(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                f64::from(fps),
                false,
            )?;
            self.set_i32(kVTCompressionPropertyKey_MaxFrameDelayCount, 0, false)?;
            self.set_bool(
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                true,
                false,
            )?;
            self.set_string(
                kVTCompressionPropertyKey_ColorPrimaries,
                kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
                false,
            )?;
            self.set_string(
                kVTCompressionPropertyKey_TransferFunction,
                kCMFormatDescriptionTransferFunction_ITU_R_709_2,
                false,
            )?;
            self.set_string(
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
                false,
            )?;
            match self.cfg.codec {
                VideoCodec::H265 => {
                    self.set_string(
                        kVTCompressionPropertyKey_ProfileLevel,
                        kVTProfileLevel_HEVC_Main_AutoLevel,
                        false,
                    )?;
                }
                VideoCodec::H264 => {
                    if self
                        .set_string(
                            kVTCompressionPropertyKey_ProfileLevel,
                            kVTProfileLevel_H264_High_AutoLevel,
                            false,
                        )
                        .is_err()
                    {
                        self.set_string(
                            kVTCompressionPropertyKey_ProfileLevel,
                            kVTProfileLevel_H264_Main_AutoLevel,
                            false,
                        )?;
                    }
                    self.set_string(
                        kVTCompressionPropertyKey_H264EntropyMode,
                        kVTH264EntropyMode_CABAC,
                        false,
                    )?;
                }
            }
        }
        self.apply_bitrate(self.cfg.bitrate_kbps)?;
        // SAFETY: valid session.
        let status = unsafe { self.session.prepare_to_encode_frames() };
        if status != 0 {
            bail!("VTCompressionSessionPrepareToEncodeFrames failed: {status}");
        }
        Ok(())
    }

    fn pixel_buffer_pool(&mut self) -> Result<&CVPixelBufferPool> {
        if self.pool.is_none() {
            // SAFETY: reading CoreFoundation constants.
            let (fmt_key, w_key, h_key, surf_key) = unsafe {
                (
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                )
            };
            let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::empty();
            let fmt = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
            let w = CFNumber::new_i32(self.cfg.width as i32);
            let h = CFNumber::new_i32(self.cfg.height as i32);
            let attrs: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(
                &[fmt_key, w_key, h_key, surf_key],
                &[&fmt, &w, &h, &empty],
            );
            let mut raw: *mut CVPixelBufferPool = std::ptr::null_mut();
            // SAFETY: `raw` receives a +1 pool on success.
            let ret = unsafe {
                CVPixelBufferPool::create(
                    None,
                    None,
                    Some(attrs.as_opaque()),
                    NonNull::from(&mut raw),
                )
            };
            if ret != 0 || raw.is_null() {
                bail!("CVPixelBufferPoolCreate failed: {ret}");
            }
            // SAFETY: +1 reference from create.
            self.pool = Some(unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) });
        }
        Ok(self.pool.as_deref().expect("pool initialised above"))
    }

    /// Upload a CPU BGRA frame into a pooled pixel buffer.
    fn upload_bgra(
        &mut self,
        data: &[u8],
        stride: usize,
        width: u32,
        height: u32,
    ) -> Result<CFRetained<CVPixelBuffer>> {
        if width != self.cfg.width || height != self.cfg.height {
            bail!(
                "frame {width}x{height} does not match encoder {}x{}",
                self.cfg.width,
                self.cfg.height
            );
        }
        let pool = self.pixel_buffer_pool()?;
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: `raw` receives a +1 pixel buffer on success.
        let ret =
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut raw)) };
        if ret != 0 || raw.is_null() {
            bail!("CVPixelBufferPoolCreatePixelBuffer failed: {ret}");
        }
        // SAFETY: +1 reference from create.
        let pb: CFRetained<CVPixelBuffer> =
            unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };
        let row_bytes = width as usize * 4;
        // SAFETY: lock/unlock paired; we write within the buffer's bounds.
        unsafe {
            let ret = CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
            if ret != 0 {
                bail!("CVPixelBufferLockBaseAddress failed: {ret}");
            }
            let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
            let dst_stride = CVPixelBufferGetBytesPerRow(&pb);
            let result = if base.is_null() {
                Err(anyhow!("pooled pixel buffer has no base address"))
            } else if data.len() < stride * (height as usize - 1) + row_bytes {
                Err(anyhow!(
                    "BGRA buffer too small for {width}x{height} stride {stride}"
                ))
            } else {
                for row in 0..height as usize {
                    std::ptr::copy_nonoverlapping(
                        data.as_ptr().add(row * stride),
                        base.add(row * dst_stride),
                        row_bytes,
                    );
                }
                Ok(())
            };
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
            result?;
        }
        Ok(pb)
    }

    fn drain(&self, out: &mut Vec<EncodedFrame>) {
        while let Ok(f) = self.rx.try_recv() {
            out.push(f);
        }
    }

    fn invalidate(&mut self) {
        if self.invalidated {
            return;
        }
        self.invalidated = true;
        // SAFETY: valid session; after invalidate no callbacks fire, so the refcon can go.
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
            drop(Arc::from_raw(self.refcon));
        }
    }
}

impl Encoder for VtEncoder {
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let start = *self.started.get_or_insert(frame.captured_at);
        let pts_us = frame
            .captured_at
            .saturating_duration_since(start)
            .as_micros() as i64;
        // Keep timestamps strictly increasing even if two frames share a capture instant.
        let pts_us = pts_us.max(self.frames_in as i64);
        let fps = self.cfg.fps.max(1) as i64;

        let uploaded;
        let image: &CVPixelBuffer = match &frame.data {
            FrameData::PixelBuffer(pb) => {
                if (pb.width(), pb.height()) != (self.cfg.width, self.cfg.height) {
                    bail!(
                        "frame {}x{} does not match encoder {}x{}",
                        pb.width(),
                        pb.height(),
                        self.cfg.width,
                        self.cfg.height
                    );
                }
                pb.as_cv()
            }
            FrameData::Bgra { data, stride } => {
                uploaded = self.upload_bgra(data, *stride, frame.width, frame.height)?;
                &uploaded
            }
        };

        let props: Option<CFRetained<CFDictionary<CFString, CFType>>> = if force_keyframe {
            // SAFETY: reading a CoreFoundation constant.
            let key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
            Some(CFDictionary::from_slices(&[key], &[cf_bool(true).as_ref()]))
        } else {
            None
        };

        let mut flags = VTEncodeInfoFlags::empty();
        // SAFETY: valid session and image buffer; CMTime values are plain data.
        let status = unsafe {
            self.session.encode_frame(
                image,
                CMTime::new(pts_us, TIMESCALE),
                CMTime::new(TIMESCALE as i64 / fps, TIMESCALE),
                props.as_deref().map(|d| d.as_opaque()),
                std::ptr::null_mut(),
                &mut flags,
            )
        };
        if status != 0 {
            bail!("VTCompressionSessionEncodeFrame failed: {status}");
        }
        self.frames_in += 1;

        let mut out = Vec::with_capacity(1);
        self.drain(&mut out);
        if out.is_empty() {
            // Low-latency mode normally completes synchronously; flush if it did not.
            // SAFETY: valid session.
            unsafe { self.session.complete_frames(kCMTimeInvalid) };
            self.drain(&mut out);
        }
        let err = self.shared.last_error.swap(0, Ordering::Relaxed);
        if err != 0 {
            bail!("VideoToolbox reported status {err}");
        }
        Ok(out)
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.cfg.bitrate_kbps = kbps;
        self.apply_bitrate(kbps)
    }

    fn codec(&self) -> VideoCodec {
        self.cfg.codec
    }

    fn is_hardware(&self) -> bool {
        true
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        self.invalidate();
        tracing::debug!(
            frames_in = self.frames_in,
            frames_out = self.shared.frames_out.load(Ordering::Relaxed),
            dropped = self.shared.dropped_by_encoder.load(Ordering::Relaxed),
            "VideoToolbox session closed"
        );
    }
}

pub fn create(cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
    if !supports(cfg.codec) {
        bail!("no hardware {:?} encoder on this machine", cfg.codec);
    }
    if cfg.width == 0 || cfg.height == 0 {
        bail!("invalid encoder size {}x{}", cfg.width, cfg.height);
    }
    let (tx, rx) = crossbeam_channel::bounded(OUTPUT_QUEUE);
    let shared = Arc::new(Shared {
        codec: cfg.codec,
        tx,
        last_error: AtomicI32::new(0),
        dropped_by_encoder: AtomicU64::new(0),
        frames_out: AtomicU64::new(0),
    });
    let refcon = Arc::into_raw(shared.clone());

    // SAFETY: reading CoreFoundation constants.
    let (require_hw_key, fmt_key, w_key, h_key) = unsafe {
        (
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
        )
    };
    let spec: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[require_hw_key], &[cf_bool(true).as_ref()]);
    let fmt = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
    let w = CFNumber::new_i32(cfg.width as i32);
    let h = CFNumber::new_i32(cfg.height as i32);
    let source_attrs: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[fmt_key, w_key, h_key], &[&fmt, &w, &h]);

    let output = fit_size(cfg.width, cfg.height, max_output_size(cfg.codec));
    if output != (cfg.width, cfg.height) {
        tracing::info!(
            codec = ?cfg.codec,
            "source {}x{} exceeds the hardware limit; encoding at {}x{}",
            cfg.width,
            cfg.height,
            output.0,
            output.1
        );
    }
    let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
    // SAFETY: all pointers are valid; refcon stays alive until invalidate (see Drop).
    let status = unsafe {
        VTCompressionSession::create(
            None,
            output.0 as i32,
            output.1 as i32,
            codec_type(cfg.codec),
            Some(spec.as_opaque()),
            Some(source_attrs.as_opaque()),
            None,
            Some(output_callback),
            refcon as *mut c_void,
            NonNull::from(&mut raw),
        )
    };
    if status != 0 || raw.is_null() {
        // SAFETY: the session was not created, so nobody else holds the refcon.
        unsafe { drop(Arc::from_raw(refcon)) };
        bail!(
            "VTCompressionSessionCreate({:?}) failed: {status}",
            cfg.codec
        );
    }
    // SAFETY: +1 reference from create.
    let session = unsafe { CFRetained::from_raw(NonNull::new_unchecked(raw)) };

    let mut enc = VtEncoder {
        session,
        shared,
        refcon,
        rx,
        cfg: cfg.clone(),
        output,
        pool: None,
        started: None,
        frames_in: 0,
        invalidated: false,
    };
    if let Err(err) = enc.configure() {
        enc.invalidate();
        return Err(err);
    }
    tracing::info!(
        codec = ?cfg.codec,
        width = output.0,
        height = output.1,
        fps = cfg.fps,
        bitrate_kbps = cfg.bitrate_kbps,
        "VideoToolbox hardware encoder ready"
    );
    Ok(Box::new(enc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{create_capturer, CaptureConfig, Capturer};

    fn synthetic_frame(width: u32, height: u32, t: u32) -> Frame {
        let stride = width as usize * 4;
        let mut data = vec![0u8; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let o = y * stride + x * 4;
                data[o] = ((x as u32 + t * 7) & 0xff) as u8;
                data[o + 1] = ((y as u32 + t * 3) & 0xff) as u8;
                data[o + 2] = (((x ^ y) as u32 + t) & 0xff) as u8;
                data[o + 3] = 0xff;
            }
        }
        Frame {
            width,
            height,
            captured_at: Instant::now(),
            data: FrameData::Bgra { data, stride },
        }
    }

    fn nal_types(frame: &EncodedFrame, codec: VideoCodec) -> Vec<u8> {
        annexb_nals(&frame.data)
            .iter()
            .map(|n| match codec {
                VideoCodec::H265 => hevc_nal_type(n).unwrap(),
                VideoCodec::H264 => h264_nal_type(n).unwrap(),
            })
            .collect()
    }

    fn check_stream(frames: &[EncodedFrame], codec: VideoCodec) {
        assert!(!frames.is_empty(), "no encoded frames");
        assert!(frames[0].keyframe, "first output must be a keyframe");
        for f in frames {
            assert!(f.data.starts_with(&[0, 0, 0, 1]), "Annex-B start code");
            let types = nal_types(f, codec);
            if f.keyframe {
                match codec {
                    VideoCodec::H265 => {
                        assert_eq!(
                            &types[..3],
                            &[32, 33, 34],
                            "VPS/SPS/PPS before IDR: {types:?}"
                        );
                        assert!(
                            types[3..].iter().any(|t| (16..=21).contains(t)),
                            "IRAP slice: {types:?}"
                        );
                    }
                    VideoCodec::H264 => {
                        assert_eq!(&types[..2], &[7, 8], "SPS/PPS before IDR: {types:?}");
                        assert!(types[2..].contains(&5), "IDR slice: {types:?}");
                    }
                }
            } else {
                match codec {
                    VideoCodec::H265 => assert!(
                        types.iter().all(|t| *t < 32),
                        "no param sets in P-frame: {types:?}"
                    ),
                    VideoCodec::H264 => {
                        assert!(!types.contains(&7) && !types.contains(&8), "{types:?}")
                    }
                }
            }
        }
    }

    /// Encode synthetic CPU frames (no Screen Recording permission needed).
    fn encode_synthetic(codec: VideoCodec) {
        if !supports(codec) {
            eprintln!("no hardware {codec:?} encoder on this machine; skipping");
            return;
        }
        let cfg = EncoderConfig {
            codec,
            width: 640,
            height: 360,
            fps: 30,
            bitrate_kbps: 2000,
        };
        let mut enc = create(&cfg).expect("create encoder");
        assert!(enc.is_hardware());
        assert_eq!(enc.codec(), codec);
        let mut frames = Vec::new();
        for t in 0..40 {
            let f = synthetic_frame(640, 360, t);
            frames.extend(enc.encode(&f, false).expect("encode"));
        }
        check_stream(&frames, codec);
        let before = frames.len();
        // Force a keyframe: the next output frame must be a sync sample.
        let f = synthetic_frame(640, 360, 99);
        let mut forced = enc.encode(&f, true).expect("encode forced");
        if forced.is_empty() {
            let f = synthetic_frame(640, 360, 100);
            forced = enc.encode(&f, false).expect("encode after forced");
        }
        assert!(
            forced.first().map(|f| f.keyframe).unwrap_or(false),
            "forced keyframe"
        );
        check_stream(&forced, codec);
        enc.set_bitrate(500).expect("set_bitrate");
        let total: usize = frames.iter().map(|f| f.data.len()).sum();
        eprintln!(
            "{codec:?}: {before} frames, avg {} bytes/frame, keyframes {}",
            total / before.max(1),
            frames.iter().filter(|f| f.keyframe).count()
        );
        assert!(total / before.max(1) < 200_000, "frames unreasonably large");
    }

    #[test]
    fn fit_size_respects_codec_limits() {
        assert_eq!(
            fit_size(1920, 1080, max_output_size(VideoCodec::H264)),
            (1920, 1080)
        );
        assert_eq!(
            fit_size(5120, 2160, max_output_size(VideoCodec::H265)),
            (5120, 2160)
        );
        let (w, h) = fit_size(5120, 2160, max_output_size(VideoCodec::H264));
        assert!(w <= 4096 && h <= 2304 && w % 2 == 0 && h % 2 == 0);
        assert!(
            (f64::from(w) / f64::from(h) - 5120.0 / 2160.0).abs() < 0.01,
            "{w}x{h}"
        );
        assert_eq!(fit_size(6016, 3384, (4096, 2304)), (4096, 2304));
        assert_eq!(fit_size(1001, 601, (4096, 2304)), (1000, 600));
    }

    /// 5K sources exceed the hardware H.264 limit; VideoToolbox must scale them down.
    #[test]
    fn macos_encode_h264_scales_5k_source() {
        if !supports(VideoCodec::H264) {
            return;
        }
        let cfg = EncoderConfig {
            codec: VideoCodec::H264,
            width: 5120,
            height: 2160,
            fps: 30,
            bitrate_kbps: 8000,
        };
        let mut enc = create(&cfg).expect("create 5K H264 encoder");
        let mut frames = Vec::new();
        for t in 0..6 {
            frames.extend(
                enc.encode(&synthetic_frame(5120, 2160, t), false)
                    .expect("encode 5K frame"),
            );
        }
        check_stream(&frames, VideoCodec::H264);
    }

    #[test]
    fn macos_hardware_encoder_detection() {
        let h265 = supports(VideoCodec::H265);
        let h264 = supports(VideoCodec::H264);
        eprintln!("hardware HEVC: {h265}, hardware H264: {h264}");
        // Every Mac VideoToolbox supports since 2015 has hardware H.264.
        assert!(h264, "expected a hardware H.264 encoder");
    }

    #[test]
    fn macos_encode_synthetic_h264() {
        encode_synthetic(VideoCodec::H264);
    }

    #[test]
    fn macos_encode_synthetic_h265() {
        encode_synthetic(VideoCodec::H265);
    }

    /// Full capture → encode path; needs the Screen Recording permission.
    #[test]
    #[ignore]
    fn macos_capture_and_encode_display0() {
        for codec in [VideoCodec::H265, VideoCodec::H264] {
            if !supports(codec) {
                eprintln!("skipping {codec:?}: no hardware encoder");
                continue;
            }
            let mut cap: Box<dyn Capturer> = create_capturer(&CaptureConfig {
                display_index: 0,
                max_fps: 30,
                show_cursor: true,
            })
            .expect("capturer");
            let (w, h) = cap.size();
            let cfg = EncoderConfig {
                codec,
                width: w,
                height: h,
                fps: 30,
                bitrate_kbps: 8000,
            };
            let mut enc = create(&cfg).expect("encoder");
            let mut frames = Vec::new();
            let mut latencies = Vec::new();
            let start = Instant::now();
            let mut captured = 0;
            while captured < 30 && start.elapsed() < Duration::from_secs(4) {
                let Some(frame) = cap
                    .next_frame(Duration::from_millis(100))
                    .expect("next_frame")
                else {
                    continue;
                };
                captured += 1;
                let out = enc.encode(&frame, captured == 15).expect("encode");
                if !out.is_empty() {
                    latencies.push(frame.captured_at.elapsed());
                }
                frames.extend(out);
            }
            cap.stop();
            check_stream(&frames, codec);
            assert!(
                frames.iter().skip(1).any(|f| f.keyframe),
                "forced keyframe present"
            );
            let avg_latency = latencies.iter().sum::<Duration>() / latencies.len().max(1) as u32;
            let max_latency = latencies.iter().max().copied().unwrap_or_default();
            let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
            eprintln!(
                "{codec:?} {w}x{h}: captured {captured}, encoded {} frames in {:?}; capture→encode avg {avg_latency:?} max {max_latency:?}; avg {} bytes/frame",
                frames.len(),
                start.elapsed(),
                bytes / frames.len().max(1)
            );
            assert!(frames.len() >= 10, "encoded only {} frames", frames.len());
            assert!(
                avg_latency < Duration::from_millis(50),
                "avg latency {avg_latency:?} too high"
            );
        }
    }
}
