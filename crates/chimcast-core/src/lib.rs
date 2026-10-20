//! Project model for Chimcast.
//!
//! Everything here is plain data: scenes, the sources placed in them, global
//! audio sources and output settings. The Windows engine reads a [`Project`]
//! and turns it into capture sessions, a compositor pass and encoders.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid project file: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Stable identifier for scenes, scene items and audio sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Id(pub u64);

/// Axis-aligned rectangle in canvas pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    /// Largest rect with the given aspect ratio that fits inside `self`, centered.
    pub fn fit_aspect(&self, aspect: f32) -> Rect {
        if aspect <= 0.0 || self.w <= 0.0 || self.h <= 0.0 {
            return *self;
        }
        let (w, h) = if self.w / self.h > aspect {
            (self.h * aspect, self.h)
        } else {
            (self.w, self.w / aspect)
        };
        Rect::new(
            self.x + (self.w - w) / 2.0,
            self.y + (self.h - h) / 2.0,
            w,
            h,
        )
    }
}

/// What a scene item shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VideoSourceKind {
    /// A whole monitor, by its enumeration index.
    Monitor { index: u32 },
    /// A top-level window, matched by title (and executable name when known).
    Window { title: String, exe: Option<String> },
    /// A webcam, by Media Foundation symbolic link.
    Webcam { device_id: String },
    /// A still image file (PNG, JPEG, BMP, ...).
    Image { path: PathBuf },
    /// A solid color, straight (non-premultiplied) RGBA.
    Color { rgba: [u8; 4] },
    /// A web page (e.g. a Streamlabs alert widget) rendered with a transparent
    /// background at `width` x `height`. `url` may also be a local file path.
    Browser {
        url: String,
        width: u32,
        height: u32,
    },
}

