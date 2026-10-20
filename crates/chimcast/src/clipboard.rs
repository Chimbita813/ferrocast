//! Text clipboard through user32, so copy and paste work in text fields
//! without pulling in a clipboard crate.

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

const CF_UNICODETEXT: u32 = 13;

/// Holds the clipboard open; closes it on drop.
struct Open;

impl Open {
    fn new() -> Option<Self> {
        unsafe { OpenClipboard(None).ok().map(|_| Open) }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

pub fn get() -> Option<String> {
    let _open = Open::new()?;
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let mem = HGLOBAL(handle.0);
        let ptr = GlobalLock(mem) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let max = GlobalSize(mem) / 2;
        let text = std::slice::from_raw_parts(ptr, max);
        let len = text.iter().position(|&c| c == 0).unwrap_or(max);
        let s = String::from_utf16_lossy(&text[..len]);
        let _ = GlobalUnlock(mem);
        Some(s)
    }
}

pub fn set(text: &str) {
    let Some(_open) = Open::new() else {
        return;
    };
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) else {
            return;
        };
        let ptr = GlobalLock(mem) as *mut u16;
        if ptr.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(mem);
        let _ = EmptyClipboard();
        // The clipboard owns the memory from here on.
        let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0)));
    }
}
