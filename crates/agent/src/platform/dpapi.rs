//! Windows DPAPI wrappers for the device secret (`CryptProtectData` / `CryptUnprotectData`).
use anyhow::{Context, Result};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn blob_of(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    }
}

fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: pbData/cbData were filled by the API; the buffer is freed right after copying.
    unsafe {
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut core::ffi::c_void)));
        v
    }
}

pub fn protect(secret: &[u8]) -> Result<Vec<u8>> {
    let input = blob_of(secret);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: valid input blob, output filled by the API.
    unsafe {
        CryptProtectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .context("CryptProtectData")?;
    }
    Ok(take(out))
}

pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>> {
    let input = blob_of(blob);
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as above.
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
        .context("CryptUnprotectData")?;
    }
    Ok(take(out))
}
