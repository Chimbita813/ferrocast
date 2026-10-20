//! The main window: preview on top, then scenes, sources, audio mixer and controls.

use std::time::{Duration, Instant};

use chimcast_core::{
    AudioSourceKind, Id, Project, Rect, VideoSourceKind, AUDIO_BITRATES_KBPS, STREAM_PRESETS,
};
use chimcast_media::output::OutputState;
use chimcast_win::{devices, Engine};
use egui::{Color32, CornerRadius, Pos2, Rect as ERect, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::painter::PREVIEW;

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Move,
    Resize,
}

#[derive(Clone, Copy, PartialEq)]
enum AddDialog {
    Image,
    Browser,
}

#[derive(Default)]
pub struct UiState {
    add_dialog: Option<AddDialog>,
    show_connect: bool,
    show_tests: bool,
    show_token: bool,
    /// Result of the last test alert, filled in by its background thread.
    alert_result: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    stream_error: Option<String>,
    selected: Option<Id>,
    drag: Option<Drag>,
    show_settings: bool,
    show_key: bool,
    image_path: String,
    browser_url: String,
    browser_size: [u32; 2],
    /// Edit buffer for the selected browser source: (item, url, size).
    browser_edit: Option<(Id, String, [u32; 2])>,
    message: Option<(String, Instant)>,
    recording_started: Option<Instant>,
    streaming_started: Option<Instant>,
    /// Updater status, filled in by the app each frame.
    pub update: Option<String>,
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

    let alert = st.alert_result.lock().unwrap().take();
    if let Some(msg) = alert {
        st.say(msg);
    }
    egui::Panel::top("test-widgets").show(ui, |ui| test_widgets(ui, st, project));
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

