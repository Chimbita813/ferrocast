//! The main window: preview on top, then scenes, sources, audio mixer and controls.

use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Pos2, Rect as ERect, Sense, Stroke, StrokeKind, Ui, Vec2};
use ferrocast_core::{
    AudioSourceKind, Id, Project, Rect, VideoSourceKind, AUDIO_BITRATES_KBPS, STREAM_PRESETS,
};
use ferrocast_media::output::OutputState;
use ferrocast_win::{devices, Engine};

use crate::painter::PREVIEW;

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Move,
    Resize,
}

#[derive(Default)]
pub struct UiState {
    selected: Option<Id>,
    drag: Option<Drag>,
    show_settings: bool,
    show_key: bool,
    image_path: String,
    message: Option<(String, Instant)>,
    recording_started: Option<Instant>,
    streaming_started: Option<Instant>,
}

impl UiState {
    fn say(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        log::info!("{msg}");
        self.message = Some((msg, Instant::now()));
    }
}

/// Draws the whole UI. Returns true if the project changed.
pub fn draw(ui: &mut Ui, st: &mut UiState, project: &mut Project, engine: &mut Engine) -> bool {
    let before = project.clone();

    egui::Panel::bottom("status").show(ui, |ui| status_bar(ui, st, engine));
    egui::Panel::bottom("dock")
        .resizable(true)
        .default_size(250.0)
        .min_size(180.0)
        .show(ui, |ui| {
            ui.columns(4, |cols| {
                scenes(&mut cols[0], st, project);
                sources(&mut cols[1], st, project, engine);
                mixer(&mut cols[2], project, engine);
                controls(&mut cols[3], st, project, engine);
            });
        });
    egui::CentralPanel::default_margins().show(ui, |ui| preview(ui, st, project));

    if st.show_settings {
        let mut open = true;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .show(ui.ctx(), |ui| settings(ui, st, project, engine.is_live()));
        st.show_settings = open;
    }
    *project != before
}

fn heading(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong());
    ui.separator();
}

fn status_bar(ui: &mut Ui, st: &mut UiState, engine: &Engine) {
    let s = engine.stats();
    ui.horizontal(|ui| {
        ui.label(format!("{:.0} fps", s.fps));
        ui.separator();
        ui.label(format!("render {:.1} ms", s.render_ms));
        ui.separator();
        ui.label(format!("RAM {} MB", s.private_bytes / (1024 * 1024)));
        if let Some(enc) = &s.video_encoder {
            ui.separator();
            ui.label(format!("encoder: {enc}"));
        }
        if s.encoder_dropped_frames + s.skipped_frames > 0 {
            ui.separator();
            ui.label(format!(
                "dropped {} / skipped {}",
                s.encoder_dropped_frames, s.skipped_frames
            ));
        }
        if let Some(err) = &s.last_error {
            ui.separator();
            ui.colored_label(Color32::LIGHT_RED, err);
        }
        if let Some((msg, at)) = &st.message {
            if at.elapsed() < Duration::from_secs(8) {
                ui.separator();
                ui.label(msg);
            } else {
                st.message = None;
            }
        }
    });
}

fn scenes(ui: &mut Ui, st: &mut UiState, project: &mut Project) {
    heading(ui, "Scenes");
    egui::ScrollArea::vertical()
        .id_salt("scenes")
        .max_height(ui.available_height() - 60.0)
        .show(ui, |ui| {
            let active = project.active_scene;
            let mut clicked = None;
            for scene in &project.scenes {
                if ui
                    .selectable_label(scene.id == active, &scene.name)
                    .clicked()
                {
                    clicked = Some(scene.id);
                }
            }
            if let Some(id) = clicked {
                project.active_scene = id;
                st.selected = None;
            }
        });
    ui.text_edit_singleline(&mut project.active_mut().name)
        .on_hover_text("Rename scene");
    ui.horizontal(|ui| {
        if ui.button("+").on_hover_text("Add scene").clicked() {
            let n = project.scenes.len() + 1;
            project.active_scene = project.add_scene(&format!("Scene {n}"));
            st.selected = None;
        }
        if ui.button("−").on_hover_text("Remove scene").clicked() {
            let id = project.active_scene;
            project.remove_scene(id);
            st.selected = None;
        }
    });
}

