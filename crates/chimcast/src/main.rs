//! Chimcast: a lightweight streaming and recording app for Windows.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod dialog;
mod logger;
#[cfg(windows)]
mod painter;
#[cfg(windows)]
mod ui;

fn main() {
    logger::init();
    #[cfg(windows)]
    if let Err(e) = app::run() {
        log::error!("{e}");
        app::fatal(&e.to_string());
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        eprintln!("Chimcast uses Windows capture and encoding APIs and only runs on Windows.");
        std::process::exit(1);
    }
}
