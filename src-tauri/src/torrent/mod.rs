//! Torrent backend abstraction and shared types.
//!
//! These types mirror the `native` TypeScript contract exactly
//! (`hayase-app/native/index.ts`), so the existing `hayase-interface` can run
//! unchanged through `bridge/native-bridge.ts`.
//!
//! The real engine lives in [`rqbit`] and is backed by `librqbit`. A stub
//! implementation remains available for tests and as a degraded fallback if
//! the engine fails to start.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

pub mod ebml;
pub mod library;
pub mod media;
pub mod rqbit;
pub mod stream_server;
pub mod subtitles;

pub use rqbit::start_rqbit;

// ---------------------------------------------------------------------------
// Settings (mirrors `ClientSettings` from `native`)
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}
fn default_speed() -> f64 {
    40.0
}
fn default_max_conns() -> usize {
    50
}
fn default_connection_budget() -> usize {
    500
}
fn default_nzb_port() -> u16 {
    119
}
fn default_nzb_pool_size() -> usize {
    5
}
fn default_remote_access() -> String {
    "off".into()
}

/// Torrent client settings. Field names/semantics match the Electron app:
/// `torrent_dht == true` means **disable** DHT, `torrent_pex == true` means
/// **disable** PeX (that is how the UI labels them).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientSettings {
    pub torrent_persist: bool,
    pub torrent_dht: bool,
    #[serde(default = "default_true")]
    pub torrent_streamed_download: bool,
    #[serde(default = "default_speed")]
    pub torrent_speed: f64,
    #[serde(default = "default_connection_budget")]
    pub connection_budget: usize,
    #[serde(default = "default_max_conns")]
    pub max_conns: usize,
    pub torrent_port: u16,
    pub dht_port: u16,
    pub torrent_pex: bool,
    pub nzb_domain: String,
    pub nzb_login: String,
    pub nzb_password: String,
    #[serde(default = "default_nzb_port")]
    pub nzb_port: u16,
    #[serde(default = "default_nzb_pool_size")]
    pub nzb_pool_size: usize,
    #[serde(default = "default_remote_access")]
    pub remote_access: String,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            torrent_persist: false,
            torrent_dht: false,
            torrent_streamed_download: true,
            torrent_speed: default_speed(),
            connection_budget: default_connection_budget(),
            max_conns: default_max_conns(),
            torrent_port: 0,
            dht_port: 0,
            torrent_pex: false,
            nzb_domain: String::new(),
            nzb_login: String::new(),
            nzb_password: String::new(),
            nzb_port: default_nzb_port(),
            nzb_pool_size: default_nzb_pool_size(),
            remote_access: default_remote_access(),
        }
    }
}

impl ClientSettings {
    /// Effective download limit in bytes/sec (0 = unlimited).
    pub fn download_bps(&self) -> u64 {
        if self.torrent_speed <= 0.0 || !self.torrent_speed.is_finite() {
            return 0;
        }
        (self.torrent_speed * 1024.0 * 1024.0 / 8.0).round() as u64
    }

    pub fn upload_bps(&self) -> u64 {
        (self.download_bps() as f64 * 1.2).round() as u64
    }
}