impl VideoSourceKind {
    pub fn label(&self) -> &'static str {
        match self {
            VideoSourceKind::Monitor { .. } => "Display",
            VideoSourceKind::Window { .. } => "Window",
            VideoSourceKind::Webcam { .. } => "Webcam",
            VideoSourceKind::Image { .. } => "Image",
            VideoSourceKind::Color { .. } => "Color",
            VideoSourceKind::Browser { .. } => "Browser",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneItem {
    pub id: Id,
    pub name: String,
    pub source: VideoSourceKind,
    pub rect: Rect,
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    pub id: Id,
    pub name: String,
    /// Draw order: the first item is at the bottom.
    pub items: Vec<SceneItem>,
}

impl Scene {
    pub fn item(&self, id: Id) -> Option<&SceneItem> {
        self.items.iter().find(|i| i.id == id)
    }

    pub fn item_mut(&mut self, id: Id) -> Option<&mut SceneItem> {
        self.items.iter_mut().find(|i| i.id == id)
    }

    /// Topmost visible item under a canvas point.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<Id> {
        self.items
            .iter()
            .rev()
            .find(|i| i.visible && i.rect.contains(x, y))
            .map(|i| i.id)
    }

    /// Moves an item one step up (towards the top of the stack). Returns false if it can't move.
    pub fn raise(&mut self, id: Id) -> bool {
        match self.items.iter().position(|i| i.id == id) {
            Some(pos) if pos + 1 < self.items.len() => {
                self.items.swap(pos, pos + 1);
                true
            }
            _ => false,
        }
    }

    /// Moves an item one step down (towards the bottom of the stack). Returns false if it can't move.
    pub fn lower(&mut self, id: Id) -> bool {
        match self.items.iter().position(|i| i.id == id) {
            Some(pos) if pos > 0 => {
                self.items.swap(pos, pos - 1);
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioSourceKind {
    /// Capture device; `None` follows the Windows default microphone.
    Microphone { device_id: Option<String> },
    /// Loopback of a playback device; `None` follows the Windows default output.
    Desktop { device_id: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSource {
    pub id: Id,
    pub name: String,
    pub kind: AudioSourceKind,
    /// Linear gain, 0.0 ..= 2.0.
    pub volume: f32,
    pub muted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EncoderSettings {
    pub video_bitrate_kbps: u32,
    pub keyframe_interval_secs: u32,
    /// AAC bitrate. Media Foundation's encoder only accepts 96, 128, 160 or 192.
    pub audio_bitrate_kbps: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordingSettings {
    pub directory: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamSettings {
    /// RTMP ingest URL, e.g. `rtmp://live.twitch.tv/app`.
    pub server: String,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub video: VideoSettings,
    pub encoder: EncoderSettings,
    pub recording: RecordingSettings,
    pub stream: StreamSettings,
    /// Check GitHub for a newer build on launch and install it.
    #[serde(default = "enabled")]
    pub auto_update: bool,
}

fn enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            video: VideoSettings {
                width: 1920,
                height: 1080,
                fps: 60,
            },
            encoder: EncoderSettings {
                video_bitrate_kbps: 6000,
                keyframe_interval_secs: 2,
                audio_bitrate_kbps: 160,
            },
            recording: RecordingSettings {
                directory: PathBuf::from("."),
            },
            stream: StreamSettings {
                server: STREAM_PRESETS[0].1.to_string(),
                key: String::new(),
            },
            auto_update: true,
        }
    }
}

/// Well-known ingest servers shown in the settings UI.
pub const STREAM_PRESETS: &[(&str, &str)] = &[
    ("Twitch", "rtmp://live.twitch.tv/app"),
    ("YouTube", "rtmp://a.rtmp.youtube.com/live2"),
    (
        "Kick",
        "rtmp://fa723fc1b171.global-contribute.live-video.net/app",
    ),
];

/// AAC bitrates the Windows encoder supports.
pub const AUDIO_BITRATES_KBPS: &[u32] = &[96, 128, 160, 192];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub scenes: Vec<Scene>,
    pub active_scene: Id,
    pub audio: Vec<AudioSource>,
    pub settings: Settings,
    next_id: u64,
}

impl Default for Project {
    /// One scene showing the primary display, plus desktop audio and the default mic.
    fn default() -> Self {
        let mut p = Project {
            scenes: Vec::new(),
            active_scene: Id(0),
            audio: Vec::new(),
            settings: Settings::default(),
            next_id: 1,
        };
        let scene = p.add_scene("Scene 1");
        p.active_scene = scene;
        p.add_item(scene, "Display 1", VideoSourceKind::Monitor { index: 0 });
        p.add_audio(
            "Desktop Audio",
            AudioSourceKind::Desktop { device_id: None },
        );
        p.add_audio("Mic", AudioSourceKind::Microphone { device_id: None });
        p
    }
}

impl Project {
    fn alloc_id(&mut self) -> Id {
        let id = Id(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn canvas_size(&self) -> (f32, f32) {
        (
            self.settings.video.width as f32,
            self.settings.video.height as f32,
        )
    }

    pub fn scene(&self, id: Id) -> Option<&Scene> {
        self.scenes.iter().find(|s| s.id == id)
    }

    pub fn scene_mut(&mut self, id: Id) -> Option<&mut Scene> {
        self.scenes.iter_mut().find(|s| s.id == id)
    }

    pub fn active(&self) -> &Scene {
        self.scene(self.active_scene).unwrap_or(&self.scenes[0])
    }

    pub fn active_mut(&mut self) -> &mut Scene {
        let id = self.active_scene;
        let idx = self.scenes.iter().position(|s| s.id == id).unwrap_or(0);
        &mut self.scenes[idx]
    }

    pub fn add_scene(&mut self, name: &str) -> Id {
        let id = self.alloc_id();
        self.scenes.push(Scene {
            id,
            name: name.to_string(),
            items: Vec::new(),
        });
        id
    }

    /// Removes a scene. The last remaining scene can't be removed.
    pub fn remove_scene(&mut self, id: Id) -> bool {
        if self.scenes.len() <= 1 {
            return false;
        }
        let before = self.scenes.len();
        self.scenes.retain(|s| s.id != id);
        if self.active_scene == id {
            self.active_scene = self.scenes[0].id;
        }
        self.scenes.len() != before
    }

    /// Adds a source on top of a scene with a sensible default placement.
    pub fn add_item(&mut self, scene: Id, name: &str, source: VideoSourceKind) -> Option<Id> {
        let (cw, ch) = self.canvas_size();
        let rect = match source {
            VideoSourceKind::Monitor { .. }
            | VideoSourceKind::Window { .. }
            | VideoSourceKind::Color { .. } => Rect::new(0.0, 0.0, cw, ch),
            // Pages keep their own pixel size, centered (widgets are mostly transparent).
            VideoSourceKind::Browser { width, height, .. } => {
                let (w, h) = (width as f32, height as f32);
                Rect::new((cw - w) / 2.0, (ch - h) / 2.0, w, h)
            }
            // Webcams and images start in the bottom-right quarter.
            VideoSourceKind::Webcam { .. } | VideoSourceKind::Image { .. } => {
                Rect::new(cw * 0.7, ch * 0.7, cw * 0.28, ch * 0.28)
            }
        };
        let id = self.alloc_id();
        self.scene_mut(scene)?.items.push(SceneItem {
            id,
            name: name.to_string(),
            source,
            rect,
            visible: true,
        });
        Some(id)
    }

    pub fn remove_item(&mut self, scene: Id, item: Id) -> bool {
        match self.scene_mut(scene) {
            Some(s) => {
                let before = s.items.len();
                s.items.retain(|i| i.id != item);
                s.items.len() != before
            }
            None => false,
        }
    }

    pub fn add_audio(&mut self, name: &str, kind: AudioSourceKind) -> Id {
        let id = self.alloc_id();
        self.audio.push(AudioSource {
            id,
            name: name.to_string(),
            kind,
            volume: 1.0,
            muted: false,
        });
        id
    }

    pub fn remove_audio(&mut self, id: Id) -> bool {
        let before = self.audio.len();
        self.audio.retain(|a| a.id != id);
        self.audio.len() != before
    }

    pub fn load(path: &Path) -> Result<Project> {
        let text = std::fs::read_to_string(path)?;
        let mut p: Project = serde_json::from_str(&text)?;
        // Guard against hand-edited files.
        if p.scenes.is_empty() {
            p.add_scene("Scene 1");
        }
        if p.scene(p.active_scene).is_none() {
            p.active_scene = p.scenes[0].id;
        }
        let max_id = p
            .scenes
            .iter()
            .flat_map(|s| std::iter::once(s.id.0).chain(s.items.iter().map(|i| i.id.0)))
            .chain(p.audio.iter().map(|a| a.id.0))
            .max()
            .unwrap_or(0);
        p.next_id = p.next_id.max(max_id + 1);
        Ok(p)
    }

    /// Writes the project atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_project_has_display_and_audio() {
        let p = Project::default();
        assert_eq!(p.scenes.len(), 1);
        assert_eq!(p.active().items.len(), 1);
        assert_eq!(
            p.active().items[0].rect,
            Rect::new(0.0, 0.0, 1920.0, 1080.0)
        );
        assert_eq!(p.audio.len(), 2);
    }

    #[test]
    fn ids_are_unique() {
        let mut p = Project::default();
        let s = p.add_scene("Two");
        let a = p
            .add_item(
                s,
                "Cam",
                VideoSourceKind::Webcam {
                    device_id: "x".into(),
                },
            )
            .unwrap();
        let b = p
            .add_item(
                s,
                "Red",
                VideoSourceKind::Color {
                    rgba: [255, 0, 0, 255],
                },
            )
            .unwrap();
        let mut ids = vec![s, a, b, p.scenes[0].id, p.scenes[0].items[0].id];
        ids.extend(p.audio.iter().map(|a| a.id));
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn cannot_remove_last_scene_and_active_follows() {
        let mut p = Project::default();
        let first = p.active_scene;
        assert!(!p.remove_scene(first));
        let second = p.add_scene("Second");
        p.active_scene = second;
        assert!(p.remove_scene(second));
        assert_eq!(p.active_scene, first);
    }

    #[test]
    fn hit_test_picks_topmost_visible() {
        let mut p = Project::default();
        let s = p.active_scene;
        let cam = p
            .add_item(
                s,
                "Cam",
                VideoSourceKind::Webcam {
                    device_id: "x".into(),
                },
            )
            .unwrap();
        let display = p.active().items[0].id;
        assert_eq!(p.active().hit_test(1800.0, 1000.0), Some(cam));
        assert_eq!(p.active().hit_test(10.0, 10.0), Some(display));
        p.active_mut().item_mut(cam).unwrap().visible = false;
        assert_eq!(p.active().hit_test(1800.0, 1000.0), Some(display));
    }

    #[test]
    fn reorder_items() {
        let mut p = Project::default();
        let s = p.active_scene;
        let display = p.active().items[0].id;
        let cam = p
            .add_item(
                s,
                "Cam",
                VideoSourceKind::Webcam {
                    device_id: "x".into(),
                },
            )
            .unwrap();
        assert!(!p.active_mut().raise(cam));
        assert!(p.active_mut().lower(cam));
        assert_eq!(p.active().items[0].id, cam);
        assert!(!p.active_mut().lower(cam));
        assert!(p.active_mut().raise(cam));
        assert_eq!(p.active().items[0].id, display);
    }

    #[test]
    fn fit_aspect_letterboxes() {
        let r = Rect::new(0.0, 0.0, 200.0, 100.0).fit_aspect(1.0);
        assert_eq!(r, Rect::new(50.0, 0.0, 100.0, 100.0));
        let r = Rect::new(0.0, 0.0, 100.0, 200.0).fit_aspect(2.0);
        assert_eq!(r, Rect::new(0.0, 75.0, 100.0, 50.0));
    }

    #[test]
    fn save_load_roundtrip_and_id_recovery() {
        let dir = std::env::temp_dir().join(format!("chimcast-test-{}", std::process::id()));
        let path = dir.join("project.json");
        let mut p = Project::default();
        p.settings.stream.key = "live_123".into();
        p.save(&path).unwrap();
        let mut loaded = Project::load(&path).unwrap();
        assert_eq!(loaded, p);
        // A stale next_id from a hand-edited file must not cause collisions.
        loaded.next_id = 0;
        loaded.save(&path).unwrap();
        let mut again = Project::load(&path).unwrap();
        let new = again.add_scene("New");
        assert!(new.0 > p.audio.iter().map(|a| a.id.0).max().unwrap());
        std::fs::remove_dir_all(dir).ok();
    }
}
