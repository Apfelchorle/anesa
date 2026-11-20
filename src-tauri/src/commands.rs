//! Tauri command surface.
//!
//! Names and argument shapes are consumed by `bridge/native-bridge.ts`, which
//! mirrors the `native` TypeScript contract used by `hayase-interface`.
//! Security-sensitive arguments are validated here before touching the engine.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, Runtime, State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::events::{EVENT_ERROR, EVENT_TORRENT_SUBTITLE, EVENT_UPDATE_PROGRESS};
use crate::player::PlayerState;
use crate::security::{
    resolve_macos_app, validate_download_dir, validate_external_url, validate_infohash,
    validate_player_executable, validate_player_url, validate_remote_fetch_url,
    validate_torrent_identifier,
};
use crate::settings::{AppSettings, SettingsState};
use crate::torrent::{
    Attachment, Chapter, ClientSettings, FileInfo, LibraryEntry, PeerCountScrape, PeerInfo,
    ProtocolStatus, SubtitleTrack, TorrentFile, TorrentInfo, TorrentSource, TorrentState,
    TrackerStats,
};

// --- App / window / shell ---

#[tauri::command]
pub fn version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    let validated = validate_external_url(&url)?;
    app.opener()
        .open_url(validated.as_str(), None::<String>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn window_minimise(window: WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn window_maximise(window: WebviewWindow) -> Result<(), String> {
    if window.is_maximized().map_err(|e| e.to_string())? {
        window.unmaximize().map_err(|e| e.to_string())
    } else {
        window.maximize().map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub fn window_fullscreen(window: WebviewWindow, enabled: bool) -> Result<(), String> {
    window.set_fullscreen(enabled).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn window_close(window: WebviewWindow, settings: State<'_, SettingsState>) -> Result<(), String> {
    if settings.get().hide_to_tray {
        window.hide().map_err(|e| e.to_string())
    } else {
        window.close().map_err(|e| e.to_string())
    }
}

#[tauri::command]
pub fn window_focus(window: WebviewWindow) -> Result<(), String> {
    if window.is_minimized().map_err(|e| e.to_string())? {
        window.unminimize().map_err(|e| e.to_string())?;
    }
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn app_restart(app: AppHandle) {
    app.restart();
}

#[tauri::command]
pub fn app_quit(app: AppHandle, player: State<'_, PlayerState>) {
    player.kill();
    app.exit(0);
}

#[tauri::command]
pub fn set_zoom(
    window: WebviewWindow,
    scale: f64,
    settings: State<'_, SettingsState>,
) -> Result<AppSettings, String> {
    let clamped = if scale.is_finite() {
        scale.clamp(0.3, 2.5)
    } else {
        1.0
    };
    window.set_zoom(clamped).map_err(|e| e.to_string())?;
    Ok(settings.update(|s| s.zoom = clamped))
}

#[tauri::command]
pub fn set_hide_to_tray(settings: State<'_, SettingsState>, enabled: bool) -> AppSettings {
    settings.update(|s| s.hide_to_tray = enabled)
}

#[tauri::command]
pub async fn select_player<R: Runtime>(
    app: AppHandle<R>,
    settings: State<'_, SettingsState>,
) -> Result<Option<String>, String> {
    let Some(file) = app.dialog().file().blocking_pick_file() else {
        return Ok(settings.get().player_path);
    };
    let path = file.into_path().map_err(|e| e.to_string())?;
    let validated = validate_player_executable(path.to_string_lossy().as_ref())?;
    let path_str = validated.to_string_lossy().into_owned();
    settings.update(|s| s.player_path = Some(path_str.clone()));

    // Match Electron behavior: return a display-friendly basename on selection.
    let display = Path::new(&path_str)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or(path_str);
    Ok(Some(display))
}

#[tauri::command]
pub async fn select_download<R: Runtime>(
    app: AppHandle<R>,
    settings: State<'_, SettingsState>,
    torrent: State<'_, TorrentState>,
    _kind: Option<String>,
) -> Result<Option<String>, String> {
    let Some(folder) = app.dialog().file().blocking_pick_folder() else {
        return Ok(settings.get().torrent_path);
    };
    let path = folder.into_path().map_err(|e| e.to_string())?;
    let validated = validate_download_dir(path.to_string_lossy().as_ref())?;
    let mut path_str = validated.to_string_lossy().into_owned();
    if !path_str.ends_with(std::path::MAIN_SEPARATOR) {
        path_str.push(std::path::MAIN_SEPARATOR);
    }
    settings.update(|s| s.torrent_path = Some(path_str.clone()));
    torrent.backend().set_download_dir(&path_str).await?;
    Ok(Some(path_str))
}

/// Spawn an external player for a localhost streaming URL.
/// Hardened vs Electron: URL parsed/validated, executable validated, previous
/// player killed, no shell, and child-process cleanup on exit.
#[tauri::command]
pub async fn spawn_player(
    settings: State<'_, SettingsState>,
    player: State<'_, PlayerState>,
    url: String,
) -> Result<(), String> {
    let player_url = validate_player_url(&url)?;
    let stored = settings
        .get()
        .player_path
        .ok_or_else(|| "No player selected".to_string())?;
    let exe = validate_player_executable(&stored)?;
    let exe = resolve_macos_app(&exe);

    #[cfg(target_os = "linux")]
    if std::env::var_os("FLATPAK_ID").is_some() {
        use std::process::Command;
        let exe_str = exe.to_string_lossy().into_owned();
        player.kill();
        let status = Command::new("flatpak-spawn")
            .args(["--host", &exe_str, player_url.as_str()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("External player exited with an error".into());
        }
        return Ok(());
    }

    player.spawn(&exe, player_url.as_str())
}

#[tauri::command]
pub fn open_ui_devtools(window: WebviewWindow) {
    window.open_devtools();
}

#[tauri::command]
pub fn open_torrent_devtools(app: AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        window.open_devtools();
    }
}

// --- Settings ---

#[tauri::command]
pub fn get_settings(settings: State<'_, SettingsState>) -> AppSettings {
    settings.get()
}

#[tauri::command]
pub async fn update_settings(
    state: State<'_, SettingsState>,
    torrent: State<'_, TorrentState>,
    settings: ClientSettings,
) -> Result<AppSettings, String> {
    let updated = state.update(|s| s.torrent_settings = settings.clone());
    torrent.backend().update_settings(&settings).await?;
    Ok(updated)
}

#[tauri::command]
pub fn set_doh(settings: State<'_, SettingsState>, dns: String) -> AppSettings {
    // Persisted for the Rust networking layer; DoH resolution is not wired yet.
    settings.update(|s| s.doh = Some(dns))
}

#[tauri::command]
pub fn set_angle(app: AppHandle, settings: State<'_, SettingsState>, angle: String) -> AppSettings {
    let current = settings.get().angle;
    let updated = settings.update(|s| s.angle = Some(angle));
    if current != updated.angle {
        // Matches Electron: graphics backend changes require a restart.
        app.restart();
    }
    updated
}

// --- Device / logs / misc ---

#[tauri::command]
pub async fn get_logs(app: AppHandle) -> Result<String, String> {
    let log_dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    let candidates = ["hayase.log", "Hayase.log"];
    for name in candidates {
        let path = log_dir.join(name);
        if path.exists() {
            return tail_file(&path, 512 * 1024);
        }
    }
    Ok(String::new())
}

fn tail_file(path: &std::path::Path, max_bytes: u64) -> Result<String, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub features: Value,
    pub info: Value,
    pub cpu: CpuInfo,
    pub ram: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfo {
    pub model: String,
    pub speed: u64,
}

#[tauri::command]
pub async fn get_device_info() -> Result<DeviceInfo, String> {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();
    let cpu = sys.cpus().first();
    Ok(DeviceInfo {
        // GPU feature status/info is not available cross-platform without extra
        // platform-specific code; preserve the shape for the UI.
        features: serde_json::json!({}),
        info: Value::Null,
        cpu: CpuInfo {
            model: cpu.map(|c| c.brand().to_string()).unwrap_or_default(),
            speed: cpu.map(|c| c.frequency()).unwrap_or(0),
        },
        ram: sys.total_memory(),
    })
}

#[tauri::command]
pub fn default_transparency() -> bool {
    false
}

#[tauri::command]
pub fn accent_color() -> String {
    // TODO: query the OS accent color (Windows registry / macOS NSColor / portal).
    "#ffffff".into()
}

// --- Update checks (plugin scaffolded; wire to tauri-plugin-updater in a later pass) ---

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<bool, String> {
    let _ = app.emit(EVENT_UPDATE_PROGRESS, 0.0_f64);
    Ok(false)
}

#[tauri::command]
pub async fn update_ready(app: AppHandle) -> Result<(), String> {
    let _ = app.emit(EVENT_UPDATE_PROGRESS, 100.0_f64);
    Ok(())
}

#[tauri::command]
pub fn update_and_restart(app: AppHandle) {
    let _ = app.emit(EVENT_UPDATE_PROGRESS, 100.0_f64);
    app.restart();
}

#[allow(dead_code)]
pub fn emit_error<R: Runtime>(app: &AppHandle<R>, source: &str, message: &str) {
    let _ = app.emit(
        EVENT_ERROR,
        serde_json::json!({ "source": source, "message": message }),
    );
}

// --- Torrent backend ---

#[tauri::command]
pub async fn add_torrent(
    torrent: State<'_, TorrentState>,
    id: Value,
    media_id: i64,
    episode: i64,
    background: bool,
) -> Result<Vec<TorrentFile>, String> {
    let source = TorrentSource::from_json(&id)?;
    if let TorrentSource::Url(url) = &source {
        if url.starts_with("magnet:?") {
            validate_torrent_identifier(url)?;
        } else if url.starts_with("http://") || url.starts_with("https://") {
            validate_remote_fetch_url(url)?;
        } else {
            validate_torrent_identifier(url)?;
        }
    }
    torrent
        .backend()
        .add_torrent(source, media_id, episode, background)
        .await
}

#[tauri::command]
pub async fn torrent_active(torrent: State<'_, TorrentState>) -> Result<Vec<TorrentInfo>, String> {
    torrent.backend().active_torrents().await
}

#[tauri::command]
pub async fn torrent_info(
    torrent: State<'_, TorrentState>,
    hash: String,
) -> Result<TorrentInfo, String> {
    validate_infohash(&hash)?;
    torrent.backend().torrent_info(&hash).await
}

#[tauri::command]
pub async fn torrent_file_info(
    torrent: State<'_, TorrentState>,
    hash: String,
) -> Result<Vec<FileInfo>, String> {
    validate_infohash(&hash)?;
    torrent.backend().file_info(&hash).await
}

#[tauri::command]
pub async fn torrent_peer_info(
    torrent: State<'_, TorrentState>,
    hash: String,
) -> Result<Vec<PeerInfo>, String> {
    validate_infohash(&hash)?;
    torrent.backend().peer_info(&hash).await
}

#[tauri::command]
pub async fn torrent_trackers(
    torrent: State<'_, TorrentState>,
    hash: String,
) -> Result<std::collections::BTreeMap<String, TrackerStats>, String> {
    validate_infohash(&hash)?;
    torrent.backend().trackers(&hash).await
}

#[tauri::command]
pub async fn torrent_protocol_status(
    torrent: State<'_, TorrentState>,
    hash: String,
) -> Result<ProtocolStatus, String> {
    validate_infohash(&hash)?;
    torrent.backend().protocol_status(&hash).await
}

#[tauri::command]
pub async fn torrent_library(
    torrent: State<'_, TorrentState>,
) -> Result<Vec<LibraryEntry>, String> {
    torrent.backend().library().await
}

#[tauri::command]
pub async fn torrent_cached(torrent: State<'_, TorrentState>) -> Result<Vec<String>, String> {
    torrent.backend().cached_torrents().await
}

#[tauri::command]
pub async fn torrent_rescan(
    torrent: State<'_, TorrentState>,
    hashes: Vec<String>,
) -> Result<(), String> {
    for hash in &hashes {
        validate_infohash(hash)?;
    }
    torrent.backend().rescan_torrents(&hashes).await
}

#[tauri::command]
pub async fn torrent_delete(
    torrent: State<'_, TorrentState>,
    hashes: Vec<String>,
) -> Result<(), String> {
    for hash in &hashes {
        validate_infohash(hash)?;
    }
    torrent.backend().delete_torrents(&hashes).await
}

#[tauri::command]
pub async fn torrent_remove_background(
    torrent: State<'_, TorrentState>,
    hashes: Vec<String>,
) -> Result<(), String> {
    for hash in &hashes {
        validate_infohash(hash)?;
    }
    torrent.backend().remove_background_torrents(&hashes).await
}

#[tauri::command]
pub async fn torrent_check_available_space(
    torrent: State<'_, TorrentState>,
    path: Option<String>,
) -> Result<u64, String> {
    if let Some(ref p) = path {
        validate_download_dir(p)?;
    }
    torrent.backend().check_available_space(path.as_deref()).await
}

#[tauri::command]
pub async fn torrent_check_incoming_connections(
    torrent: State<'_, TorrentState>,
    port: u16,
) -> Result<bool, String> {
    torrent.backend().check_incoming_connections(port).await
}

#[tauri::command]
pub async fn torrent_create_nzb(
    torrent: State<'_, TorrentState>,
    hash: String,
    url: String,
) -> Result<(), String> {
    validate_infohash(&hash)?;
    validate_remote_fetch_url(&url)?;
    torrent.backend().create_nzb(&hash, &url).await
}

#[tauri::command]
pub async fn torrent_create_http_webseed(
    torrent: State<'_, TorrentState>,
    hash: String,
    url: String,
    authorization: Option<String>,
    file_index: Option<usize>,
    rate_limit: Option<u64>,
) -> Result<(), String> {
    validate_infohash(&hash)?;
    validate_remote_fetch_url(&url)?;
    torrent
        .backend()
        .create_http_webseed(
            &hash,
            &url,
            authorization.as_deref(),
            file_index,
            rate_limit,
        )
        .await
}

// --- Media pipeline (EBML extraction not fully ported yet) ---

#[tauri::command]
pub async fn torrent_attachments(
    torrent: State<'_, TorrentState>,
    hash: String,
    id: usize,
) -> Result<Vec<Attachment>, String> {
    validate_infohash(&hash)?;
    torrent.backend().attachments(&hash, id).await
}

#[tauri::command]
pub async fn torrent_tracks(
    torrent: State<'_, TorrentState>,
    hash: String,
    id: usize,
) -> Result<Vec<SubtitleTrack>, String> {
    validate_infohash(&hash)?;
    torrent.backend().tracks(&hash, id).await
}

#[tauri::command]
pub async fn torrent_chapters(
    torrent: State<'_, TorrentState>,
    hash: String,
    id: usize,
) -> Result<Vec<Chapter>, String> {
    validate_infohash(&hash)?;
    torrent.backend().chapters(&hash, id).await
}

#[derive(Debug, Clone, Serialize)]
struct SubtitleEventPayload {
    hash: String,
    id: usize,
    subtitle: SubtitleEvent,
    #[serde(rename = "trackNumber")]
    track_number: i64,
}

#[derive(Debug, Clone, Serialize)]
struct SubtitleEvent {
    text: String,
    time: f64,
    duration: f64,
}

/// Stream embedded subtitles as `torrent://subtitle` events.
///
/// Best-effort MKV subtitle cue extraction: parses subtitle blocks from the
/// on-disk media file (when available) and emits cues to the frontend, which
/// feeds them into Jassub. This enables embedded tracks (e.g. ASS/SRT-in-MKV)
/// that VLC can display but the web player otherwise can't access.
#[tauri::command]
pub async fn torrent_subtitles<R: Runtime>(
    app: AppHandle<R>,
    torrent: State<'_, TorrentState>,
    hash: String,
    id: usize,
) -> Result<(), String> {
    validate_infohash(&hash)?;

    let backend = torrent.backend();
    let path = match backend.file_disk_path(&hash, id).await {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(()),
        Err(e) => {
            log::debug!("torrent_subtitles: could not resolve file path for {hash}/{id}: {e}");
            return Ok(());
        }
    };

    // Spawn extraction so the command returns promptly (frontend awaits it but also
    // listens for streaming `torrent://subtitle` events).
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = emit_embedded_subtitles(&app, &hash, id, &path) {
            log::debug!("torrent_subtitles: extraction failed for {hash}/{id}: {e}");
        }
    });

    Ok(())
}

fn emit_embedded_subtitles<R: Runtime>(
    app: &AppHandle<R>,
    hash: &str,
    id: usize,
    path: &Path,
) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("Media file not found on disk: {}", path.display()));
    }

    let file = std::fs::File::open(path)
        .map_err(|e| format!("Failed to open media file {}: {e}", path.display()))?;
    let mut mkv = matroska_demuxer::MatroskaFile::open(file)
        .map_err(|e| format!("Failed to parse Matroska file: {e:?}"))?;

    let track_list: Vec<_> = mkv.tracks().iter().collect();
    log::debug!(
        "torrent_subtitles: parsed {} ({} tracks: {:?})",
        path.display(),
        track_list.len(),
        track_list
            .iter()
            .map(|t| (t.track_number().get(), format!("{:?}", t.track_type())))
            .collect::<Vec<_>>()
    );

    // Note: we deliberately do not filter frames by track number here.
    // `matroska_demuxer`'s `frame.track` is not guaranteed to equal the MKV
    // `TrackNumber` used by the `matroska` crate for the UI track list, and
    // filtering on it previously dropped every cue on affected files. We instead
    // keep only frames whose payload is valid UTF-8 (subtitle tracks carry text;
    // video/audio frames are binary).

    let mut frame = matroska_demuxer::Frame::default();
    let mut emitted: u64 = 0;
    let mut skipped_binary: u64 = 0;
    let mut skipped_empty: u64 = 0;
    loop {
        match mkv.next_frame(&mut frame) {
            Ok(true) => {
                // Subtitle tracks carry text; video/audio frames are binary and
                // must not be emitted as cues. Skip anything that isn't valid UTF-8.
                let Ok(text) = String::from_utf8(frame.data.clone()) else {
                    skipped_binary += 1;
                    continue;
                };
                if text.trim().is_empty() {
                    skipped_empty += 1;
                    continue;
                }

                // matroska-demuxer reports timestamps/durations in nanoseconds.
                let time = frame.timestamp as f64 / 1_000_000_000.0;
                let duration = frame
                    .duration
                    .map(|d| d as f64 / 1_000_000_000.0)
                    .unwrap_or(0.0);

                let payload = SubtitleEventPayload {
                    hash: hash.to_string(),
                    id,
                    subtitle: SubtitleEvent {
                        text,
                        time,
                        duration,
                    },
                    track_number: frame.track as i64,
                };

                // Best-effort emit; if the webview is gone, stop.
                if app.emit(EVENT_TORRENT_SUBTITLE, payload).is_err() {
                    log::debug!("torrent_subtitles: emit failed, stopping");
                    return Ok(());
                }
                emitted += 1;
            }
            Ok(false) => break,
            Err(e) => {
                // Partial/incomplete files are common while streaming torrents; emit what we got.
                log::debug!("torrent_subtitles: stopped reading frames for {hash}/{id}: {e:?}");
                break;
            }
        }
    }

    log::debug!(
        "torrent_subtitles: finished {hash}/{id} — emitted={emitted} skipped_binary={skipped_binary} skipped_empty={skipped_empty}"
    );
    Ok(())
}

#[tauri::command]
pub async fn torrent_update_peer_counts(
    torrent: State<'_, TorrentState>,
    hashes: Vec<String>,
) -> Result<Vec<PeerCountScrape>, String> {
    for hash in &hashes {
        validate_infohash(hash)?;
    }
    torrent.backend().update_peer_counts(&hashes).await
}

#[tauri::command]
pub fn torrent_download_progress(window: WebviewWindow, percent: f64) {
    use tauri::window::{ProgressBarState, ProgressBarStatus};
    let p = if percent.is_finite() {
        percent.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let state = if p <= 0.0 || p >= 1.0 {
        ProgressBarState {
            status: Some(ProgressBarStatus::None),
            progress: None,
        }
    } else {
        ProgressBarState {
            status: Some(ProgressBarStatus::Normal),
            progress: Some((p * 100.0).round() as u64),
        }
    };
    let _ = window.set_progress_bar(state);
}

#[tauri::command]
pub fn debug(_levels: String) {}

#[tauri::command]
pub fn profile(_seconds: u64) {}