fn add_source(project: &mut Project, st: &mut UiState, name: String, kind: VideoSourceKind) {
    let scene = project.active_scene;
    st.selected = project.add_item(scene, &name, kind);
}

fn sources(ui: &mut Ui, st: &mut UiState, project: &mut Project, engine: &Engine) {
    heading(ui, "Sources");
    let scene_id = project.active_scene;
    egui::ScrollArea::vertical()
        .id_salt("sources")
        .max_height(ui.available_height() - 60.0)
        .show(ui, |ui| {
            let scene = project.active_mut();
            // Top of the stack first, like OBS.
            for item in scene.items.iter_mut().rev() {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut item.visible, "").on_hover_text("Visible");
                    let label = format!("{} · {}", item.name, item.source.label());
                    if ui
                        .selectable_label(st.selected == Some(item.id), label)
                        .clicked()
                    {
                        st.selected = Some(item.id);
                    }
                    if let Some(err) = engine.source_error(item.id) {
                        ui.colored_label(Color32::LIGHT_RED, "⚠").on_hover_text(err);
                    }
                });
            }
        });

    ui.horizontal(|ui| {
        ui.menu_button("+", |ui| {
            ui.menu_button("Display", |ui| {
                for (i, m) in devices::monitors().iter().enumerate() {
                    if ui.button(&m.name).clicked() {
                        add_source(
                            project,
                            st,
                            format!("Display {}", i + 1),
                            VideoSourceKind::Monitor { index: i as u32 },
                        );
                        ui.close();
                    }
                }
            });
            ui.menu_button("Window", |ui| {
                egui::ScrollArea::vertical()
                    .max_height(400.0)
                    .show(ui, |ui| {
                        for w in devices::windows() {
                            let label = match &w.exe {
                                Some(exe) => format!("[{exe}] {}", w.title),
                                None => w.title.clone(),
                            };
                            if ui.button(label).clicked() {
                                let name = w.exe.clone().unwrap_or_else(|| w.title.clone());
                                add_source(
                                    project,
                                    st,
                                    name,
                                    VideoSourceKind::Window {
                                        title: w.title,
                                        exe: w.exe,
                                    },
                                );
                                ui.close();
                            }
                        }
                    });
            });
            ui.menu_button("Webcam", |ui| match devices::webcams() {
                Ok(cams) if !cams.is_empty() => {
                    for c in cams {
                        if ui.button(&c.name).clicked() {
                            add_source(
                                project,
                                st,
                                c.name.clone(),
                                VideoSourceKind::Webcam { device_id: c.id },
                            );
                            ui.close();
                        }
                    }
                }
                _ => {
                    ui.label("No cameras found");
                }
            });
            ui.menu_button("Image", |ui| {
                ui.label("Path to a PNG/JPEG/BMP file:");
                ui.text_edit_singleline(&mut st.image_path);
                if ui.button("Add").clicked() && !st.image_path.trim().is_empty() {
                    let path = std::path::PathBuf::from(st.image_path.trim().trim_matches('"'));
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Image".into());
                    add_source(project, st, name, VideoSourceKind::Image { path });
                    st.image_path.clear();
                    ui.close();
                }
            });
            if ui.button("Color").clicked() {
                add_source(
                    project,
                    st,
                    "Color".into(),
                    VideoSourceKind::Color {
                        rgba: [32, 32, 40, 255],
                    },
                );
                ui.close();
            }
        });
        let selected = st.selected;
        ui.add_enabled_ui(selected.is_some(), |ui| {
            let sel = selected.unwrap_or(Id(0));
            if ui.button("−").on_hover_text("Remove source").clicked() {
                project.remove_item(scene_id, sel);
                st.selected = None;
            }
            if ui.button("▲").on_hover_text("Move up").clicked() {
                project.active_mut().raise(sel);
            }
            if ui.button("▼").on_hover_text("Move down").clicked() {
                project.active_mut().lower(sel);
            }
            let (cw, ch) = project.canvas_size();
            if ui
                .button("Fit")
                .on_hover_text("Fit to canvas, keeping the source's aspect ratio")
                .clicked()
            {
                let aspect = engine
                    .source_size(sel)
                    .map(|(w, h)| w as f32 / h as f32)
                    .unwrap_or(cw / ch);
                if let Some(item) = project.active_mut().item_mut(sel) {
                    item.rect = Rect::new(0.0, 0.0, cw, ch).fit_aspect(aspect);
                }
            }
        });
    });
    if let Some(item) = st.selected.and_then(|id| project.active_mut().item_mut(id)) {
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut item.name);
        });
        ui.horizontal(|ui| {
            for (label, v) in [
                ("x", &mut item.rect.x),
                ("y", &mut item.rect.y),
                ("w", &mut item.rect.w),
                ("h", &mut item.rect.h),
            ] {
                ui.label(label);
                ui.add(egui::DragValue::new(v).speed(1.0));
            }
        });
        if let VideoSourceKind::Color { rgba } = &mut item.source {
            ui.color_edit_button_srgba_unmultiplied(rgba);
        }
    }
}

