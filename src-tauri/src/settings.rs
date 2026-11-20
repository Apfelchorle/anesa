//! Persistent settings with atomic writes and migration from Electron's `settings.json`.
//!
//! Electron reference: `hayase-electron/src/main/store.ts` (sync read, unqueued writes).
//! This implementation:
//! - Loads once at startup (blocking acceptable for small JSON) but tolerates corruption.
//! - Writes atomically via temp file + rename.
//! - Serializes writes through a mutex to avoid lost updates/races.
//! - Stores under Tauri's app-config dir (`settings.json`), with best-effort import of
//!   the legacy Electron `userData/settings.json` when present.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::torrent::ClientSettings;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    #[serde(default)]
    pub player_path: Option<String>,
    #[serde(default)]
    pub torrent_path: Option<String>,
    #[serde(default)]
    pub hide_to_tray: bool,
    #[serde(default)]
    pub angle: Option<String>,
    #[serde(default)]
    pub doh: Option<String>,
    #[serde(default = "default_zoom")]
    pub zoom: f64,
    #[serde(default)]
    pub torrent_settings: ClientSettings,
    /// Persisted crash history for renderer-crash loop protection (Electron reset this per launch).
    #[serde(default)]
    pub crash_timestamps: Vec<i64>,
}

fn default_zoom() -> f64 {
    1.0
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            player_path: None,
            torrent_path: None,
            hide_to_tray: false,
            angle: None,
            doh: None,
            zoom: 1.0,
            torrent_settings: ClientSettings::default(),
            crash_timestamps: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct SettingsState {
    path: PathBuf,
    inner: Mutex<AppSettings>,
}

impl SettingsState {
    pub fn load(path: PathBuf) -> Self {
        let data = load_settings(&path).unwrap_or_else(|e| {
            log::warn!("Failed to load settings from {}: {e}", path.display());
            AppSettings::default()
        });
        Self { path, inner: Mutex::new(data) }
    }

    pub fn get(&self) -> AppSettings {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Mutate and persist atomically. Returns the new settings snapshot.
    pub fn update(&self, f: impl FnOnce(&mut AppSettings)) -> AppSettings {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard);
        let snapshot = guard.clone();
        if let Err(e) = persist_settings(&self.path, &snapshot) {
            log::error!("Failed to persist settings to {}: {e}", self.path.display());
        }
        snapshot
    }

    /// Record a crash timestamp, pruning entries older than 10 minutes.
    pub fn record_crash(&self, now_ms: i64) -> usize {
        let snapshot = self.update(|s| {
            let cutoff = now_ms - 10 * 60 * 1000;
            s.crash_timestamps.retain(|t| *t >= cutoff);
            s.crash_timestamps.push(now_ms);
        });
        snapshot.crash_timestamps.len()
    }

    pub fn clear_crashes(&self) {
        self.update(|s| s.crash_timestamps.clear());
    }
}

fn load_settings(path: &Path) -> Result<AppSettings, String> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AppSettings::default()),
        Err(e) => Err(e.to_string()),
    }
}

/// Atomic write: write temp file in same directory, fsync, then rename over target.
fn persist_settings(path: &Path, settings: &AppSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;

    let tmp_path = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
        file.write_all(&json).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp_path, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Best-effort migration from the Electron app's `settings.json` layout.
/// The UI may also send settings via `update_settings`; this only seeds defaults.
pub fn migrate_from_electron(electron_settings_path: &Path, target: &mut AppSettings) {
    let Ok(raw) = fs::read_to_string(electron_settings_path) else { return };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else { return };

    if let Some(v) = value.get("player").and_then(|v| v.as_str()) {
        target.player_path = Some(v.to_string());
    }
    if let Some(v) = value.get("torrentPath").and_then(|v| v.as_str()) {
        target.torrent_path = Some(v.to_string());
    }
    if let Some(v) = value.get("doh").and_then(|v| v.as_str()) {
        target.doh = Some(v.to_string());
    }
    if let Some(v) = value.get("angle").and_then(|v| v.as_str()) {
        target.angle = Some(v.to_string());
    }
    if let Some(v) = value.get("torrentSettings") {
        if let Ok(parsed) = serde_json::from_value::<ClientSettings>(v.clone()) {
            target.torrent_settings = parsed;
        }
    }
    log::info!("Imported legacy Electron settings from {}", electron_settings_path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_atomic_write() {
        let dir = std::env::temp_dir().join(format!("hayase-settings-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        let state = SettingsState::load(path.clone());
        let updated = state.update(|s| {
            s.player_path = Some("/usr/bin/mpv".into());
            s.zoom = 1.25;
        });
        assert_eq!(updated.player_path.as_deref(), Some("/usr/bin/mpv"));

        let reloaded = SettingsState::load(path);
        assert_eq!(reloaded.get().zoom, 1.25);
        let _ = std::fs::remove_dir_all(dir);
    }
}
