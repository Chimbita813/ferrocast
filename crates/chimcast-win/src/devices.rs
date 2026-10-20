//! Enumeration of things the user can capture: monitors, windows, webcams, audio endpoints.

use windows::core::{BOOL, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::Media::Audio::{
    eCapture, eRender, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantToBSTR;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::mf;
use crate::Result;

#[derive(Clone, Debug)]
pub struct Monitor {
    pub handle: HMONITOR,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Monitors in a stable order: primary first, then left to right.
pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<(HMONITOR, RECT, bool, String)>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(h, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
            let name = String::from_utf16_lossy(&info.szDevice)
                .trim_end_matches('\0')
                .to_string();
            list.push((
                h,
                info.monitorInfo.rcMonitor,
                info.monitorInfo.dwFlags & 1 != 0,
                name,
            ));
        }
        true.into()
    }
    let mut raw: Vec<(HMONITOR, RECT, bool, String)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut raw as *mut _ as isize));
    }
    raw.sort_by_key(|(_, r, primary, _)| (!primary, r.left, r.top));
    raw.into_iter()
        .enumerate()
        .map(|(i, (handle, r, primary, _device))| Monitor {
            handle,
            name: format!(
                "Display {} ({}x{}){}",
                i + 1,
                r.right - r.left,
                r.bottom - r.top,
                if primary { " - primary" } else { "" }
            ),
            width: (r.right - r.left) as u32,
            height: (r.bottom - r.top) as u32,
            primary,
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct Window {
    pub handle: HWND,
    pub title: String,
    pub exe: Option<String>,
}

fn window_title(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn window_exe(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().map(str::to_string)
    }
}

/// Capturable top-level windows: visible, titled, not tool windows, not cloaked.
pub fn windows() -> Vec<Window> {
    unsafe extern "system" fn cb(hwnd: HWND, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
        list.push(hwnd);
        true.into()
    }
    let mut handles: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut handles as *mut _ as isize));
    }
    let own_pid = std::process::id();
    handles
        .into_iter()
        .filter(|&h| unsafe {
            if !IsWindowVisible(h).as_bool() {
                return false;
            }
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                return false;
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok()
                && cloaked != 0
            {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            pid != own_pid
        })
        .filter_map(|h| {
            let title = window_title(h);
            (!title.is_empty()).then(|| Window {
                handle: h,
                title,
                exe: window_exe(h),
            })
        })
        .collect()
}

/// Finds a window by exact title, falling back to the same executable.
pub fn find_window(title: &str, exe: Option<&str>) -> Option<HWND> {
    let list = windows();
    list.iter()
        .find(|w| w.title == title)
        .or_else(|| {
            exe.and_then(|e| {
                list.iter()
                    .find(|w| w.exe.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(e)))
            })
        })
        .map(|w| w.handle)
}

#[derive(Clone, Debug)]
pub struct Webcam {
    pub name: String,
    pub id: String,
}

pub fn webcams() -> Result<Vec<Webcam>> {
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.unwrap();
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFEnumDeviceSources(&attrs, &mut list, &mut count)?;
        let mut out = Vec::new();
        if !list.is_null() {
            for a in std::slice::from_raw_parts_mut(list, count as usize) {
                if let Some(a) = a.take() {
                    let name =
                        mf::get_string(&a.clone().into(), &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                            .unwrap_or_default();
                    if let Some(id) = mf::get_string(
                        &a.into(),
                        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                    ) {
                        out.push(Webcam { name, id });
                    }
                }
            }
            CoTaskMemFree(Some(list as *const _));
        }
        Ok(out)
    }
}

#[derive(Clone, Debug)]
pub struct AudioDevice {
    pub name: String,
    pub id: String,
}

fn audio_endpoints(flow: EDataFlow) -> Result<Vec<AudioDevice>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..collection.GetCount()? {
            let device = collection.Item(i)?;
            let id_ptr = device.GetId()?;
            let id = id_ptr.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id_ptr.0 as *const _));
            let name = device
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
                .and_then(|v| PropVariantToBSTR(&v))
                .map(|b| b.to_string())
                .unwrap_or_else(|_| id.clone());
            out.push(AudioDevice { name, id });
        }
        Ok(out)
    }
}

pub fn microphones() -> Result<Vec<AudioDevice>> {
    audio_endpoints(eCapture)
}

pub fn speakers() -> Result<Vec<AudioDevice>> {
    audio_endpoints(eRender)
}
