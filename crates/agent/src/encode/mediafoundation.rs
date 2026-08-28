//! Windows hardware encoding via Media Foundation transforms (H.264 / HEVC encoder MFTs).
//!
//! * [`supports`] enumerates hardware encoder MFTs for the codec (cached).
//! * [`create`] activates the first hardware MFT as an *asynchronous* transform, binds it to
//!   the D3D11 device that produced the captured textures through an `IMFDXGIDeviceManager`,
//!   configures low-latency CBR encoding through `ICodecAPI` and converts every BGRA capture
//!   texture to NV12 on the GPU with `ID3D11VideoProcessor` before handing it to the MFT as a
//!   DXGI surface buffer — nothing is copied to system memory.
//! * Output samples are Annex-B; parameter sets are guaranteed in front of every keyframe
//!   (prepended from `MF_MT_MPEG_SEQUENCE_HEADER` when the MFT omits them).
//!
//! The encoder is driven from the capture thread: [`Encoder::encode`] submits one input
//! sample and drains whatever output the MFT has ready, waiting briefly for the output of
//! the frame just submitted so end-to-end latency stays around one frame.

use super::{EncodedFrame, Encoder, EncoderConfig};
use crate::capture::windows::D3dDevice;
use crate::capture::{Frame, FrameData};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use protocol::common::VideoCodec;
use std::mem::ManuallyDrop;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_FLAG,
    D3D11_RESOURCE_MISC_FLAG, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_USAGE_DEFAULT,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL,
};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncCommonRealTime, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFAttributes, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, METransformHaveOutput, METransformNeedInput, MFCreateDXGIDeviceManager,
    MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFNominalRange_16_235, MFSampleExtension_CleanPoint, MFStartup, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFVideoTransferMatrix_BT709, MFSTARTUP_NOSOCKET, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_EVENT_FLAG_NO_WAIT, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};

/// 100 ns units per second (Media Foundation time base).
const HNS_PER_SEC: i64 = 10_000_000;
/// How long `encode()` waits for the MFT to accept input before dropping the frame.
const NEED_INPUT_WAIT: Duration = Duration::from_millis(50);
/// Number of pooled NV12 input textures (the MFT may hold one while we convert the next).
const NV12_POOL: usize = 3;

// ─── Process-wide initialisation ────────────────────────────────────────────────────────

fn init_thread() -> Result<()> {
    // SAFETY: standard COM/MF start-up; RPC_E_CHANGED_MODE just means another apartment
    // model is already active on this thread, which is fine for MF.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() && hr.0 != 0x8001_0106_u32 as i32 {
            bail!("CoInitializeEx failed: {hr}");
        }
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).context("MFStartup")?;
    }
    Ok(())
}

fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H265 => MFVideoFormat_HEVC,
        VideoCodec::H264 => MFVideoFormat_H264,
    }
}

/// Enumerate hardware encoder activations for `codec`, best first.
fn enumerate_hardware(codec: VideoCodec) -> Result<Vec<IMFActivate>> {
    init_thread()?;
    let out_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let in_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MFTEnumEx allocates the array with CoTaskMemAlloc; we take ownership of the
    // interface pointers and free the array below.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&in_type),
            Some(&out_type),
            &mut activates,
            &mut count,
        )
        .context("MFTEnumEx")?;
        let mut list = Vec::with_capacity(count as usize);
        if !activates.is_null() {
            for i in 0..count as usize {
                if let Some(a) = (*activates.add(i)).take() {
                    list.push(a);
                }
            }
            CoTaskMemFree(Some(activates.cast()));
        }
        Ok(list)
    }
}

/// Whether a hardware encoder MFT exists for `codec` (cached per codec).
pub fn supports(codec: VideoCodec) -> bool {
    static CACHE: OnceLock<(bool, bool)> = OnceLock::new();
    let (h265, h264) = *CACHE.get_or_init(|| {
        let has = |c: VideoCodec| match enumerate_hardware(c) {
            Ok(list) => !list.is_empty(),
            Err(err) => {
                tracing::warn!("Media Foundation encoder enumeration failed: {err:#}");
                false
            }
        };
        (has(VideoCodec::H265), has(VideoCodec::H264))
    });
    match codec {
        VideoCodec::H265 => h265,
        VideoCodec::H264 => h264,
    }
}

