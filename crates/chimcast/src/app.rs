//! Window, event loop and the frame cycle: input -> UI -> engine sync -> paint.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chimcast_core::Project;
use chimcast_win::d3d::Device;
use chimcast_win::update::{UpdateState, Updater};
use chimcast_win::Engine;
use windows::core::HSTRING;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

use crate::logger::data_dir;
use crate::painter::Painter;
use crate::ui::{self, UiState};

/// The preview and UI refresh at 30 fps; capture and encoding run at the canvas rate regardless.
const UI_FRAME: Duration = Duration::from_millis(33);
/// Project changes are written at most this often.
const SAVE_DELAY: Duration = Duration::from_secs(2);
/// Set by CI to the workflow run number; local builds are 0 and never self-update.
const BUILD: Option<&str> = option_env!("CHIMCAST_BUILD");
/// The GitHub repository releases come from, also set by CI (follows a repo rename).
const UPDATE_REPO: &str = match option_env!("CHIMCAST_UPDATE_REPO") {
    Some(r) => r,
    None => "Chimbita813/ferrocast",
};

pub fn fatal(msg: &str) {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(msg),
            &HSTRING::from("Chimcast"),
            MB_OK | MB_ICONERROR,
        );
    }
}

struct Running {
    window: Arc<Window>,
    egui: egui_winit::State,
    painter: Painter,
    engine: Engine,
    device: Device,
    ui: UiState,
}

struct App {
    project: Project,
    path: PathBuf,
    running: Option<Running>,
    next_frame: Instant,
    dirty_since: Option<Instant>,
    error: Option<String>,
    updater: Updater,
    /// Recorded or streamed this session: then an update waits for the next launch.
    went_live: bool,
    restart: bool,
}

fn default_recording_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into())
        .join("Videos")
}

impl App {
    fn new() -> Self {
        let path = data_dir().join("project.json");
        let project = match Project::load(&path) {
            Ok(p) => p,
            Err(e) => {
                if path.exists() {
                    log::warn!("couldn't read {}: {e}; starting fresh", path.display());
                }
                let mut p = Project::default();
                p.settings.recording.directory = default_recording_dir();
                p
            }
        };
        let build = BUILD.and_then(|b| b.parse().ok()).unwrap_or(0);
        let updater = Updater::start(UPDATE_REPO, build, project.settings.auto_update);
        Self {
            project,
            path,
            running: None,
            next_frame: Instant::now(),
            dirty_since: None,
            error: None,
            updater,
            went_live: false,
            restart: false,
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.project.save(&self.path) {
            log::error!("saving {}: {e}", self.path.display());
        }
        self.dirty_since = None;
    }

    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<Running, String> {
        let attrs = Window::default_attributes()
            .with_title(match BUILD {
                Some(b) => format!("Chimcast (build {b})"),
                None => "Chimcast (dev build)".into(),
            })
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 820.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(800.0, 560.0))
            .with_window_icon(
                winit::window::Icon::from_rgba(
                    include_bytes!("../../../assets/icon-64.rgba").to_vec(),
                    64,
                    64,
                )
                .ok(),
            );
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        let hwnd = match window.window_handle().map_err(|e| e.to_string())?.as_raw() {
            RawWindowHandle::Win32(h) => HWND(h.hwnd.get() as *mut _),
            _ => return Err("unexpected window handle".into()),
        };
        let device =
            Device::new().map_err(|e| format!("Direct3D 11 is required: {}", e.message()))?;
        let engine = Engine::new(&self.project, device.clone()).map_err(|e| e.message())?;
        let size = window.inner_size();
        let painter =
            Painter::new(&device, hwnd, size.width, size.height).map_err(|e| e.message())?;
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        let egui = egui_winit::State::new(
            ctx,
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(8192),
        );
        Ok(Running {
            window,
            egui,
            painter,
            engine,
            device,
            ui: UiState::default(),
        })
    }

    fn frame(&mut self) {
        let Some(r) = self.running.as_mut() else {
            return;
        };
        self.went_live |= r.engine.is_live();
        r.ui.update = update_note(&self.updater.state(), self.went_live);
        let raw = r.egui.take_egui_input(&r.window);
        let ctx = r.egui.egui_ctx().clone();
        let mut changed = false;
        let output = ctx.run_ui(raw, |ui| {
            changed |= ui::draw(ui, &mut r.ui, &mut self.project, &mut r.engine);
        });
        r.egui
            .handle_platform_output(&r.window, output.platform_output);

        // Canvas size or frame rate changed: rebuild the engine (only possible while idle).
        if r.engine.needs_rebuild(&self.project) && !r.engine.is_live() {
            match Engine::new(&self.project, r.device.clone()) {
                Ok(e) => r.engine = e,
                Err(e) => log::error!("engine restart failed: {e}"),
            }
        }
        // Sources are re-synced every frame so closed windows and unplugged cameras recover.
        r.engine.sync(&self.project);
        if changed && self.dirty_since.is_none() {
            self.dirty_since = Some(Instant::now());
        }

        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let preview = r.engine.preview();
        if let Err(e) = r.painter.paint(
            &primitives,
            &output.textures_delta,
            output.pixels_per_point,
            Some(&preview),
        ) {
            log::error!("paint failed: {e}");
        }
        if self.dirty_since.is_some_and(|t| t.elapsed() >= SAVE_DELAY) {
            self.save();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.running.is_some() {
            return;
        }
        match self.start(event_loop) {
            Ok(r) => self.running = Some(r),
            Err(e) => {
                self.error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(r) = self.running.as_mut() else {
            return;
        };
        let response = r.egui.on_window_event(&r.window, &event);
        match event {
            WindowEvent::CloseRequested => {
                self.save();
                // Dropping the engine finalizes any recording in progress.
                self.running = None;
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Err(e) = r.painter.resize(size.width, size.height) {
                    log::error!("resize failed: {e}");
                }
                r.window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.frame(),
            _ => {
                if response.repaint {
                    r.window.request_redraw();
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(r) = self.running.as_ref() else {
            return;
        };
        // A new build was installed before anything went live: switch to it now.
        if matches!(self.updater.state(), UpdateState::Ready { .. })
            && !self.went_live
            && !r.engine.is_live()
        {
            log::info!("restarting into the new build");
            self.save();
            self.running = None;
            self.restart = true;
            event_loop.exit();
            return;
        }
        let now = Instant::now();
        if now >= self.next_frame {
            self.next_frame = now + UI_FRAME;
            // Keep refreshing while visible: the preview is live video.
            if !r.window.is_minimized().unwrap_or(false) {
                r.window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }
}

pub fn run() -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    let mut app = App::new();
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    if app.restart {
        if let Err(e) = app.updater.relaunch() {
            log::error!("couldn't start the new build: {e}");
            return Err(format!(
                "Chimcast was updated, but couldn't restart itself ({e}). Open it again."
            ));
        }
    }
    match app.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Status-bar text for the updater, if there's anything worth showing.
fn update_note(state: &UpdateState, went_live: bool) -> Option<String> {
    match state {
        UpdateState::Off | UpdateState::UpToDate => None,
        UpdateState::Checking => Some("Checking for updates".into()),
        UpdateState::Downloading { build } => Some(format!("Downloading update (build {build})")),
        UpdateState::Ready { build } if went_live => Some(format!(
            "Build {build} installed; it starts next time you open Chimcast"
        )),
        UpdateState::Ready { build } => Some(format!("Restarting into build {build}")),
        UpdateState::Failed(e) => Some(format!("Update: {e}")),
    }
}