fn db_to_meter(db: f32) -> f32 {
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn mixer(ui: &mut Ui, project: &mut Project, engine: &Engine) {
    heading(ui, "Audio Mixer");
    let peaks = engine.stats().audio_peaks;
    let mut remove = None;
    egui::ScrollArea::vertical()
        .id_salt("mixer")
        .max_height(ui.available_height() - 30.0)
        .show(ui, |ui| {
            for a in &mut project.audio {
                ui.horizontal(|ui| {
                    ui.label(&a.name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").on_hover_text("Remove").clicked() {
                            remove = Some(a.id);
                        }
                        let label = if a.muted { "Muted" } else { "Mute" };
                        ui.toggle_value(&mut a.muted, label);
                    });
                });
                let peak = peaks.get(&a.id.0).copied().unwrap_or(f32::NEG_INFINITY);
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(rect, CornerRadius::same(2), Color32::from_gray(40));
                let level = if a.muted { 0.0 } else { db_to_meter(peak) };
                let color = if peak > -3.0 {
                    Color32::from_rgb(230, 70, 60)
                } else if peak > -12.0 {
                    Color32::from_rgb(230, 200, 60)
                } else {
                    Color32::from_rgb(80, 200, 100)
                };
                p.rect_filled(
                    ERect::from_min_size(rect.min, Vec2::new(rect.width() * level, rect.height())),
                    CornerRadius::same(2),
                    color,
                );
                let mut db = if a.volume > 0.0 {
                    20.0 * a.volume.log10()
                } else {
                    -60.0
                };
                if ui
                    .add(
                        egui::Slider::new(&mut db, -60.0..=6.0)
                            .suffix(" dB")
                            .show_value(true),
                    )
                    .changed()
                {
                    a.volume = if db <= -60.0 {
                        0.0
                    } else {
                        10f32.powf(db / 20.0)
                    };
                }
                ui.add_space(4.0);
            }
        });
    if let Some(id) = remove {
        project.remove_audio(id);
    }
    ui.menu_button("+", |ui| {
        let mut add = |ui: &mut Ui, name: String, kind: AudioSourceKind| {
            if ui.button(&name).clicked() {
                project.add_audio(&name, kind);
                ui.close();
            }
        };
        ui.label("Desktop audio");
        add(
            ui,
            "Desktop Audio".into(),
            AudioSourceKind::Desktop { device_id: None },
        );
        for d in devices::speakers().unwrap_or_default() {
            add(
                ui,
                d.name.clone(),
                AudioSourceKind::Desktop {
                    device_id: Some(d.id),
                },
            );
        }
        ui.separator();
        ui.label("Microphones");
        add(
            ui,
            "Default Mic".into(),
            AudioSourceKind::Microphone { device_id: None },
        );
        for d in devices::microphones().unwrap_or_default() {
            add(
                ui,
                d.name.clone(),
                AudioSourceKind::Microphone {
                    device_id: Some(d.id),
                },
            );
        }
    });
}

