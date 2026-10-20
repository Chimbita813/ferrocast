//! Minimal HTTPS GET over WinHTTP (built into Windows, so no TLS crate in the binary).

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_INVALID_DATA, E_INVALIDARG};
use windows::Win32::Networking::WinHttp::*;

use crate::Result;

/// Closes a WinHTTP handle on drop.
struct Handle(*mut core::ffi::c_void);

impl Handle {
    fn new(h: *mut core::ffi::c_void) -> Result<Self> {
        if h.is_null() {
            Err(windows::core::Error::from_thread())
        } else {
            Ok(Self(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// Fetches an `https://` URL, following redirects, and returns the body.
/// Fails on any status other than 200. `limit` caps the body size.
pub fn get(url: &str, user_agent: &str, accept: &str, limit: usize) -> Result<Vec<u8>> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| windows::core::Error::new(E_INVALIDARG, "only https URLs are supported"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    unsafe {
        let session = Handle::new(WinHttpOpen(
            &HSTRING::from(user_agent),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        // Resolve, connect, send, receive (ms).
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000)?;
        let connect = Handle::new(WinHttpConnect(
            session.0,
            &HSTRING::from(host),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ))?;
        let request = Handle::new(WinHttpOpenRequest(
            connect.0,
            &HSTRING::from("GET"),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        let headers: Vec<u16> = format!("Accept: {accept}\r\n").encode_utf16().collect();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;

        let mut status = 0u32;
        let mut len = 4u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut _),
            &mut len,
            std::ptr::null_mut(),
        )?;
        if status != 200 {
            return Err(windows::core::Error::new(
                ERROR_INVALID_DATA.to_hresult(),
                format!("HTTP {status} from {host}"),
            ));
        }

        let mut body = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut read,
            )?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() > limit {
                return Err(windows::core::Error::new(
                    ERROR_INVALID_DATA.to_hresult(),
                    "response too large",
                ));
            }
        }
        Ok(body)
    }
}