// ─── VARIANT helpers for ICodecAPI ──────────────────────────────────────────────────────

fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

fn variant_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: if v {
                        VARIANT_TRUE
                    } else {
                        windows::Win32::Foundation::VARIANT_FALSE
                    },
                },
            }),
        },
    }
}

fn codec_set(api: &ICodecAPI, key: &GUID, value: VARIANT, what: &str) -> bool {
    // SAFETY: VARIANT holds a plain integer; ICodecAPI copies it.
    match unsafe { api.SetValue(key, &value) } {
        Ok(()) => true,
        Err(err) => {
            tracing::debug!("ICodecAPI {what} not accepted: {err}");
            false
        }
    }
}

fn pack_u64(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

// ─── GPU colour conversion (BGRA → NV12) ────────────────────────────────────────────────

struct Converter {
    device: Arc<D3dDevice>,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    width: u32,
    height: u32,
    nv12_pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    pool_next: usize,
    /// Upload target for CPU BGRA frames.
    upload: Option<ID3D11Texture2D>,
}

impl Converter {
    fn new(device: Arc<D3dDevice>, width: u32, height: u32, fps: u32) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device
            .device
            .cast()
            .context("ID3D11VideoDevice (device lacks video support)")?;
        let video_context: ID3D11VideoContext =
            device.context.cast().context("ID3D11VideoContext")?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: fps,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: desc is fully initialised.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc) }
            .context("CreateVideoProcessorEnumerator")?;
        // SAFETY: enumerator is valid; rate conversion index 0 is always present.
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor")?;
        // Colour spaces: RGB input full range → YCbCr BT.709 limited range output.
        // D3D11_VIDEO_PROCESSOR_COLOR_SPACE bitfield: Usage(1) RGB_Range(1) YCbCr_Matrix(1) YCbCr_xvYCC(1) Nominal_Range(2).
        let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 }; // playback, full RGB range, BT.601 matrix bit unused
        let yuv_709_limited = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: (1 << 2) | (1 << 4),
        }; // YCbCr_Matrix=1 (709), Nominal_Range=1 (16-235)
           // SAFETY: plain COM calls on valid objects.
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &rgb_full);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &yuv_709_limited);
        }
        let mut me = Self {
            device,
            video_device,
            video_context,
            enumerator,
            processor,
            width,
            height,
            nv12_pool: Vec::new(),
            pool_next: 0,
            upload: None,
        };
        for _ in 0..NV12_POOL {
            let tex = me.device.create_texture(
                width,
                height,
                DXGI_FORMAT_NV12,
                D3D11_USAGE_DEFAULT,
                D3D11_BIND_RENDER_TARGET,
                D3D11_CPU_ACCESS_FLAG(0),
                D3D11_RESOURCE_MISC_FLAG(0),
            )?;
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view = None;
            // SAFETY: tex is a render-target NV12 texture matching the enumerator.
            unsafe {
                me.video_device.CreateVideoProcessorOutputView(
                    &tex,
                    &me.enumerator,
                    &view_desc,
                    Some(&mut view),
                )
            }
            .context("CreateVideoProcessorOutputView")?;
            let view = view.ok_or_else(|| anyhow!("no output view"))?;
            me.nv12_pool.push((tex, view));
        }
        Ok(me)
    }

    /// Convert `src` (BGRA, on this converter's device) into the next pooled NV12 texture.
    fn convert(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY: src is a 2D BGRA texture on the same device.
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                src,
                &self.enumerator,
                &in_desc,
                Some(&mut in_view),
            )
        }
        .context("CreateVideoProcessorInputView")?;
        let in_view = in_view.ok_or_else(|| anyhow!("no input view"))?;
        let idx = self.pool_next;
        self.pool_next = (self.pool_next + 1) % self.nv12_pool.len();
        let (tex, out_view) = &self.nv12_pool[idx];
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(in_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        }];
        // SAFETY: all views belong to this processor; the stream struct outlives the call.
        let result = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, out_view, 0, &streams)
        };
        // Release the input view we moved into the (ManuallyDrop) stream descriptor.
        // SAFETY: the field was initialised with a live interface above and is dropped once.
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result.context("VideoProcessorBlt")?;
        Ok(tex.clone())
    }

    /// Upload CPU BGRA rows into a device texture (used for `FrameData::Bgra`).
    fn upload_bgra(&mut self, data: &[u8], stride: usize) -> Result<ID3D11Texture2D> {
        let tex = match &self.upload {
            Some(t) => t.clone(),
            None => {
                let t = self.device.create_texture(
                    self.width,
                    self.height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_USAGE_DEFAULT,
                    D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
                    D3D11_CPU_ACCESS_FLAG(0),
                    D3D11_RESOURCE_MISC_FLAG(0),
                )?;
                self.upload = Some(t.clone());
                t
            }
        };
        if data.len() < stride * (self.height as usize - 1) + self.width as usize * 4 {
            bail!("BGRA buffer too small for {}x{}", self.width, self.height);
        }
        // SAFETY: the buffer covers `height` rows of `stride` bytes.
        unsafe {
            self.device.context.UpdateSubresource(
                &tex,
                0,
                None,
                data.as_ptr().cast(),
                stride as u32,
                0,
            );
        }
        Ok(tex)
    }
}

