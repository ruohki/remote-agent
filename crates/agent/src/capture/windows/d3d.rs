//! Direct3D 11 device management shared by DXGI desktop duplication and the Media
//! Foundation encoder. Both must use the *same* device for zero-copy texture hand-off.

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_FLAG,
    D3D11_CPU_ACCESS_FLAG, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_RESOURCE_MISC_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
    DXGI_ERROR_NOT_FOUND,
};
use windows::Win32::Graphics::Gdi::HMONITOR;

/// A D3D11 device + immediate context. The immediate context is not thread safe; the
/// capture and encode pipeline uses it from one thread only, and multithread protection
/// is enabled as a belt-and-braces measure.
pub struct D3dDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// Adapter LUID as `(low, high)`; used as cache key.
    pub adapter_luid: (u32, i32),
}

// SAFETY: COM interface pointers can be moved between threads; D3D11 devices are
// free-threaded and the context is protected via ID3D10Multithread.
unsafe impl Send for D3dDevice {}
unsafe impl Sync for D3dDevice {}

impl D3dDevice {
    /// `true` while the device has not been removed/reset by the driver.
    pub fn is_alive(&self) -> bool {
        // SAFETY: plain COM call on a live interface.
        unsafe { self.device.GetDeviceRemovedReason().is_ok() }
    }

    /// Create a 2D texture with the given parameters.
    #[allow(clippy::too_many_arguments)]
    pub fn create_texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        usage: D3D11_USAGE,
        bind: D3D11_BIND_FLAG,
        cpu_access: D3D11_CPU_ACCESS_FLAG,
        misc: D3D11_RESOURCE_MISC_FLAG,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: usage,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: cpu_access.0 as u32,
            MiscFlags: misc.0 as u32,
        };
        let mut tex = None;
        // SAFETY: desc is fully initialised; tex receives the created texture.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .with_context(|| format!("CreateTexture2D {width}x{height} {format:?}"))?;
        tex.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))
    }

    /// Copy a BGRA texture into CPU memory as tightly packed rows (`stride = width * 4`).
    pub fn read_bgra(&self, tex: &ID3D11Texture2D) -> Result<(Vec<u8>, usize)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc only writes into desc.
        unsafe { tex.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(anyhow!("texture format {:?} is not BGRA", desc.Format));
        }
        let (width, height) = (desc.Width as usize, desc.Height as usize);
        let staging = self.create_texture(
            desc.Width,
            desc.Height,
            desc.Format,
            D3D11_USAGE_STAGING,
            D3D11_BIND_FLAG(0),
            D3D11_CPU_ACCESS_READ,
            D3D11_RESOURCE_MISC_FLAG(0),
        )?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging and tex have identical descriptions; Map/Unmap are paired and the
        // mapped pointer is only read in between.
        unsafe {
            self.context.CopyResource(&staging, tex);
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("mapping staging texture")?;
            let src_stride = mapped.RowPitch as usize;
            let dst_stride = width * 4;
            let mut out = vec![0u8; dst_stride * height];
            let base = mapped.pData as *const u8;
            for row in 0..height {
                let src = std::slice::from_raw_parts(base.add(row * src_stride), dst_stride);
                out[row * dst_stride..(row + 1) * dst_stride].copy_from_slice(src);
            }
            self.context.Unmap(&staging, 0);
            Ok((out, dst_stride))
        }
    }
}

fn create_device(adapter: Option<&IDXGIAdapter1>) -> Result<Arc<D3dDevice>> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    let mut context = None;
    let mut level = D3D_FEATURE_LEVEL::default();
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let base_adapter: Option<IDXGIAdapter> = match adapter {
        Some(a) => Some(a.cast::<IDXGIAdapter>().context("adapter cast")?),
        None => None,
    };
    let driver_type = if base_adapter.is_some() {
        D3D_DRIVER_TYPE_UNKNOWN
    } else {
        D3D_DRIVER_TYPE_HARDWARE
    };
    // SAFETY: all out-pointers are valid for the duration of the call.
    unsafe {
        D3D11CreateDevice(
            base_adapter.as_ref(),
            driver_type,
            HMODULE::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;
    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
    let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;
    if level.0 < D3D_FEATURE_LEVEL_11_0.0 {
        return Err(anyhow!(
            "Direct3D feature level 11.0 required (got {:#x})",
            level.0
        ));
    }
    // Serialise immediate-context access in case another thread ever touches it.
    if let Ok(mt) = context.cast::<ID3D10Multithread>() {
        // SAFETY: plain COM call.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    let adapter_luid = match adapter {
        Some(a) => luid_of(a)?,
        None => {
            // Resolve the adapter the default device ended up on.
            let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice =
                device.cast().context("IDXGIDevice cast")?;
            // SAFETY: plain COM calls.
            let a = unsafe { dxgi.GetAdapter() }.context("GetAdapter")?;
            let a1: IDXGIAdapter1 = a.cast().context("IDXGIAdapter1 cast")?;
            luid_of(&a1)?
        }
    };
    tracing::debug!(
        ?adapter_luid,
        feature_level = format_args!("{:#x}", level.0),
        "created D3D11 device"
    );
    Ok(Arc::new(D3dDevice {
        device,
        context,
        adapter_luid,
    }))
}

fn luid_of(adapter: &IDXGIAdapter1) -> Result<(u32, i32)> {
    // SAFETY: plain COM call.
    let desc = unsafe { adapter.GetDesc1() }.context("IDXGIAdapter1::GetDesc1")?;
    Ok((desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart))
}

/// Devices cached per adapter LUID (`None` = default adapter).
type DeviceCache = Mutex<HashMap<Option<(u32, i32)>, Weak<D3dDevice>>>;

fn cache() -> &'static DeviceCache {
    static CACHE: std::sync::OnceLock<DeviceCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_or_create(
    key: Option<(u32, i32)>,
    adapter: Option<&IDXGIAdapter1>,
) -> Result<Arc<D3dDevice>> {
    let mut cache = cache().lock();
    if let Some(existing) = cache.get(&key).and_then(Weak::upgrade) {
        if existing.is_alive() {
            return Ok(existing);
        }
        tracing::warn!("D3D11 device was removed; re-creating");
    }
    let dev = create_device(adapter)?;
    cache.insert(key, Arc::downgrade(&dev));
    Ok(dev)
}

/// The device on the default adapter (used when frames arrive as CPU BGRA).
pub fn shared() -> Result<Arc<D3dDevice>> {
    cached_or_create(None, None)
}

/// The device on the adapter that drives `monitor`, plus the matching DXGI output.
/// Desktop duplication only works with a device created on the output's own adapter.
pub fn for_monitor(monitor: HMONITOR) -> Result<(Arc<D3dDevice>, IDXGIOutput)> {
    // SAFETY: plain COM factory creation.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut adapter_index = 0;
    loop {
        // SAFETY: plain COM enumeration; NOT_FOUND terminates it.
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e).context("EnumAdapters1"),
        };
        adapter_index += 1;
        let mut output_index = 0;
        loop {
            // SAFETY: plain COM enumeration.
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e).context("EnumOutputs"),
            };
            output_index += 1;
            // SAFETY: plain COM call.
            let desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc")?;
            if desc.Monitor == monitor {
                let luid = luid_of(&adapter)?;
                let dev = cached_or_create(Some(luid), Some(&adapter))?;
                return Ok((dev, output));
            }
        }
    }
    Err(anyhow!("no DXGI output found for monitor {:?}", monitor.0))
}