    if st.show_token {
        token_dialog(ui.ctx(), st, project);
    }
    if st.show_connect {
        connect_dialog(ui.ctx(), st, project, engine);
    }
    if let Some(dialog) = st.add_dialog {
        add_dialog(ui.ctx(), st, project, dialog);
    }
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
        if let Some(note) = &st.update {
            ui.separator();
            ui.label(note);
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
    let count = project.active().items.len();
    if count == 0 {
        ui.weak("No sources yet. Use \"+ Add source\" below.");
    } else {
        ui.weak("Top layer first. Click the eye to show or hide.");
    }
    egui::ScrollArea::vertical()
        .id_salt("sources")
        .max_height((ui.available_height() * 0.45).max(60.0))
        .show(ui, |ui| {
            let scene = project.active_mut();
            // Top of the stack first, like OBS.
            for (layer, item) in scene.items.iter_mut().rev().enumerate() {
                let error = engine.source_error(item.id);
                source_row(ui, st, item, layer + 1, error.as_deref());
            }
        });
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.menu_button("+ Add source", |ui| {
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
            // Text fields live in their own window: a menu closes on the first click inside it.
            if ui.button("Image...").clicked() {
                st.add_dialog = Some(AddDialog::Image);
                ui.close();
            }
            if ui.button("Browser...").clicked() {
                st.add_dialog = Some(AddDialog::Browser);
                ui.close();
            }
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
            let items = &project.active().items;
            let pos = items.iter().position(|i| i.id == sel);
            let at_top = pos.is_none_or(|p| p + 1 == items.len());
            let at_bottom = pos.is_none_or(|p| p == 0);
            if ui.button("Remove").clicked() {
                project.remove_item(scene_id, sel);
                st.selected = None;
            }
            if ui
                .add_enabled(!at_top, egui::Button::new("Move up"))
                .on_hover_text("Bring forward one layer")
                .clicked()
            {
                project.active_mut().raise(sel);
            }
            if ui
                .add_enabled(!at_bottom, egui::Button::new("Move down"))
                .on_hover_text("Send back one layer")
                .clicked()
            {
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
    let canvas = project.canvas_size();
    if let Some(item) = st.selected.and_then(|id| project.active_mut().item_mut(id)) {
        ui.add_space(4.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt("source-props")
                .show(ui, |ui| properties(ui, st, item, engine, canvas));
        });
    }
}

fn properties(
    ui: &mut Ui,
    st: &mut UiState,
    item: &mut chimcast_core::SceneItem,
    engine: &Engine,
    canvas: (f32, f32),
) {
    ui.label(egui::RichText::new(format!("{} properties", item.source.label())).strong());
    ui.horizontal(|ui| {
        ui.label("Name");
        ui.text_edit_singleline(&mut item.name);
    });
    ui.checkbox(&mut item.visible, "Visible");
    ui.horizontal_wrapped(|ui| {
        ui.label("Position");
        ui.add(
            egui::DragValue::new(&mut item.rect.x)
                .speed(1.0)
                .prefix("x "),
        );
        ui.add(
            egui::DragValue::new(&mut item.rect.y)
                .speed(1.0)
                .prefix("y "),
        );
        ui.label("Size");
        ui.add(
            egui::DragValue::new(&mut item.rect.w)
                .speed(1.0)
                .prefix("w "),
        );
        ui.add(
            egui::DragValue::new(&mut item.rect.h)
                .speed(1.0)
                .prefix("h "),
        );
    });
    if let VideoSourceKind::Color { rgba } = &mut item.source {
        ui.color_edit_button_srgba_unmultiplied(rgba);
    }
    if let VideoSourceKind::Browser { url, width, height } = &mut item.source {
        // Edits go to a buffer: changing the source re-creates the page.
        if st
            .browser_edit
            .as_ref()
            .is_none_or(|(id, ..)| *id != item.id)
        {
            st.browser_edit = Some((item.id, url.clone(), [*width, *height]));
        }
        let (_, edit_url, edit_size) = st.browser_edit.as_mut().unwrap();
        ui.horizontal(|ui| {
            ui.label("URL");
            ui.add(egui::TextEdit::singleline(edit_url).desired_width(f32::INFINITY));
        });
        ui.horizontal_wrapped(|ui| page_size(ui, edit_size));
        ui.horizontal(|ui| {
            let changed = *edit_url.trim() != *url || *edit_size != [*width, *height];
            if ui
                .add_enabled(changed, egui::Button::new("Apply"))
                .clicked()
            {
                *url = edit_url.trim().to_string();
                [*width, *height] = *edit_size;
                item.rect = page_rect(*width, *height, canvas);
            }
            if ui
                .button("Refresh")
                .on_hover_text("Reload the page, e.g. after changing the widget in Streamlabs")
                .clicked()
            {
                engine.reload_source(item.id);
            }
        });
        if let Some(err) = engine.source_error(item.id) {
            ui.colored_label(Color32::LIGHT_RED, err);
        }
    }
}

/// One line in the source list: eye toggle, layer number, name, type.
fn source_row(
    ui: &mut Ui,
    st: &mut UiState,
    item: &mut chimcast_core::SceneItem,
    layer: usize,
    error: Option<&str>,
) {
    let selected = st.selected == Some(item.id);
    let visuals = ui.visuals().clone();
    let fill = if selected {
        visuals.selection.bg_fill
    } else {
        Color32::TRANSPARENT
    };
    let row = egui::Frame::NONE
        .fill(fill)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let eye = eye_icon(ui, item.visible);
                ui.weak(format!("{layer}."));
                let mut name = egui::RichText::new(&item.name);
                if !item.visible {
                    name = name.weak().italics();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let kind = egui::RichText::new(item.source.label()).small().weak();
                    ui.add(egui::Label::new(kind).selectable(false));
                    if !item.visible {
                        let hidden = egui::RichText::new("hidden").small().weak();
                        ui.add(egui::Label::new(hidden).selectable(false));
                    }
                    if let Some(err) = error {
                        ui.colored_label(Color32::LIGHT_RED, "error")
                            .on_hover_text(err);
                    }
                    // The name takes whatever width is left, cut short with "...".
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(name).truncate().selectable(false));
                    });
                });
                eye
            })
            .inner
        });
    // One click target for the whole row (drawn on top of its contents): the eye
    // toggles visibility, anywhere else selects.
    let eye = row.inner;
    let response = ui.interact(
        row.response.rect,
        ui.make_persistent_id(("source-row", item.id.0)),
        Sense::click(),
    );
    if response.clicked() {
        let on_eye = response
            .interact_pointer_pos()
            .is_some_and(|p| eye.expand(3.0).contains(p));
        if on_eye {
            item.visible = !item.visible;
        } else {
            st.selected = Some(item.id);
        }
    }
    if response.hovered() {
        let on_eye = response
            .hover_pos()
            .is_some_and(|p| eye.expand(3.0).contains(p));
        if on_eye {
            response
                .clone()
                .on_hover_text(if item.visible { "Hide" } else { "Show" });
        }
    }
    if response.hovered() && !selected {
        ui.painter().rect_stroke(
            row.response.rect,
            CornerRadius::same(4),
            Stroke::new(1.0, visuals.widgets.hovered.bg_stroke.color),
            StrokeKind::Inside,
        );
    }
}