// ─── The encoder ────────────────────────────────────────────────────────────────────────

pub struct MfEncoder {
    codec: VideoCodec,
    cfg: EncoderConfig,
    activate: IMFActivate,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    device_manager: IMFDXGIDeviceManager,
    reset_token: u32,
    converter: Option<Converter>,
    in_stream: u32,
    out_stream: u32,
    provides_samples: bool,
    out_buffer_size: u32,
    need_input: u32,
    streaming: bool,
    started: Option<Instant>,
    frames_in: u64,
    frames_out: u64,
    sequence_header: Vec<u8>,
    force_next_keyframe: bool,
}

// SAFETY: all COM objects are used from the pipeline thread only; MF objects are
// free-threaded.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    fn new(cfg: &EncoderConfig) -> Result<Self> {
        init_thread()?;
        let activates = enumerate_hardware(cfg.codec)?;
        let activate = activates
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no hardware {:?} encoder MFT", cfg.codec))?;
        // SAFETY: plain COM activation.
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.context("activating encoder MFT")?;
        // SAFETY: plain COM calls.
        let attrs: IMFAttributes =
            unsafe { transform.GetAttributes() }.context("IMFTransform::GetAttributes")?;
        unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
            .context("MF_TRANSFORM_ASYNC_UNLOCK")?;
        let events: IMFMediaEventGenerator = transform
            .cast()
            .context("encoder MFT is not asynchronous")?;
        let codec_api: Option<ICodecAPI> = transform.cast().ok();

        let mut reset_token = 0u32;
        let mut device_manager = None;
        // SAFETY: out-pointers valid.
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager) }
            .context("MFCreateDXGIDeviceManager")?;
        let device_manager = device_manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;

        let (mut in_ids, mut out_ids) = ([0u32; 1], [0u32; 1]);
        // SAFETY: arrays sized for one stream each; E_NOTIMPL means ids are 0.
        let (in_stream, out_stream) =
            match unsafe { transform.GetStreamIDs(&mut in_ids, &mut out_ids) } {
                Ok(()) => (in_ids[0], out_ids[0]),
                Err(_) => (0, 0),
            };

        Ok(Self {
            codec: cfg.codec,
            cfg: cfg.clone(),
            activate,
            transform,
            events,
            codec_api,
            device_manager,
            reset_token,
            converter: None,
            in_stream,
            out_stream,
            provides_samples: false,
            out_buffer_size: 0,
            need_input: 0,
            streaming: false,
            started: None,
            frames_in: 0,
            frames_out: 0,
            sequence_header: Vec::new(),
            force_next_keyframe: true,
        })
    }

    /// Bind to `device`, negotiate media types and start streaming. Called on the first frame.
    fn start(&mut self, device: Arc<D3dDevice>) -> Result<()> {
        let (w, h, fps) = (self.cfg.width, self.cfg.height, self.cfg.fps.max(1));
        // SAFETY: plain COM calls; the device outlives the manager binding (we hold an Arc).
        unsafe {
            self.device_manager
                .ResetDevice(&device.device, self.reset_token)
                .context("IMFDXGIDeviceManager::ResetDevice")?;
            let mgr_ptr: *mut std::ffi::c_void = self.device_manager.as_raw();
            self.transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr_ptr as usize)
                .context("MFT_MESSAGE_SET_D3D_MANAGER (encoder is not D3D11 aware)")?;
        }
        self.converter = Some(Converter::new(device, w, h, fps)?);

        // Codec settings that must precede type negotiation on most encoder MFTs.
        if let Some(api) = &self.codec_api {
            codec_set(
                api,
                &CODECAPI_AVLowLatencyMode,
                variant_bool(true),
                "AVLowLatencyMode",
            );
            codec_set(
                api,
                &CODECAPI_AVEncCommonRealTime,
                variant_u32(1),
                "AVEncCommonRealTime",
            );
            codec_set(
                api,
                &CODECAPI_AVEncCommonRateControlMode,
                variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
                "AVEncCommonRateControlMode=CBR",
            );
            codec_set(
                api,
                &CODECAPI_AVEncCommonMeanBitRate,
                variant_u32(self.cfg.bitrate_kbps.saturating_mul(1000)),
                "AVEncCommonMeanBitRate",
            );
            codec_set(
                api,
                &CODECAPI_AVEncMPVGOPSize,
                variant_u32(fps * 2),
                "AVEncMPVGOPSize",
            );
            codec_set(
                api,
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                variant_u32(0),
                "AVEncMPVDefaultBPictureCount",
            );
            // 0 = quality, 100 = speed.
            codec_set(
                api,
                &CODECAPI_AVEncCommonQualityVsSpeed,
                variant_u32(80),
                "AVEncCommonQualityVsSpeed",
            );
        } else {
            tracing::warn!("encoder MFT does not expose ICodecAPI; using defaults");
        }

        // Output type first (encoders require it), then input.
        let out_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
        unsafe {
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &subtype(self.codec))?;
            out_type.SetUINT32(
                &MF_MT_AVG_BITRATE,
                self.cfg.bitrate_kbps.saturating_mul(1000),
            )?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(w, h))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            self.transform
                .SetOutputType(self.out_stream, &out_type, 0)
                .context("SetOutputType")?;
        }
        let in_type = unsafe { MFCreateMediaType() }.context("MFCreateMediaType")?;
        unsafe {
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(w, h))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u64(fps, 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u64(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            in_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            in_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            self.transform
                .SetInputType(self.in_stream, &in_type, 0)
                .context("SetInputType NV12")?;
        }

        // Bitrate is (re)applied through ICodecAPI once the output type is in place; some
        // MFTs only honour it after type negotiation.
        if let Some(api) = &self.codec_api {
            codec_set(
                api,
                &CODECAPI_AVEncCommonMeanBitRate,
                variant_u32(self.cfg.bitrate_kbps.saturating_mul(1000)),
                "AVEncCommonMeanBitRate",
            );
        }

        // SAFETY: plain COM calls.
        let info = unsafe { self.transform.GetOutputStreamInfo(self.out_stream) }
            .context("GetOutputStreamInfo")?;
        self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        self.out_buffer_size = info.cbSize.max(1 << 20);
        self.refresh_sequence_header();

        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("NOTIFY_BEGIN_STREAMING")?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("NOTIFY_START_OF_STREAM")?;
        }
        self.streaming = true;
        tracing::info!(codec = ?self.codec, w, h, fps, kbps = self.cfg.bitrate_kbps, "Media Foundation encoder started");
        Ok(())
    }

    fn refresh_sequence_header(&mut self) {
        // SAFETY: plain COM calls.
        let header = unsafe {
            self.transform
                .GetOutputCurrentType(self.out_stream)
                .ok()
                .and_then(|t: IMFMediaType| {
                    let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
                    let mut buf = vec![0u8; size as usize];
                    t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None)
                        .ok()?;
                    Some(buf)
                })
        };
        if let Some(h) = header {
            self.sequence_header = h;
        }
    }

    /// Drain pending MFT events. Returns finished output samples.
    fn drain_events(&mut self, out: &mut Vec<EncodedFrame>) -> Result<()> {
        loop {
            // SAFETY: NO_WAIT never blocks.
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e).context("IMFMediaEventGenerator::GetEvent"),
            };
            // SAFETY: plain COM call.
            let kind = unsafe { event.GetType() }.context("IMFMediaEvent::GetType")?;
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                if let Some(frame) = self.process_output()? {
                    out.push(frame);
                }
            } else {
                tracing::trace!("MFT event {kind}");
            }
        }
    }

    fn process_output(&mut self) -> Result<Option<EncodedFrame>> {
        let sample = if self.provides_samples {
            None
        } else {
            // SAFETY: plain COM calls.
            let s = unsafe {
                let s = MFCreateSample()?;
                let b = MFCreateMemoryBuffer(self.out_buffer_size)?;
                s.AddBuffer(&b)?;
                s
            };
            Some(s)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.out_stream,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        // SAFETY: buffers is valid for the call; the returned sample is taken below.
        let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
        let sample = ManuallyDrop::into_inner(std::mem::replace(
            &mut buffers[0].pSample,
            ManuallyDrop::new(None),
        ));
        let _events = ManuallyDrop::into_inner(std::mem::replace(
            &mut buffers[0].pEvents,
            ManuallyDrop::new(None),
        ));
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                tracing::debug!("encoder output stream change");
                self.refresh_sequence_header();
                return Ok(None);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) => return Err(e).context("IMFTransform::ProcessOutput"),
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        self.sample_to_frame(&sample).map(Some)
    }

    fn sample_to_frame(&mut self, sample: &IMFSample) -> Result<EncodedFrame> {
        // SAFETY: plain COM calls; Lock/Unlock are paired.
        let (mut data, keyframe, pts_hns) = unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .context("ConvertToContiguousBuffer")?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .context("IMFMediaBuffer::Lock")?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            let _ = buffer.Unlock();
            let keyframe = sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .map(|v| v != 0)
                .unwrap_or(false);
            let pts = sample.GetSampleTime().unwrap_or(0);
            (data, keyframe, pts)
        };
        if !(data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])) {
            bail!("encoder MFT produced non Annex-B output");
        }
        let keyframe = keyframe || has_idr(&data, self.codec);
        if keyframe && !has_parameter_sets(&data, self.codec) {
            if self.sequence_header.is_empty() {
                self.refresh_sequence_header();
            }
            if !self.sequence_header.is_empty() {
                let mut with_ps = Vec::with_capacity(self.sequence_header.len() + data.len());
                with_ps.extend_from_slice(&self.sequence_header);
                with_ps.extend_from_slice(&data);
                data = with_ps;
            } else {
                tracing::warn!("keyframe without parameter sets and no sequence header available");
            }
        }
        self.frames_out += 1;
        Ok(EncodedFrame {
            data: Bytes::from(data),
            keyframe,
            pts: Duration::from_nanos((pts_hns.max(0) as u64) * 100),
        })
    }

    /// Wait (bounded) for the MFT to signal it can take input.
    fn wait_need_input(&mut self, out: &mut Vec<EncodedFrame>) -> Result<bool> {
        let deadline = Instant::now() + NEED_INPUT_WAIT;
        loop {
            self.drain_events(out)?;
            if self.need_input > 0 {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_micros(250));
        }
    }

    fn submit(
        &mut self,
        nv12: &ID3D11Texture2D,
        pts: Duration,
        force_keyframe: bool,
    ) -> Result<()> {
        let fps = self.cfg.fps.max(1) as i64;
        // SAFETY: plain COM calls; the texture is on the device bound to the MFT.
        let sample = unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)
                .context("MFCreateDXGISurfaceBuffer")?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((pts.as_nanos() / 100) as i64)?;
            sample.SetSampleDuration(HNS_PER_SEC / fps)?;
            sample
        };
        if force_keyframe {
            if let Some(api) = &self.codec_api {
                codec_set(
                    api,
                    &CODECAPI_AVEncVideoForceKeyFrame,
                    variant_u32(1),
                    "AVEncVideoForceKeyFrame",
                );
            }
        }
        // SAFETY: plain COM call.
        unsafe { self.transform.ProcessInput(self.in_stream, &sample, 0) }
            .context("IMFTransform::ProcessInput")?;
        self.need_input = self.need_input.saturating_sub(1);
        self.frames_in += 1;
        Ok(())
    }
}