// ---------------------------------------------------------------------------
// Data shapes (mirror `native/index.ts`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFile {
    pub name: String,
    pub hash: String,
    #[serde(rename = "type")]
    pub file_type: String,
    pub size: u64,
    pub path: String,
    pub url: String,
    pub lan: String,
    pub id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SizeStats {
    pub total: u64,
    pub downloaded: u64,
    pub uploaded: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SpeedStats {
    pub down: u64,
    pub up: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TimeStats {
    pub remaining: u64,
    pub elapsed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PeerCounts {
    pub seeders: u64,
    pub leechers: u64,
    pub wires: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PieceStats {
    pub total: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TorrentInfo {
    pub name: String,
    pub progress: f64,
    pub size: SizeStats,
    pub speed: SpeedStats,
    pub time: TimeStats,
    pub peers: PeerCounts,
    pub pieces: PieceStats,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PeerInfo {
    pub ip: String,
    pub seeder: bool,
    pub client: String,
    pub progress: f64,
    pub size: SizeStats,
    pub speed: SpeedStats,
    pub flags: Vec<String>,
    pub time: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub progress: f64,
    pub selections: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LibraryEntry {
    pub media_id: i64,
    pub episode: i64,
    pub files: usize,
    pub hash: String,
    pub progress: f64,
    pub date: i64,
    pub size: u64,
    pub name: String,
    /// Internal: whether this was a background download. Not sent to the UI contract,
    /// but harmless extra data (serde ignores unknown fields on the TS side).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackerStats {
    pub complete: u64,
    pub downloaded: u64,
    pub incomplete: u64,
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProtocolStatus {
    pub dht: bool,
    pub lsd: bool,
    pub pex: bool,
    pub nat: bool,
    pub forwarding: bool,
    pub persisting: bool,
    pub streaming: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Attachment {
    pub filename: String,
    pub mimetype: String,
    pub id: usize,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubtitleTrack {
    pub number: i64,
    pub language: String,
    #[serde(rename = "type")]
    pub track_type: String,
    #[serde(default, rename = "_compressed")]
    pub compressed: bool,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub forced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Chapter {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// A single embedded subtitle cue, in JASSUB/libass units (milliseconds).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SubtitleCue {
    #[serde(rename = "trackNumber")]
    pub track_number: i64,
    pub text: String,
    /// Cue start in milliseconds.
    pub time: f64,
    /// Cue duration in milliseconds.
    pub duration: f64,
}

/// Cached cues plus a live feed for a torrent file.
pub struct SubtitleSubscription {
    pub cues: Vec<SubtitleCue>,
    pub receiver: tokio::sync::broadcast::Receiver<SubtitleCue>,
}

/// Shape returned by `updatePeerCounts` in the native contract.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PeerCountScrape {
    pub hash: String,
    pub complete: String,
    pub downloaded: String,
    pub incomplete: String,
}

/// A torrent identifier supplied by the UI: magnet/hash/URL string or `.torrent` bytes.
#[derive(Debug, Clone)]
pub enum TorrentSource {
    Url(String),
    Bytes(Vec<u8>),
}

impl TorrentSource {
    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        match value {
            serde_json::Value::String(s) => {
                let s = s.trim();
                if s.is_empty() {
                    return Err("Empty torrent identifier".into());
                }
                // Accept bare infohashes for parity with original parse-torrent/remote behavior.
                let is_hash = (s.len() == 40 || s.len() == 64)
                    && s.bytes().all(|b| b.is_ascii_hexdigit());
                if is_hash {
                    Ok(Self::Url(format!("magnet:?xt=urn:btih:{s}")))
                } else {
                    Ok(Self::Url(s.to_string()))
                }
            }
            serde_json::Value::Array(items) => {
                let mut bytes = Vec::with_capacity(items.len());
                for item in items {
                    let n = item
                        .as_u64()
                        .ok_or_else(|| "Torrent bytes must be an array of numbers".to_string())?;
                    if n > 255 {
                        return Err("Torrent bytes out of range".into());
                    }
                    bytes.push(n as u8);
                }
                if bytes.is_empty() {
                    return Err("Empty torrent file".into());
                }
                Ok(Self::Bytes(bytes))
            }
            _ => Err("Unsupported torrent identifier".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait TorrentBackend: Send + Sync {
    async fn add_torrent(
        &self,
        source: TorrentSource,
        media_id: i64,
        episode: i64,
        background: bool,
    ) -> Result<Vec<TorrentFile>, String>;

    async fn active_torrents(&self) -> Result<Vec<TorrentInfo>, String>;
    async fn torrent_info(&self, hash: &str) -> Result<TorrentInfo, String>;
    async fn file_info(&self, hash: &str) -> Result<Vec<FileInfo>, String>;
    async fn peer_info(&self, hash: &str) -> Result<Vec<PeerInfo>, String>;
    async fn trackers(&self, hash: &str) -> Result<BTreeMap<String, TrackerStats>, String>;
    async fn protocol_status(&self, hash: &str) -> Result<ProtocolStatus, String>;

    async fn library(&self) -> Result<Vec<LibraryEntry>, String>;
    async fn cached_torrents(&self) -> Result<Vec<String>, String>;
    async fn rescan_torrents(&self, hashes: &[String]) -> Result<(), String>;
    async fn delete_torrents(&self, hashes: &[String]) -> Result<(), String>;
    async fn remove_background_torrents(&self, hashes: &[String]) -> Result<(), String>;

    async fn check_available_space(&self, path: Option<&str>) -> Result<u64, String>;
    async fn check_incoming_connections(&self, port: u16) -> Result<bool, String>;

    async fn create_nzb(&self, hash: &str, url: &str) -> Result<(), String>;
    async fn create_http_webseed(
        &self,
        hash: &str,
        url: &str,
        authorization: Option<&str>,
        file_index: Option<usize>,
        rate_limit: Option<u64>,
    ) -> Result<(), String>;

    async fn update_settings(&self, settings: &ClientSettings) -> Result<(), String>;

    /// Change the download directory used for future torrents.
    async fn set_download_dir(&self, path: &str) -> Result<(), String>;

    /// Best-effort on-disk path for a torrent file, if known/available.
    async fn file_disk_path(&self, hash: &str, id: usize) -> Result<Option<std::path::PathBuf>, String>;

    async fn attachments(&self, hash: &str, id: usize) -> Result<Vec<Attachment>, String>;
    async fn tracks(&self, hash: &str, id: usize) -> Result<Vec<SubtitleTrack>, String>;
    async fn chapters(&self, hash: &str, id: usize) -> Result<Vec<Chapter>, String>;
    /// Cached embedded-subtitle cues plus a live feed. Starts the tap extractor
    /// if it is not already running.
    async fn subtitle_stream(&self, hash: &str, id: usize) -> Result<SubtitleSubscription, String>;
    async fn update_peer_counts(&self, hashes: &[String]) -> Result<Vec<PeerCountScrape>, String>;
}

// ---------------------------------------------------------------------------
// State + degraded stub
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TorrentState(Arc<dyn TorrentBackend>);

impl TorrentState {
    pub fn new(backend: Arc<dyn TorrentBackend>) -> Self {
        Self(backend)
    }

    pub fn backend(&self) -> Arc<dyn TorrentBackend> {
        Arc::clone(&self.0)
    }
}

impl Default for TorrentState {
    fn default() -> Self {
        Self::new(Arc::new(StubTorrentBackend))
    }
}

/// Degraded backend used only when the real engine cannot start (or in tests).
/// It keeps the app launchable and returns honest empty/error results.
#[derive(Debug, Default)]
pub struct StubTorrentBackend;

#[async_trait]
impl TorrentBackend for StubTorrentBackend {
    async fn add_torrent(
        &self,
        _source: TorrentSource,
        _media_id: i64,
        _episode: i64,
        _background: bool,
    ) -> Result<Vec<TorrentFile>, String> {
        Err("Torrent engine unavailable: failed to initialize the Rust torrent backend".into())
    }

    async fn active_torrents(&self) -> Result<Vec<TorrentInfo>, String> {
        Ok(vec![])
    }

    async fn torrent_info(&self, hash: &str) -> Result<TorrentInfo, String> {
        Err(format!("Torrent not found: {hash}"))
    }

    async fn file_info(&self, _hash: &str) -> Result<Vec<FileInfo>, String> {
        Ok(vec![])
    }

    async fn peer_info(&self, _hash: &str) -> Result<Vec<PeerInfo>, String> {
        Ok(vec![])
    }

    async fn trackers(&self, _hash: &str) -> Result<BTreeMap<String, TrackerStats>, String> {
        Ok(BTreeMap::new())
    }

    async fn protocol_status(&self, _hash: &str) -> Result<ProtocolStatus, String> {
        Ok(ProtocolStatus::default())
    }

    async fn library(&self) -> Result<Vec<LibraryEntry>, String> {
        Ok(vec![])
    }

    async fn cached_torrents(&self) -> Result<Vec<String>, String> {
        Ok(vec![])
    }

    async fn rescan_torrents(&self, _hashes: &[String]) -> Result<(), String> {
        Ok(())
    }

    async fn delete_torrents(&self, _hashes: &[String]) -> Result<(), String> {
        Ok(())
    }

    async fn remove_background_torrents(&self, _hashes: &[String]) -> Result<(), String> {
        Ok(())
    }

    async fn check_available_space(&self, _path: Option<&str>) -> Result<u64, String> {
        Ok(0)
    }

    async fn check_incoming_connections(&self, _port: u16) -> Result<bool, String> {
        Ok(false)
    }

    async fn create_nzb(&self, _hash: &str, _url: &str) -> Result<(), String> {
        Err("NZB support is not implemented in the Tauri backend yet".into())
    }

    async fn create_http_webseed(
        &self,
        _hash: &str,
        _url: &str,
        _authorization: Option<&str>,
        _file_index: Option<usize>,
        _rate_limit: Option<u64>,
    ) -> Result<(), String> {
        Err("HTTP webseed support is not implemented in the Tauri backend yet".into())
    }

    async fn update_settings(&self, _settings: &ClientSettings) -> Result<(), String> {
        Ok(())
    }

    async fn set_download_dir(&self, _path: &str) -> Result<(), String> {
        Ok(())
    }

    async fn file_disk_path(&self, _hash: &str, _id: usize) -> Result<Option<std::path::PathBuf>, String> {
        Ok(None)
    }

    async fn attachments(&self, _hash: &str, _id: usize) -> Result<Vec<Attachment>, String> {
        Ok(vec![])
    }

    async fn tracks(&self, _hash: &str, _id: usize) -> Result<Vec<SubtitleTrack>, String> {
        Ok(vec![])
    }

    async fn chapters(&self, _hash: &str, _id: usize) -> Result<Vec<Chapter>, String> {
        Ok(vec![])
    }

    async fn subtitle_stream(&self, _hash: &str, _id: usize) -> Result<SubtitleSubscription, String> {
        let (_tx, rx) = tokio::sync::broadcast::channel(1);
        Ok(SubtitleSubscription {
            cues: Vec::new(),
            receiver: rx,
        })
    }

    async fn update_peer_counts(&self, hashes: &[String]) -> Result<Vec<PeerCountScrape>, String> {
        Ok(hashes
            .iter()
            .map(|h| PeerCountScrape {
                hash: h.clone(),
                complete: "0".into(),
                downloaded: "0".into(),
                incomplete: "0".into(),
            })
            .collect())
    }
}