/// Paints an eye: open when visible, crossed out when hidden. Returns its rect.
fn eye_icon(ui: &mut Ui, visible: bool) -> ERect {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(20.0, 16.0), Sense::hover());
    let color = if visible {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let p = ui.painter();
    let c = rect.center();
    let stroke = Stroke::new(1.5, color);
    // Almond outline from two arcs, approximated with line segments.
    let (hw, hh) = (8.0, 5.0);
    let arc = |sign: f32| -> Vec<Pos2> {
        (0..=12)
            .map(|i| {
                let t = i as f32 / 12.0 * std::f32::consts::PI;
                Pos2::new(c.x - hw * t.cos(), c.y + sign * hh * t.sin())
            })
            .collect()
    };
    p.add(egui::Shape::line(arc(-1.0), stroke));
    p.add(egui::Shape::line(arc(1.0), stroke));
    if visible {
        p.circle_filled(c, 2.5, color);
    } else {
        p.line_segment([c + Vec2::new(-7.0, 6.0), c + Vec2::new(7.0, -6.0)], stroke);
    }
    rect
}

/// The "Test Widgets" strip above the preview, like Streamlabs Desktop.
fn test_widgets(ui: &mut Ui, st: &mut UiState, project: &Project) {
    use chimcast_win::streamlabs::{send_test_alert, TEST_ALERTS};
    ui.horizontal(|ui| {
        ui.toggle_value(&mut st.show_tests, "Test Widgets");
        if !st.show_tests {
            return;
        }
        ui.separator();
        for (label, kind) in TEST_ALERTS {
            let pill = egui::Button::new(*label).corner_radius(CornerRadius::same(12));
            if !ui.add(pill).clicked() {
                continue;
            }
            let token = project.settings.stream.streamlabs_token.trim().to_string();
            if token.is_empty() {
                st.show_token = true;
                continue;
            }
            st.say(format!("Sending a test {label} alert..."));
            let result = st.alert_result.clone();
            let (label, kind) = (*label, *kind);
            std::thread::spawn(move || {
                let msg = match send_test_alert(&token, kind) {
                    Ok(()) => format!("Test {label} sent"),
                    Err(e) => {
                        log::warn!("test {kind} alert: {e}");
                        format!("Test {label} failed: {e}")
                    }
                };
                *result.lock().unwrap() = Some(msg);
            });
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Token...").clicked() {
                st.show_token = true;
            }
        });
    });
}

/// Asks for the Streamlabs API access token the test buttons need.
fn token_dialog(ctx: &egui::Context, st: &mut UiState, project: &mut Project) {
    let mut open = true;
    let mut done = false;
    egui::Window::new("Connect Streamlabs")
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label("Test Widgets plays test alerts on your Streamlabs widgets.");
            ui.label("1. Open Streamlabs API Settings, then the \"API Tokens\" tab:");
            if ui.button("Open Streamlabs API Settings").clicked() {
                crate::dialog::open_url(chimcast_win::streamlabs::TOKEN_PAGE);
            }
            ui.label("2. Copy \"Your API Access Token\" and paste it here:");
            let token = &mut project.settings.stream.streamlabs_token;
            ui.add(
                egui::TextEdit::singleline(token)
                    .password(true)
                    .desired_width(360.0),
            );
            ui.weak("Keep it private: it gives access to your Streamlabs account.");
            if ui
                .add_enabled(!token.trim().is_empty(), egui::Button::new("Save"))
                .clicked()
            {
                done = true;
            }
        });
    if done || !open {
        st.show_token = false;
    }
}