impl Encoder for MfEncoder {
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        if frame.width != self.cfg.width || frame.height != self.cfg.height {
            bail!(
                "frame size {}x{} does not match encoder {}x{}",
                frame.width,
                frame.height,
                self.cfg.width,
                self.cfg.height
            );
        }
        let mut out = Vec::new();

        // Bind to the frame's device on first use.
        if !self.streaming {
            let device = match &frame.data {
                FrameData::D3d11Texture(tex) => Arc::clone(tex.device()),
                FrameData::Bgra { .. } => crate::capture::windows::d3d::shared()?,
            };
            self.start(device)?;
        }
        let converter = self
            .converter
            .as_mut()
            .ok_or_else(|| anyhow!("encoder not started"))?;

        // Get a BGRA texture on the encoder's device.
        let bgra: ID3D11Texture2D = match &frame.data {
            FrameData::D3d11Texture(tex) if Arc::ptr_eq(tex.device(), &converter.device) => {
                tex.texture().clone()
            }
            FrameData::D3d11Texture(tex) => {
                // Different adapter: go through system memory.
                let (data, stride) = tex.to_bgra()?;
                converter.upload_bgra(&data, stride)?
            }
            FrameData::Bgra { data, stride } => converter.upload_bgra(data, *stride)?,
        };
        let nv12 = converter.convert(&bgra)?;

