//! Self-update from the newest GitHub release.
//!
//! CI publishes every build of `main` as a release tagged `build-<n>` with two
//! assets: `chimcast.exe` and `chimcast.exe.sha256`. On launch the app asks
//! GitHub for the latest release; if its build number is higher than the one
//! compiled in, the new exe is downloaded next to the running one, checked
//! against its SHA-256, and swapped in (Windows lets a running exe be renamed,
//! not overwritten). The app then restarts into it, or the next launch does.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};

const EXE_ASSET: &str = "chimcast.exe";
const HASH_ASSET: &str = "chimcast.exe.sha256";
const MAX_EXE: usize = 200 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    /// Not checking: a development build, or turned off in settings.
    Off,
    Checking,
    Downloading {
        build: u64,
    },
    UpToDate,
    /// The new exe is in place; it runs on the next start.
    Ready {
        build: u64,
    },
    Failed(String),
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub struct Updater {
    state: Arc<Mutex<UpdateState>>,
    exe: Option<PathBuf>,
}

impl Updater {
    /// Starts a background check against `repo` (`owner/name`). `build` is this
    /// binary's build number; 0 means a local build, which never updates.
    pub fn start(repo: &str, build: u64, enabled: bool) -> Self {
        let exe = std::env::current_exe().ok();
        if let Some(exe) = &exe {
            remove_old(exe);
        }
        let state = Arc::new(Mutex::new(UpdateState::Off));
        let (Some(path), true, true) = (exe.clone(), enabled, build > 0) else {
            return Self { state, exe };
        };
        *state.lock().unwrap() = UpdateState::Checking;
        let shared = state.clone();
        let repo = repo.to_string();
        let spawned = std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || {
                let result = run(&repo, build, &path, &shared);
                let next = match result {
                    Ok(Some(b)) => UpdateState::Ready { build: b },
                    Ok(None) => UpdateState::UpToDate,
                    Err(e) => {
                        log::warn!("update check failed: {e}");
                        UpdateState::Failed(e)
                    }
                };
                log::info!("updater: {next:?}");
                *shared.lock().unwrap() = next;
            });
        if spawned.is_err() {
            *state.lock().unwrap() = UpdateState::Failed("couldn't start the updater".into());
        }
        Self { state, exe }
    }

    pub fn state(&self) -> UpdateState {
        self.state.lock().unwrap().clone()
    }

    /// Starts the (already swapped-in) new exe. The caller should exit right after.
    pub fn relaunch(&self) -> std::io::Result<()> {
        let exe = self
            .exe
            .as_ref()
            .ok_or_else(|| std::io::Error::other("unknown exe path"))?;
        std::process::Command::new(exe).spawn().map(|_| ())
    }
}

/// Parses `build-<n>`.
fn build_number(tag: &str) -> Option<u64> {
    tag.strip_prefix("build-")?.parse().ok()
}

fn run(
    repo: &str,
    build: u64,
    exe: &Path,
    state: &Mutex<UpdateState>,
) -> Result<Option<u64>, String> {
    let agent = format!("Chimcast/{build}");
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let json = crate::http::get(&url, &agent, "application/vnd.github+json", 1024 * 1024)
        .map_err(|e| format!("couldn't reach GitHub: {}", e.message()))?;
    let release: Release =
        serde_json::from_slice(&json).map_err(|e| format!("unexpected reply from GitHub: {e}"))?;
    let Some(latest) = build_number(&release.tag_name) else {
        return Err(format!("unexpected release tag {}", release.tag_name));
    };
    if latest <= build {
        return Ok(None);
    }
    let asset = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| format!("release {} has no {name}", release.tag_name))
    };
    let (exe_url, hash_url) = (asset(EXE_ASSET)?, asset(HASH_ASSET)?);
    *state.lock().unwrap() = UpdateState::Downloading { build: latest };
    log::info!("downloading build {latest} (running {build})");

    let download = |url: &str, limit| {
        crate::http::get(url, &agent, "application/octet-stream", limit)
            .map_err(|e| format!("download failed: {}", e.message()))
    };
    let hash_file = download(&hash_url, 4096)?;
    let expected = String::from_utf8_lossy(&hash_file)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let bytes = download(&exe_url, MAX_EXE)?;
    let actual = sha256_hex(&bytes)?;
    if expected != actual {
        return Err(format!(
            "checksum mismatch for build {latest}; not installing it"
        ));
    }
    install(exe, &bytes).map_err(|e| format!("couldn't install the update: {e}"))?;
    Ok(Some(latest))
}

fn sha256_hex(data: &[u8]) -> Result<String, String> {
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    if status.is_err() {
        return Err(format!("SHA-256 failed ({:#x})", status.0));
    }
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

fn with_suffix(exe: &Path, suffix: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

/// Puts `bytes` in place of the running exe, keeping the old one as `.old`.
fn install(exe: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let new = with_suffix(exe, ".new");
    let old = with_suffix(exe, ".old");
    std::fs::write(&new, bytes)?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(exe, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&new, exe) {
        // Put the running version back so the next launch still works.
        let _ = std::fs::rename(&old, exe);
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    Ok(())
}

/// Deletes the exe left behind by the previous update. The old process may
/// still be shutting down, so this retries for a few seconds in the background.
fn remove_old(exe: &Path) {
    let old = with_suffix(exe, ".old");
    if !old.exists() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("updater-cleanup".into())
        .spawn(move || {
            for _ in 0..20 {
                if std::fs::remove_file(&old).is_ok() || !old.exists() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags() {
        assert_eq!(build_number("build-42"), Some(42));
        assert_eq!(build_number("v1.0"), None);
    }
}