fn go_live(st: &mut UiState, project: &Project, engine: &mut Engine) {
    match engine.start_streaming(project) {
        Ok(()) => {
            st.stream_error = None;
            st.streaming_started = Some(Instant::now());
            st.say("Connecting to the stream server...");
        }
        Err(e) => {
            log::warn!("can't stream: {e}");
            st.stream_error = Some(e);
        }
    }
}

/// Where each service shows the stream key.
fn key_page(service: &str) -> Option<&'static str> {
    match service {
        "Twitch" => Some("https://dashboard.twitch.tv/settings/stream"),
        "YouTube" => Some("https://studio.youtube.com/channel/UC/livestreaming"),
        "Kick" => Some("https://kick.com/dashboard/settings/stream"),
        _ => None,
    }
}

/// "Connect to Twitch": pick the service, paste the stream key, go live.
fn connect_dialog(
    ctx: &egui::Context,
    st: &mut UiState,
    project: &mut Project,
    engine: &mut Engine,
) {
    let mut open = true;
    let mut go = false;
    egui::Window::new("Connect your stream")
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let s = &mut project.settings.stream;
            let service = STREAM_PRESETS
                .iter()
                .find(|(_, url)| *url == s.server)
                .map(|(name, _)| *name)
                .unwrap_or("Custom");
            ui.horizontal(|ui| {
                ui.label("Service");
                for (name, url) in STREAM_PRESETS {
                    if ui.selectable_label(s.server == *url, *name).clicked() {
                        s.server = url.to_string();
                    }
                }
            });
            ui.add_space(6.0);
            if let Some(page) = key_page(service) {
                ui.label(format!(
                    "1. Copy your stream key from the {service} dashboard:"
                ));
                if ui
                    .button(format!("Open {service} stream key page"))
                    .clicked()
                {
                    crate::dialog::open_url(page);
                }
                ui.add_space(4.0);
                ui.label("2. Paste it here (Ctrl+V):");
            } else {
                ui.label(format!("Server: {}", s.server));
                ui.label("Stream key:");
            }
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut s.key)
                        .password(!st.show_key)
                        .hint_text("live_...")
                        .desired_width(320.0),
                );
                ui.checkbox(&mut st.show_key, "show");
            });
            ui.weak("Keep this key private: anyone with it can stream to your channel.");
            ui.add_space(6.0);
            let ready = !s.key.trim().is_empty();
            if ui
                .add_enabled(
                    ready,
                    egui::Button::new(format!("Save and go live on {service}")),
                )
                .clicked()
            {
                go = true;
            }
        });
    if go {
        st.show_connect = false;
        go_live(st, project, engine);
    } else if !open {
        st.show_connect = false;
    }
}