fn elapsed(since: Option<Instant>) -> String {
    let s = since.map_or(0, |t| t.elapsed().as_secs());
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn controls(ui: &mut Ui, st: &mut UiState, project: &mut Project, engine: &mut Engine) {
    heading(ui, "Controls");
    let wide = Vec2::new(ui.available_width(), 30.0);

    let streaming = engine.streaming_status();
    match &streaming {
        None => {
            if ui
                .add_sized(wide, egui::Button::new("Start Streaming"))
                .clicked()
            {
                match engine.start_streaming(project) {
                    Ok(()) => {
                        st.streaming_started = Some(Instant::now());
                        st.say("Connecting to the stream server…");
                    }
                    Err(e) => st.say(format!("Can't stream: {e}")),
                }
            }
        }
        Some(s) => {
            let label = match &s.state {
                OutputState::Starting => "Connecting… (click to cancel)".to_string(),
                OutputState::Running => {
                    format!("Stop Streaming  {}", elapsed(st.streaming_started))
                }
                OutputState::Failed(_) | OutputState::Stopped => {
                    "Stream ended (click to reset)".to_string()
                }
            };
            let button = egui::Button::new(label).fill(Color32::from_rgb(140, 30, 30));
            if ui.add_sized(wide, button).clicked() {
                engine.stop_streaming();
                st.streaming_started = None;
            }
            if let OutputState::Failed(e) = &s.state {
                ui.colored_label(Color32::LIGHT_RED, e);
            } else if s.dropped_packets > 0 {
                ui.label(format!(
                    "{} packets dropped (network too slow)",
                    s.dropped_packets
                ));
            }
        }
    }

    match engine.recording_status() {
        None => {
            if ui
                .add_sized(wide, egui::Button::new("Start Recording"))
                .clicked()
            {
                match engine.start_recording(project) {
                    Ok(path) => {
                        st.recording_started = Some(Instant::now());
                        st.say(format!("Recording to {}", path.display()));
                    }
                    Err(e) => st.say(format!("Can't record: {e}")),
                }
            }
        }
        Some((s, _path)) => {
            let label = format!(
                "Stop Recording  {}  ({} MB)",
                elapsed(st.recording_started),
                s.bytes / (1024 * 1024)
            );
            if ui
                .add_sized(
                    wide,
                    egui::Button::new(label).fill(Color32::from_rgb(140, 30, 30)),
                )
                .clicked()
            {
                engine.stop_recording();
                st.recording_started = None;
                st.say("Recording saved");
            }
            if let OutputState::Failed(e) = &s.state {
                ui.colored_label(Color32::LIGHT_RED, e);
            }
        }
    }
    if ui.add_sized(wide, egui::Button::new("Settings")).clicked() {
        st.show_settings = true;
    }
}

fn settings(ui: &mut Ui, st: &mut UiState, project: &mut Project, live: bool) {
    let s = &mut project.settings;
    egui::Grid::new("settings")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Video").strong());
            ui.end_row();
            ui.label("Resolution");
            ui.add_enabled_ui(!live, |ui| {
                egui::ComboBox::from_id_salt("res")
                    .selected_text(format!("{}x{}", s.video.width, s.video.height))
                    .show_ui(ui, |ui| {
                        for (w, h) in [(1920, 1080), (1600, 900), (1280, 720), (2560, 1440)] {
                            if ui
                                .selectable_label(
                                    (s.video.width, s.video.height) == (w, h),
                                    format!("{w}x{h}"),
                                )
                                .clicked()
                            {
                                s.video.width = w;
                                s.video.height = h;
                            }
                        }
                    });
            });
            ui.end_row();
            ui.label("Frame rate");
            ui.add_enabled_ui(!live, |ui| {
                ui.horizontal(|ui| {
                    ui.radio_value(&mut s.video.fps, 30, "30");
                    ui.radio_value(&mut s.video.fps, 60, "60");
                });
            });
            ui.end_row();

            ui.label(egui::RichText::new("Encoder").strong());
            ui.end_row();
            ui.label("Video bitrate");
            ui.add_enabled(
                !live,
                egui::Slider::new(&mut s.encoder.video_bitrate_kbps, 1000..=20000)
                    .suffix(" kbps")
                    .step_by(250.0),
            );
            ui.end_row();
            ui.label("Keyframe interval");
            ui.add_enabled(
                !live,
                egui::Slider::new(&mut s.encoder.keyframe_interval_secs, 1..=10).suffix(" s"),
            );
            ui.end_row();
            ui.label("Audio bitrate");
            ui.add_enabled_ui(!live, |ui| {
                egui::ComboBox::from_id_salt("abr")
                    .selected_text(format!("{} kbps", s.encoder.audio_bitrate_kbps))
                    .show_ui(ui, |ui| {
                        for &b in AUDIO_BITRATES_KBPS {
                            ui.selectable_value(
                                &mut s.encoder.audio_bitrate_kbps,
                                b,
                                format!("{b} kbps"),
                            );
                        }
                    });
            });
            ui.end_row();

            ui.label(egui::RichText::new("Recording").strong());
            ui.end_row();
            ui.label("Folder");
            let mut dir = s.recording.directory.display().to_string();
            if ui.text_edit_singleline(&mut dir).changed() {
                s.recording.directory = dir.into();
            }
            ui.end_row();

            ui.label(egui::RichText::new("Stream").strong());
            ui.end_row();
            ui.label("Service");
            let current = STREAM_PRESETS
                .iter()
                .find(|(_, url)| *url == s.stream.server)
                .map_or("Custom", |(n, _)| n);
            egui::ComboBox::from_id_salt("service")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (name, url) in STREAM_PRESETS {
                        if ui
                            .selectable_label(s.stream.server == *url, *name)
                            .clicked()
                        {
                            s.stream.server = url.to_string();
                        }
                    }
                });
            ui.end_row();
            ui.label("Server");
            ui.add(egui::TextEdit::singleline(&mut s.stream.server).desired_width(320.0));
            ui.end_row();
            ui.label("Stream key");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut s.stream.key)
                        .password(!st.show_key)
                        .desired_width(260.0),
                );
                ui.checkbox(&mut st.show_key, "show");
            });
            ui.end_row();
        });
    if live {
        ui.label("Video and encoder settings are locked while streaming or recording.");
    }
}

