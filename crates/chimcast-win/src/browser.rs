//! Browser sources: web pages (Streamlabs widgets, overlays) rendered by WebView2.
//!
//! WebView2 is the Edge runtime that ships with Windows, so nothing is bundled.
//! Each page is hosted in "visual" mode: it draws into a Windows.UI.Composition
//! visual instead of a window, with a transparent background, and that visual
//! is captured with Windows.Graphics.Capture like any window. Frames stay on the
//! GPU end to end.
//!
//! WebView2 needs an STA thread with a message loop, so all pages live on one
//! host thread that the sources talk to through a channel. The page's sound
//! plays on the default speakers and is picked up by Desktop Audio.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CoreWebView2EnvironmentOptions, CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Graphics::Capture::GraphicsCaptureItem;
use windows::System::DispatcherQueueController;
use windows::Win32::Foundation::{E_FAIL, E_POINTER, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DispatcherQueueOptions, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::UI::Composition::{Compositor, ContainerVisual};

use crate::capture::{FrameSlot, ScreenCapture};
use crate::d3d::Device;
use crate::Result;

/// Keeps pages animating and timers running even though nothing shows them on screen.
const BROWSER_ARGS: &str = "--disable-features=CalculateNativeWinOcclusion \
    --disable-background-timer-throttling --disable-renderer-backgrounding \
    --disable-backgrounding-occluded-windows --autoplay-policy=no-user-gesture-required";

enum Command {
    Create {
        id: u64,
        url: String,
        width: u32,
        height: u32,
        reply: Sender<std::result::Result<GraphicsCaptureItem, String>>,
    },
    Reload(u64),
    Remove(u64),
}

struct Host {
    commands: Mutex<Sender<Command>>,
    thread_id: u32,
}

fn host() -> std::result::Result<&'static Host, String> {
    static HOST: OnceLock<std::result::Result<Host, String>> = OnceLock::new();
    HOST.get_or_init(|| {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        std::thread::Builder::new()
            .name("browser".into())
            .spawn(move || host_thread(rx, ready_tx))
            .map_err(|e| e.to_string())?;
        let thread_id = ready_rx
            .recv()
            .map_err(|_| "the browser thread exited".to_string())??;
        Ok(Host {
            commands: Mutex::new(tx),
            thread_id,
        })
    })
    .as_ref()
    .map_err(|e| e.clone())
}