fn add_dialog(ctx: &egui::Context, st: &mut UiState, project: &mut Project, dialog: AddDialog) {
    let title = match dialog {
        AddDialog::Image => "Add image",
        AddDialog::Browser => "Add browser source",
    };
    let mut open = true;
    let mut done = false;
    egui::Window::new(title)
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| match dialog {
            AddDialog::Image => {
                ui.label("Path to a PNG/JPEG/BMP file:");
                let field = ui
                    .horizontal(|ui| {
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut st.image_path)
                                .hint_text("C:\\Users\\you\\Pictures\\logo.png")
                                .desired_width(340.0),
                        );
                        if ui.button("Browse...").clicked() {
                            if let Some(p) = crate::dialog::pick_file(
                                "Choose an image",
                                &[
                                    ("Images", "*.png;*.jpg;*.jpeg;*.bmp;*.gif"),
                                    ("All files", "*.*"),
                                ],
                            ) {
                                st.image_path = p.display().to_string();
                            }
                        }
                        field
                    })
                    .inner;
                let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.button("Add").clicked() || enter) && !st.image_path.trim().is_empty() {
                    let path = std::path::PathBuf::from(st.image_path.trim().trim_matches('"'));
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Image".into());
                    add_source(project, st, name, VideoSourceKind::Image { path });
                    st.image_path.clear();
                    done = true;
                }
            }
            AddDialog::Browser => {
                if st.browser_size == [0, 0] {
                    // Overlays are usually designed for the full canvas.
                    let s = &project.settings.video;
                    st.browser_size = [s.width, s.height];
                }
                ui.label("Widget URL (from the Streamlabs dashboard) or path to an HTML file:");
                let field = ui
                    .horizontal(|ui| {
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut st.browser_url)
                                .hint_text("https://streamlabs.com/alert-box/v3/...")
                                .desired_width(340.0),
                        );
                        if ui.button("Browse...").clicked() {
                            if let Some(p) = crate::dialog::pick_file(
                                "Choose a web page",
                                &[("Web pages", "*.html;*.htm"), ("All files", "*.*")],
                            ) {
                                st.browser_url = p.display().to_string();
                            }
                        }
                        field
                    })
                    .inner;
                let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal_wrapped(|ui| page_size(ui, &mut st.browser_size));
                if (ui.button("Add").clicked() || enter) && !st.browser_url.trim().is_empty() {
                    let url = st.browser_url.trim().to_string();
                    let [width, height] = st.browser_size;
                    add_source(
                        project,
                        st,
                        browser_name(&url),
                        VideoSourceKind::Browser { url, width, height },
                    );
                    st.browser_url.clear();
                    done = true;
                }
            }
        });
    if !open || done {
        st.add_dialog = None;
    }
}

/// Where a page of this size goes: covering the canvas if it matches it,
/// otherwise centered at 1:1.
fn page_rect(width: u32, height: u32, (cw, ch): (f32, f32)) -> Rect {
    let (w, h) = (width as f32, height as f32);
    Rect::new((cw - w) / 2.0, (ch - h) / 2.0, w, h)
}

fn page_size(ui: &mut Ui, size: &mut [u32; 2]) {
    ui.label("Page size");
    for preset in [[800, 600], [1280, 720], [1920, 1080]] {
        let label = format!("{}x{}", preset[0], preset[1]);
        if ui.selectable_label(*size == preset, label).clicked() {
            *size = preset;
        }
    }
    ui.add(
        egui::DragValue::new(&mut size[0])
            .range(16..=7680)
            .prefix("w "),
    );
    ui.add(
        egui::DragValue::new(&mut size[1])
            .range(16..=4320)
            .prefix("h "),
    );
}

/// "streamlabs.com/alert-box/..." -> "Alert Box"-ish short names for the list.
fn browser_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let mut parts = path.trim_end_matches('/').rsplit(['/', '\\']);
    let last = parts.next().unwrap_or_default();
    let name = if url.contains("streamlabs.com") {
        path.split('/').find(|p| p.contains('-')).unwrap_or(last)
    } else {
        last
    };
    if name.is_empty() || name.contains(':') {
        "Browser".into()
    } else {
        name.to_string()
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
                        if ui.small_button("x").on_hover_text("Remove").clicked() {
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
    ui.menu_button("+ Add audio", |ui| {
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
                if project.settings.stream.key.trim().is_empty() {
                    // No key yet: ask for one instead of failing quietly.
                    st.show_connect = true;
                } else {
                    go_live(st, project, engine);
                }
            }
            if let Some(err) = &st.stream_error {
                ui.colored_label(Color32::LIGHT_RED, format!("Can't stream: {err}"));
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

            ui.label("Streamlabs token");
            ui.add(
                egui::TextEdit::singleline(&mut s.stream.streamlabs_token)
                    .password(true)
                    .hint_text("for Test Widgets")
                    .desired_width(260.0),
            );
            ui.end_row();

            ui.label(egui::RichText::new("App").strong());
            ui.end_row();
            ui.label("Updates");
            ui.checkbox(
                &mut s.auto_update,
                "Install new builds when Chimcast starts",
            );
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
