//! Tiny logger: stderr plus `ferrocast.log` next to the project file.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

struct Logger {
    file: Option<Mutex<File>>,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info || m.target().starts_with("ferrocast")
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

/// Directory for the project file and log: `%APPDATA%\Ferrocast`.
pub fn data_dir() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".".into())
        .join("Ferrocast")
}

pub fn init() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let file = File::create(dir.join("ferrocast.log")).ok().map(Mutex::new);
    let logger = Box::leak(Box::new(Logger { file }));
    let _ = log::set_logger(logger);
    log::set_max_level(if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    });
}