        let start = *self.started.get_or_insert(frame.captured_at);
        let pts = frame.captured_at.saturating_duration_since(start);
        let force = force_keyframe || std::mem::take(&mut self.force_next_keyframe);

        if !self.wait_need_input(&mut out)? {
            tracing::debug!("encoder busy; dropping frame");
            self.force_next_keyframe |= force;
            return Ok(out);
        }
        self.submit(&nv12, pts, force)?;

        // Wait briefly for this frame's output to keep latency at ~1 frame.
        let budget =
            Duration::from_secs_f64(0.5 / self.cfg.fps.max(1) as f64).max(Duration::from_millis(4));
        let deadline = Instant::now() + budget;
        let before = out.len();
        loop {
            self.drain_events(&mut out)?;
            if out.len() > before || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_micros(250));
        }
        Ok(out)
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.cfg.bitrate_kbps = kbps;
        match &self.codec_api {
            Some(api) if self.streaming => {
                if codec_set(
                    api,
                    &CODECAPI_AVEncCommonMeanBitRate,
                    variant_u32(kbps.saturating_mul(1000)),
                    "AVEncCommonMeanBitRate",
                ) {
                    Ok(())
                } else {
                    Err(anyhow!("encoder MFT rejected live bitrate change"))
                }
            }
            _ => Ok(()), // applied at start()
        }
    }

    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn is_hardware(&self) -> bool {
        true
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: orderly shutdown of the MFT; errors are irrelevant at this point.
        unsafe {
            if self.streaming {
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                let _ = self
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
            let _ = self.activate.ShutdownObject();
        }
        tracing::debug!(
            frames_in = self.frames_in,
            frames_out = self.frames_out,
            "Media Foundation encoder closed"
        );
    }
}

