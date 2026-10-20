//! The standard Windows "Open" file dialog.

use std::path::PathBuf;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};

/// Asks for one file. `filters` are (label, pattern) pairs such as
/// ("Web pages", "*.html;*.htm"). Returns None if the user cancels.
pub fn pick_file(title: &str, filters: &[(&str, &str)]) -> Option<PathBuf> {
    unsafe {
        // The UI thread is already an STA (winit sets up OLE); this is a no-op then.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let strings: Vec<(HSTRING, HSTRING)> = filters
            .iter()
            .map(|(name, spec)| (HSTRING::from(*name), HSTRING::from(*spec)))
            .collect();
        let specs: Vec<COMDLG_FILTERSPEC> = strings
            .iter()
            .map(|(name, spec)| COMDLG_FILTERSPEC {
                pszName: PCWSTR(name.as_ptr()),
                pszSpec: PCWSTR(spec.as_ptr()),
            })
            .collect();
        let _ = dialog.SetFileTypes(&specs);
        let _ = dialog.SetTitle(&HSTRING::from(title));
        // Fails with ERROR_CANCELLED when the user closes the dialog.
        dialog.Show(None).ok()?;
        let path = dialog
            .GetResult()
            .ok()?
            .GetDisplayName(SIGDN_FILESYSPATH)
            .ok()?;
        let result = path.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(path.0 as *const _));
        result
    }
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from("open"),
            &HSTRING::from(url),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
