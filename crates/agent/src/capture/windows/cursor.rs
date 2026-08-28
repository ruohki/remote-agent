//! Software compositing of the DXGI desktop-duplication pointer shape.
//!
//! Desktop duplication delivers the desktop *without* the mouse cursor; the shape and
//! position come as metadata. We blend the cursor into a small region of the GPU frame:
//! copy the affected rectangle into a staging texture, blend on the CPU, copy it back.
//! The frame itself never leaves the GPU.

use anyhow::{anyhow, Context, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_FLAG, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ_WRITE, D3D11_RESOURCE_MISC_FLAG, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

use super::d3d::D3dDevice;

/// Cached pointer shape plus the scratch texture used for blending.
#[derive(Default)]
pub struct CursorCompositor {
    shape: Vec<u8>,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    has_shape: bool,
    scratch: Option<(ID3D11Texture2D, u32, u32)>,
}

impl CursorCompositor {
    /// Buffer that `IDXGIOutputDuplication::GetFramePointerShape` writes into.
    pub fn shape_buffer(&mut self, required: u32) -> &mut [u8] {
        if self.shape.len() < required as usize {
            self.shape.resize(required as usize, 0);
        }
        &mut self.shape
    }

    pub fn set_shape_info(&mut self, info: DXGI_OUTDUPL_POINTER_SHAPE_INFO) {
        self.info = info;
        self.has_shape = info.Width > 0 && info.Height > 0;
    }

    pub fn has_shape(&self) -> bool {
        self.has_shape
    }

    /// Visible height of the shape (monochrome shapes pack AND + XOR masks vertically).
    fn shape_height(&self) -> u32 {
        if self.info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
            self.info.Height / 2
        } else {
            self.info.Height
        }
    }

    /// Blend the cached shape into `target` (a BGRA `D3D11_USAGE_DEFAULT` texture of
    /// `frame_w`×`frame_h`) with the shape's top-left at (`x`, `y`).
    pub fn composite(
        &mut self,
        dev: &D3dDevice,
        target: &ID3D11Texture2D,
        frame_w: u32,
        frame_h: u32,
        x: i32,
        y: i32,
    ) -> Result<()> {
        if !self.has_shape {
            return Ok(());
        }
        let shape_w = self.info.Width as i32;
        let shape_h = self.shape_height() as i32;
        // Clip the shape rectangle to the frame.
        let left = x.max(0);
        let top = y.max(0);
        let right = (x + shape_w).min(frame_w as i32);
        let bottom = (y + shape_h).min(frame_h as i32);
        if right <= left || bottom <= top {
            return Ok(());
        }
        let (rw, rh) = ((right - left) as u32, (bottom - top) as u32);
        let (sx0, sy0) = ((left - x) as usize, (top - y) as usize);

        let scratch = self.scratch_texture(dev, rw, rh)?;
        let src_box = D3D11_BOX {
            left: left as u32,
            top: top as u32,
            front: 0,
            right: right as u32,
            bottom: bottom as u32,
            back: 1,
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the box lies inside both textures; Map/Unmap are paired and the mapped
        // memory is only touched in between.
        unsafe {
            dev.context
                .CopySubresourceRegion(&scratch, 0, 0, 0, 0, target, 0, Some(&src_box));
            dev.context
                .Map(&scratch, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped))
                .context("mapping cursor scratch texture")?;
            let pitch = mapped.RowPitch as usize;
            let pixels =
                std::slice::from_raw_parts_mut(mapped.pData as *mut u8, pitch * rh as usize);
            self.blend(pixels, pitch, rw as usize, rh as usize, sx0, sy0);
            dev.context.Unmap(&scratch, 0);
            let back_box = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: rw,
                bottom: rh,
                back: 1,
            };
            dev.context.CopySubresourceRegion(
                target,
                0,
                left as u32,
                top as u32,
                0,
                &scratch,
                0,
                Some(&back_box),
            );
        }
        Ok(())
    }

    fn scratch_texture(&mut self, dev: &D3dDevice, w: u32, h: u32) -> Result<ID3D11Texture2D> {
        if let Some((tex, tw, th)) = &self.scratch {
            if *tw >= w && *th >= h {
                return Ok(tex.clone());
            }
        }
        // Allocate generously so cursor moves at the frame edge do not re-allocate.
        let (aw, ah) = (w.max(self.info.Width), h.max(self.shape_height()));
        let tex = dev
            .create_texture(
                aw,
                ah,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_USAGE_STAGING,
                D3D11_BIND_FLAG(0),
                D3D11_CPU_ACCESS_READ | D3D11_CPU_ACCESS_WRITE,
                D3D11_RESOURCE_MISC_FLAG(0),
            )
            .context("creating cursor scratch texture")?;
        self.scratch = Some((tex.clone(), aw, ah));
        Ok(tex)
    }

    /// Blend the shape sub-rectangle starting at (`sx0`, `sy0`) into `pixels`.
    fn blend(&self, pixels: &mut [u8], pitch: usize, w: usize, h: usize, sx0: usize, sy0: usize) {
        let shape_pitch = self.info.Pitch as usize;
        let kind = self.info.Type;
        for row in 0..h {
            let dst = &mut pixels[row * pitch..row * pitch + w * 4];
            let sy = sy0 + row;
            if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
                let and_row = &self.shape[sy * shape_pitch..];
                let xor_row = &self.shape[(sy + self.shape_height() as usize) * shape_pitch..];
                for col in 0..w {
                    let sx = sx0 + col;
                    let bit = 0x80 >> (sx % 8);
                    let and = and_row[sx / 8] & bit != 0;
                    let xor = xor_row[sx / 8] & bit != 0;
                    let px = &mut dst[col * 4..col * 4 + 4];
                    for c in px.iter_mut().take(3) {
                        let v = if and { *c } else { 0 };
                        *c = if xor { !v } else { v };
                    }
                    px[3] = 0xff;
                }
            } else {
                let src_row = &self.shape[sy * shape_pitch..];
                let masked = kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
                let color = kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32;
                if !masked && !color {
                    return;
                }
                for col in 0..w {
                    let sx = sx0 + col;
                    let s = &src_row[sx * 4..sx * 4 + 4];
                    let px = &mut dst[col * 4..col * 4 + 4];
                    if masked {
                        // alpha 0xFF → XOR with the screen, otherwise replace.
                        if s[3] == 0xff {
                            px[0] ^= s[0];
                            px[1] ^= s[1];
                            px[2] ^= s[2];
                        } else {
                            px[0] = s[0];
                            px[1] = s[1];
                            px[2] = s[2];
                        }
                    } else {
                        let a = s[3] as u32;
                        if a == 0 {
                            continue;
                        }
                        for c in 0..3 {
                            px[c] =
                                ((s[c] as u32 * a + px[c] as u32 * (255 - a) + 127) / 255) as u8;
                        }
                    }
                    px[3] = 0xff;
                }
            }
        }
    }
}

/// Convenience for callers that want a typed error when the shape buffer is too small.
pub fn ensure_shape_fits(buf_len: usize, required: u32) -> Result<()> {
    if buf_len < required as usize {
        Err(anyhow!(
            "pointer shape buffer too small: {buf_len} < {required}"
        ))
    } else {
        Ok(())
    }
}
