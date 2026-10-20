//! Tiny logger: stderr plus `chimcast.log` next to the project file.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

struct Logger {
    file: Option<Mutex<File>>,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info || m.target().starts_with("chimcast")
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!("[{}] {}: {}\n", r.level(), r.target(), r.args());
        eprint!("{line}");
        if let Some(f) = &self.file {
            let _ = f.lock().unwrap().write_all(line.as_bytes());
        }
    }

    fn flush(&self) {}
}

/// Directory for the project file and log: `%APPDATA%\Chimcast`.
pub fn data_dir() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".".into())
        .join("Chimcast")
}

pub fn init() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    // The app used to be called Ferrocast: carry its scenes and settings over once.
    let old = dir.with_file_name("Ferrocast").join("project.json");
    let new = dir.join("project.json");
    if !new.exists() && old.exists() {
        let _ = std::fs::copy(&old, &new);
    }
    let file = File::create(dir.join("chimcast.log")).ok().map(Mutex::new);
    let logger = Box::leak(Box::new(Logger { file }));
    let _ = log::set_logger(logger);
    log::set_max_level(if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });
}