// ─── Annex-B inspection ─────────────────────────────────────────────────────────────────

/// Iterate over NAL unit payloads (without start codes) of an Annex-B stream.
fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let n = starts.len();
    (0..n).map(move |k| {
        let s = starts[k];
        let mut e = if k + 1 < n {
            starts[k + 1] - 3
        } else {
            data.len()
        };
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        &data[s..e]
    })
}

fn nal_type(nal: &[u8], codec: VideoCodec) -> Option<u8> {
    let first = *nal.first()?;
    Some(match codec {
        VideoCodec::H264 => first & 0x1f,
        VideoCodec::H265 => (first >> 1) & 0x3f,
    })
}

fn has_parameter_sets(data: &[u8], codec: VideoCodec) -> bool {
    let mut sps = false;
    let mut pps = false;
    let mut vps = codec == VideoCodec::H264;
    for t in nal_units(data).filter_map(|n| nal_type(n, codec)) {
        match (codec, t) {
            (VideoCodec::H264, 7) | (VideoCodec::H265, 33) => sps = true,
            (VideoCodec::H264, 8) | (VideoCodec::H265, 34) => pps = true,
            (VideoCodec::H265, 32) => vps = true,
            _ => {}
        }
    }
    sps && pps && vps
}

fn has_idr(data: &[u8], codec: VideoCodec) -> bool {
    nal_units(data)
        .filter_map(|n| nal_type(n, codec))
        .any(|t| match codec {
            VideoCodec::H264 => t == 5,
            VideoCodec::H265 => (16..=21).contains(&t),
        })
}

pub fn create(cfg: &EncoderConfig) -> Result<Box<dyn Encoder>> {
    if !supports(cfg.codec) {
        bail!("no hardware {:?} encoder available", cfg.codec);
    }
    Ok(Box::new(MfEncoder::new(cfg)?))
}