/// Canvas preview with click-to-select, drag-to-move and corner-drag-to-resize.
fn preview(ui: &mut Ui, st: &mut UiState, project: &mut Project) {
    let (cw, ch) = project.canvas_size();
    let avail = ui.available_rect_before_wrap();
    let fit =
        Rect::new(avail.min.x, avail.min.y, avail.width(), avail.height()).fit_aspect(cw / ch);
    let rect = ERect::from_min_size(Pos2::new(fit.x, fit.y), Vec2::new(fit.w, fit.h));
    let response = ui.allocate_rect(rect, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.image(
        PREVIEW,
        rect,
        ERect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );

    let scale = rect.width() / cw;
    let to_screen = |r: &Rect| {
        ERect::from_min_size(
            rect.min + Vec2::new(r.x, r.y) * scale,
            Vec2::new(r.w, r.h) * scale,
        )
    };
    let to_canvas = |p: Pos2| ((p.x - rect.min.x) / scale, (p.y - rect.min.y) / scale);
    const HANDLE: f32 = 8.0;

    if response.drag_started() || response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let on_handle = st
                .selected
                .and_then(|id| project.active().item(id))
                .is_some_and(|item| (to_screen(&item.rect).max - pos).length() <= HANDLE * 1.5);
            if on_handle {
                st.drag = Some(Drag::Resize);
            } else {
                let (x, y) = to_canvas(pos);
                st.selected = project.active().hit_test(x, y);
                st.drag = st.selected.map(|_| Drag::Move);
            }
        }
    }
    if response.dragged() {
        if let (Some(id), Some(mode)) = (st.selected, st.drag) {
            let delta = response.drag_delta() / scale;
            let free = ui.input(|i| i.modifiers.shift);
            if let Some(item) = project.active_mut().item_mut(id) {
                match mode {
                    Drag::Move => {
                        item.rect.x += delta.x;
                        item.rect.y += delta.y;
                    }
                    Drag::Resize => {
                        let aspect = item.rect.w / item.rect.h.max(1.0);
                        item.rect.w = (item.rect.w + delta.x).max(16.0);
                        item.rect.h = if free {
                            (item.rect.h + delta.y).max(16.0)
                        } else {
                            item.rect.w / aspect
                        };
                    }
                }
            }
        }
    }
    if response.drag_stopped() {
        st.drag = None;
    }

    if let Some(item) = st.selected.and_then(|id| project.active().item(id)) {
        let r = to_screen(&item.rect);
        let stroke = Stroke::new(1.5, Color32::from_rgb(255, 80, 80));
        painter.rect_stroke(r, CornerRadius::ZERO, stroke, StrokeKind::Inside);
        painter.rect_filled(
            ERect::from_center_size(r.max, Vec2::splat(HANDLE)),
            CornerRadius::ZERO,
            Color32::from_rgb(255, 80, 80),
        );
    }
}