impl Host {
    fn send(&self, cmd: Command) {
        if self.commands.lock().unwrap().send(cmd).is_ok() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_APP, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Turns what the user typed into a URL: paths to local files become `file:///`.
pub fn normalize_url(input: &str) -> String {
    let s = input.trim().trim_matches('"');
    let looks_like_path = s.len() > 2 && s.as_bytes()[1] == b':' || s.starts_with("\\\\");
    if looks_like_path {
        format!("file:///{}", s.replace('\\', "/"))
    } else if s.contains("://") || s.starts_with("about:") {
        s.to_string()
    } else {
        format!("https://{s}")
    }
}

enum State {
    Loading(Receiver<std::result::Result<GraphicsCaptureItem, String>>),
    Live(ScreenCapture),
    Failed(String),
}

pub struct BrowserSource {
    id: u64,
    device: Device,
    state: Mutex<State>,
    empty: FrameSlot,
}

impl BrowserSource {
    /// Starts loading `url`; frames appear once the page has been created.
    pub fn open(device: &Device, url: &str, width: u32, height: u32) -> Result<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let host = host().map_err(|e| windows::core::Error::new(E_FAIL, e))?;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = channel();
        host.send(Command::Create {
            id,
            url: normalize_url(url),
            width: width.clamp(16, 7680),
            height: height.clamp(16, 4320),
            reply,
        });
        Ok(Self {
            id,
            device: device.clone(),
            state: Mutex::new(State::Loading(rx)),
            empty: Arc::new(Mutex::new(None)),
        })
    }

    pub fn frames(&self) -> FrameSlot {
        let mut state = self.state.lock().unwrap();
        if let State::Loading(rx) = &*state {
            let next = match rx.try_recv() {
                Ok(Ok(item)) => match ScreenCapture::from_item(&self.device, item) {
                    Ok(c) => State::Live(c),
                    Err(e) => State::Failed(e.message()),
                },
                Ok(Err(e)) => State::Failed(e),
                Err(TryRecvError::Empty) => return self.empty.clone(),
                Err(TryRecvError::Disconnected) => State::Failed("the browser stopped".into()),
            };
            if let State::Failed(e) = &next {
                log::warn!("browser source failed: {e}");
            }
            *state = next;
        }
        match &*state {
            State::Live(c) => c.frames(),
            _ => self.empty.clone(),
        }
    }

    pub fn error(&self) -> Option<String> {
        match &*self.state.lock().unwrap() {
            State::Failed(e) => Some(e.clone()),
            _ => None,
        }
    }

    /// Reloads the page (picks up widget changes made in the Streamlabs dashboard).
    pub fn reload(&self) {
        if let Ok(h) = host() {
            h.send(Command::Reload(self.id));
        }
    }
}

impl Drop for BrowserSource {
    fn drop(&mut self) {
        if let Ok(h) = host() {
            h.send(Command::Remove(self.id));
        }
    }
}

struct Page {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    _visual: ContainerVisual,
}

struct Browser {
    hwnd: HWND,
    compositor: Compositor,
    env: Option<ICoreWebView2Environment3>,
    pages: HashMap<u64, Page>,
}

fn host_thread(rx: Receiver<Command>, ready: Sender<std::result::Result<u32, String>>) {
    let setup = || -> Result<(DispatcherQueueController, Browser)> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        // Windows.UI.Composition needs a dispatcher queue on this thread.
        let queue = unsafe {
            CreateDispatcherQueueController(DispatcherQueueOptions {
                dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                threadType: DQTYPE_THREAD_CURRENT,
                apartmentType: DQTAT_COM_STA,
            })?
        };
        let compositor = Compositor::new()?;
        let hwnd = host_window()?;
        Ok((
            queue,
            Browser {
                hwnd,
                compositor,
                env: None,
                pages: HashMap::new(),
            },
        ))
    };
    let (_queue, mut browser) = match setup() {
        Ok(b) => b,
        Err(e) => {
            let _ = ready.send(Err(format!("browser sources unavailable: {}", e.message())));
            return;
        }
    };
    // Make sure the thread has a message queue before anyone posts to it.
    let mut msg = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    }
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    loop {
        // WebView2's own waits pump messages and may swallow our wake-ups, so
        // always drain the whole queue.
        loop {
            match rx.try_recv() {
                Ok(cmd) => browser.handle(cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        unsafe {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                return;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl Browser {
    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Create {
                id,
                url,
                width,
                height,
                reply,
            } => {
                let result = self.create(id, &url, width, height).map_err(|e| {
                    log::warn!("couldn't open {url}: {}", e.message());
                    e.message()
                });
                let _ = reply.send(result);
            }
            Command::Reload(id) => {
                if let Some(p) = self.pages.get(&id) {
                    unsafe {
                        let _ = p.webview.Reload();
                    }
                }
            }
            Command::Remove(id) => {
                if let Some(p) = self.pages.remove(&id) {
                    unsafe {
                        let _ = p.controller.Close();
                    }
                }
            }
        }
    }

    fn environment(&mut self) -> Result<ICoreWebView2Environment3> {
        if let Some(env) = &self.env {
            return Ok(env.clone());
        }
        let data_dir = data_dir();
        let options = CoreWebView2EnvironmentOptions::default();
        unsafe { options.set_additional_browser_arguments(BROWSER_ARGS.to_string()) };
        let options: ICoreWebView2EnvironmentOptions = options.into();
        let (tx, rx) = channel();
        CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    &HSTRING::from(data_dir.as_os_str()),
                    &options,
                    &handler,
                )
                .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |result, env| {
                result?;
                let _ = tx.send(env);
                Ok(())
            }),
        )
        .map_err(wv_error)?;
        let env: ICoreWebView2Environment3 = rx
            .recv()
            .ok()
            .flatten()
            .ok_or_else(|| {
                windows::core::Error::new(
                    E_POINTER,
                    "WebView2 isn't installed (get the Evergreen runtime from Microsoft)",
                )
            })?
            .cast()?;
        self.env = Some(env.clone());
        Ok(env)
    }

    fn create(
        &mut self,
        id: u64,
        url: &str,
        width: u32,
        height: u32,
    ) -> Result<GraphicsCaptureItem> {
        let env = self.environment()?;
        let hwnd = self.hwnd;
        let (tx, rx) = channel();
        CreateCoreWebView2CompositionControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                env.CreateCoreWebView2CompositionController(hwnd, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |result, controller| {
                result?;
                let _ = tx.send(controller);
                Ok(())
            }),
        )
        .map_err(wv_error)?;
        let composition =
            rx.recv().ok().flatten().ok_or_else(|| {
                windows::core::Error::new(E_POINTER, "WebView2 controller missing")
            })?;

        let visual = self.compositor.CreateContainerVisual()?;
        visual.SetSize(windows_numerics::Vector2 {
            X: width as f32,
            Y: height as f32,
        })?;
        let controller: ICoreWebView2Controller = composition.cast()?;
        unsafe {
            composition.SetRootVisualTarget(&visual.cast::<windows::core::IUnknown>()?)?;
            controller.SetBounds(RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            })?;
            controller.SetIsVisible(true)?;
            // Transparent page background, so alerts float over the scene.
            controller
                .cast::<ICoreWebView2Controller2>()?
                .SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                    A: 0,
                    R: 0,
                    G: 0,
                    B: 0,
                })?;
        }
        let webview = unsafe { controller.CoreWebView2()? };
        unsafe { webview.Navigate(&HSTRING::from(url))? };
        let item = GraphicsCaptureItem::CreateFromVisual(
            &visual.cast::<windows::UI::Composition::Visual>()?,
        )?;
        self.pages.insert(
            id,
            Page {
                controller,
                webview,
                _visual: visual,
            },
        );
        Ok(item)
    }
}

fn wv_error(e: webview2_com::Error) -> windows::core::Error {
    match e {
        webview2_com::Error::WindowsError(e) => e,
        other => windows::core::Error::new(E_FAIL, format!("{other:?}")),
    }
}

/// `%APPDATA%\Chimcast\WebView2`: cookies and cache for widget pages.
fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Chimcast")
        .join("WebView2")
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// WebView2 wants a parent window for focus and DPI even when it draws into a
/// visual. This one is 1x1, fully transparent, click-through and never focused.
fn host_window() -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = windows::core::w!("ChimcastBrowserHost");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            windows::core::w!("Chimcast browser"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 0, LWA_ALPHA)?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        Ok(hwnd)
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn urls() {
        assert_eq!(
            normalize_url("https://streamlabs.com/alert-box/v3/abc"),
            "https://streamlabs.com/alert-box/v3/abc"
        );
        assert_eq!(
            normalize_url("C:\\overlays\\a b.html"),
            "file:///C:/overlays/a b.html"
        );
        assert_eq!(normalize_url("example.com/w"), "https://example.com/w");
    }
}
