//! `librqbit`-backed torrent engine.
//!
//! This replaces the Node `hayase-torrent-client` utility process with an
//! in-process Rust engine. It implements the core flows used by the UI:
//! add magnet/torrent, file listing, local HTTP streaming with Range support,
//! stats/peers/trackers, library persistence, cached list, rescan/delete,
//! available-space checks and runtime speed limits.
//!
//! Not yet ported (tracked in README): NZB/HTTP webseeds, EBML subtitle/
//! attachment extraction, Chromecast/DLNA, remote host, DoH, NAT reachability.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use librqbit::api::TorrentIdOrHash;
use librqbit::http_api_types::PeerStatsFilter;
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, DhtSessionConfig, ListenerOptions, Session, SessionOptions,
    TorrentStats,
};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, SeekFrom};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use super::library::{LibraryStore, TorrentCache};
use super::stream_server;
use super::{
    Attachment, Chapter, ClientSettings, FileInfo, LibraryEntry, PeerCountScrape, PeerCounts,
    PeerInfo, PieceStats, ProtocolStatus, SizeStats, SpeedStats, SubtitleTrack, TimeStats,
    TorrentBackend, TorrentFile, TorrentInfo, TorrentSource, TrackerStats,
};

const METADATA_TIMEOUT: Duration = Duration::from_secs(120);
const RESCAN_TIMEOUT: Duration = Duration::from_secs(90);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn random_token() -> String {
    let bytes: [u8; 16] = rand::random();
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn validate_hash(hash: &str) -> Result<(), String> {
    let ok = (hash.len() == 40 || hash.len() == 64) && hash.bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err("Invalid infohash".into())
    }
}

fn local_lan_ip() -> String {
    // Best-effort local IPv4 detection without external dependencies.
    // This does not send packets; connect() picks a routing interface.
    use std::net::UdpSocket;
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        if sock.connect(("8.8.8.8", 80)).is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if let std::net::IpAddr::V4(v4) = addr.ip() {
                    if !v4.is_loopback() && !v4.is_unspecified() {
                        return v4.to_string();
                    }
                }
            }
        }
    }
    "127.0.0.1".to_string()
}

fn normalize_peer_ip(ip: &str) -> String {
    // Strip IPv4-mapped IPv6 prefix for parity with WebTorrent peer formatting.
    if let Some(stripped) = ip.strip_prefix("[::ffff:") {
        if let Some(end) = stripped.find(']') {
            return format!("{}{}", &stripped[..end], &stripped[end + 1..]);
        }
    }
    if let Some(stripped) = ip.strip_prefix("::ffff:") {
        return stripped.to_string();
    }
    ip.to_string()
}

fn download_limit(settings: &ClientSettings) -> Option<NonZeroU32> {
    let bps = settings.download_bps();
    if bps == 0 {
        None
    } else {
        NonZeroU32::new(bps.min(u32::MAX as u64) as u32)
    }
}

fn upload_limit(settings: &ClientSettings) -> Option<NonZeroU32> {
    let bps = settings.upload_bps();
    if bps == 0 {
        None
    } else {
        NonZeroU32::new(bps.min(u32::MAX as u64) as u32)
    }
}

#[derive(Clone)]
pub struct CachedAttachment {
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Default)]
struct CachedMediaMeta {
    tracks: Vec<SubtitleTrack>,
    chapters: Vec<Chapter>,
    attachment_ids: Vec<usize>,
}

pub struct RqbitBackend {
    session: Arc<Session>,
    download_dir: RwLock<PathBuf>,
    library: LibraryStore,
    cache: TorrentCache,
    stream_port: u16,
    stream_token: String,
    settings: RwLock<ClientSettings>,
    /// Extracted MKV attachments keyed by (hash, file_id, attachment_index).
    attachments: RwLock<HashMap<(String, usize, usize), CachedAttachment>>,
    /// Parsed media metadata keyed by (hash, file_id).
    media_meta: RwLock<HashMap<(String, usize), CachedMediaMeta>>,
    /// Current foreground (UI playback) torrent hash for the implicit desktop session.
    foreground_hash: RwLock<Option<String>>,
}

impl RqbitBackend {
    pub fn stream_port(&self) -> u16 {
        self.stream_port
    }

    pub fn stream_token(&self) -> &str {
        &self.stream_token
    }

    pub async fn get_attachment(
        &self,
        hash: &str,
        file_id: usize,
        att_id: usize,
    ) -> Option<CachedAttachment> {
        self.attachments
            .read()
            .await
            .get(&(hash.to_string(), file_id, att_id))
            .cloned()
    }

    pub async fn store_attachments(
        &self,
        hash: &str,
        file_id: usize,
        items: Vec<(usize, CachedAttachment)>,
    ) {
        let mut map = self.attachments.write().await;
        for (idx, att) in items {
            map.insert((hash.to_string(), file_id, idx), att);
        }
    }

    fn file_disk_path(&self, handle: &Arc<librqbit::ManagedTorrent>, file_id: usize) -> Option<PathBuf> {
        handle
            .with_metadata(|m| {
                m.file_infos.get(file_id).map(|fi| {
                    // Download dir + relative filename
                    // download_dir is stored separately; we return relative path here.
                    fi.relative_filename.clone()
                })
            })
            .ok()
            .flatten()
            .map(|rel| {
                // This is relative to output folder; caller should join with download_dir.
                rel
            })
    }

    async fn ensure_media_meta(&self, hash: &str, file_id: usize) -> Result<CachedMediaMeta, String> {
        if let Some(meta) = self.media_meta.read().await.get(&(hash.to_string(), file_id)).cloned() {
            return Ok(meta);
        }

        let handle = self.handle(hash)?;
        let rel = handle
            .with_metadata(|m| {
                m.file_infos
                    .get(file_id)
                    .map(|fi| fi.relative_filename.clone())
            })
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("File id out of range: {file_id}"))?;

        let download_dir = self.download_dir.read().await.clone();
        let disk_path = download_dir.join(&rel);

        // Best-effort: only parse if it looks like MKV/WebM and exists on disk.
        let is_matroska = rel
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                e == "mkv" || e == "webm" || e == "mka" || e == "mk3d"
            })
            .unwrap_or(false);

        let mut cached = CachedMediaMeta::default();
        if is_matroska && disk_path.exists() {
            match tokio::task::spawn_blocking(move || super::media::parse_mkv_file(&disk_path)).await {
                Ok(Ok(parsed)) => {
                    let mut att_ids = Vec::with_capacity(parsed.attachments.len());
                    {
                        let mut map = self.attachments.write().await;
                        for (idx, att) in parsed.attachments.into_iter().enumerate() {
                            map.insert(
                                (hash.to_string(), file_id, idx),
                                CachedAttachment {
                                    name: att.filename,
                                    mime: att.mimetype,
                                    data: att.data,
                                },
                            );
                            att_ids.push(idx);
                        }
                    }
                    cached = CachedMediaMeta {
                        tracks: parsed.tracks,
                        chapters: parsed.chapters,
                        attachment_ids: att_ids,
                    };
                }
                Ok(Err(e)) => {
                    log::debug!("MKV parse failed for {hash}/{file_id}: {e}");
                }
                Err(e) => {
                    log::debug!("MKV parse task failed for {hash}/{file_id}: {e}");
                }
            }
        }

        self.media_meta
            .write()
            .await
            .insert((hash.to_string(), file_id), cached.clone());
        Ok(cached)
    }

    fn handle(&self, hash: &str) -> Result<Arc<librqbit::ManagedTorrent>, String> {
        validate_hash(hash)?;
        if hash.len() == 40 {
            if let Ok(id) = TorrentIdOrHash::try_from(hash) {
                if let Some(handle) = self.session.get(id) {
                    return Ok(handle);
                }
            }
        }
        Err(format!("Torrent not found: {hash}"))
    }

    fn is_active(&self, hash: &str) -> bool {
        self.handle(hash).is_ok()
    }

    fn build_info(&self, handle: &Arc<librqbit::ManagedTorrent>, hash: &str) -> TorrentInfo {
        let stats: TorrentStats = handle.stats();
        let total = stats.total_bytes;
        let downloaded = stats.progress_bytes;
        let uploaded = stats.uploaded_bytes;

        let (down, up, seeders, leechers, wires, remaining) = if let Some(live) = &stats.live {
            let down = live.download_speed.mbps as f64;
            // librqbit exposes Speed with mbps (f64) and helper as_bytes(); prefer as_bytes() for precision.
            let down_bps = live.download_speed.as_bytes();
            let up_bps = live.upload_speed.as_bytes();
            let _ = down;
            let peers = &live.snapshot.peer_stats;
            let seeders = peers.live_seeders as u64;
            let leechers = (peers.live.saturating_sub(peers.live_seeders)) as u64;
            let wires = (peers.live + peers.connecting) as u64;
            let remaining = if down_bps > 0 {
                ((total.saturating_sub(downloaded) as f64 / down_bps as f64) * 1000.0) as u64
            } else {
                0
            };
            (down_bps, up_bps, seeders, leechers, wires, remaining)
        } else {
            (0, 0, 0, 0, 0, 0)
        };

        let (piece_size, piece_total) = handle
            .with_metadata(|m| {
                let size = m.info.info().piece_length as u64;
                let total = if size > 0 { m.info.lengths().total_pieces() as u64 } else { 0 };
                (size, total)
            })
            .unwrap_or((0, 0));

        TorrentInfo {
            name: handle.name().unwrap_or_default(),
            progress: if total > 0 {
                downloaded as f64 / total as f64
            } else {
                0.0
            },
            size: SizeStats {
                total,
                downloaded,
                uploaded,
            },
            speed: SpeedStats { down, up },
            time: TimeStats { remaining, elapsed: 0 },
            peers: PeerCounts {
                seeders,
                leechers,
                wires,
            },
            pieces: PieceStats {
                total: piece_total,
                size: piece_size,
            },
            hash: hash.to_string(),
        }
    }

    fn build_files(&self, handle: &Arc<librqbit::ManagedTorrent>, hash: &str) -> Vec<TorrentFile> {
        let port = self.stream_port;
        let token = &self.stream_token;
        let lan_ip = local_lan_ip();
        handle
            .with_metadata(|m| {
                m.file_infos
                    .iter()
                    .enumerate()
                    .map(|(id, fi)| {
                        let name = fi
                            .relative_filename
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| fi.relative_filename.to_string_lossy().into_owned());
                        let path = format!(
                            "/{}",
                            fi.relative_filename.to_string_lossy().replace('\\', "/")
                        );
                        let encoded = utf8_percent_encode(&name, NON_ALPHANUMERIC).to_string();
                        let suffix = format!("/stream/{token}/{hash}/{id}/{encoded}");
                        let url = format!("http://127.0.0.1:{port}{suffix}");
                        let lan = format!("http://{lan_ip}:{port}{suffix}");
                        TorrentFile {
                            name,
                            hash: hash.to_string(),
                            file_type: mime_guess::from_path(&fi.relative_filename)
                                .first_or_octet_stream()
                                .to_string(),
                            size: fi.len,
                            path,
                            url,
                            lan,
                            id,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Metadata for a file: display name + length. Does not open a stream.
    pub fn file_meta(&self, hash: &str, file_id: usize) -> Result<(String, u64), String> {
        let handle = self.handle(hash)?;
        handle
            .with_metadata(|m| {
                m.file_infos.get(file_id).map(|fi| {
                    let name = fi
                        .relative_filename
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| fi.relative_filename.to_string_lossy().into_owned());
                    (name, fi.len)
                })
            })
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("File id out of range: {file_id}"))
    }

    /// Best-effort resolved on-disk path for a torrent file.
    pub async fn resolve_file_disk_path(&self, hash: &str, file_id: usize) -> Result<Option<PathBuf>, String> {
        validate_hash(hash)?;
        let handle = self.handle(hash)?;
        let rel = handle
            .with_metadata(|m| {
                m.file_infos
                    .get(file_id)
                    .map(|fi| fi.relative_filename.clone())
            })
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("File id out of range: {file_id}"))?;
        let download_dir = self.download_dir.read().await.clone();
        Ok(Some(download_dir.join(rel)))
    }

    /// Open a raw seekable streaming reader for a file.
    pub async fn open_seekable_file_stream(
        &self,
        hash: &str,
        file_id: usize,
    ) -> Result<impl AsyncRead + AsyncSeek + Unpin + Send, String> {
        let handle = self.handle(hash)?;
        // Validate the file id before creating a stream.
        self.file_meta(hash, file_id)?;
        handle
            .stream(file_id)
            .await
            .map_err(|e| format!("Failed to open torrent stream: {e}"))
    }

    /// Open a streaming reader for a file, optionally seeking to `range.0`
    /// and limiting to `range.1 - range.0` bytes.
    pub async fn open_file_stream(
        &self,
        hash: &str,
        file_id: usize,
        range: Option<(u64, u64)>,
    ) -> Result<Box<dyn AsyncRead + Send + Unpin>, String> {
        let mut stream = self.open_seekable_file_stream(hash, file_id).await?;
        match range {
            Some((start, end)) => {
                stream
                    .seek(SeekFrom::Start(start))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(Box::new(stream.take(end.saturating_sub(start))))
            }
            None => Ok(Box::new(stream)),
        }
    }

    /// Read an exact byte range from a torrent file (best-effort; may block until downloaded).
    pub async fn read_file_range(
        &self,
        hash: &str,
        file_id: usize,
        start: u64,
        len: usize,
    ) -> Result<Vec<u8>, String> {
        let mut stream = self.open_seekable_file_stream(hash, file_id).await?;
        stream
            .seek(SeekFrom::Start(start))
            .await
            .map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; len];
        stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("Failed to read torrent data: {e}"))?;
        Ok(buf)
    }

    fn save_library_entry(
        &self,
        handle: &Arc<librqbit::ManagedTorrent>,
        media_id: i64,
        episode: i64,
        background: bool,
    ) {
        let hash = handle.info_hash().as_string();
        let stats = handle.stats();
        let files = handle.with_metadata(|m| m.file_infos.len()).unwrap_or(0);
        let entry = LibraryEntry {
            media_id,
            episode,
            files,
            hash: hash.clone(),
            progress: if stats.total_bytes > 0 {
                stats.progress_bytes as f64 / stats.total_bytes as f64
            } else {
                0.0
            },
            date: now_ms(),
            size: stats.total_bytes,
            name: handle.name().unwrap_or_default(),
            background,
        };
        self.library.upsert(entry);

        if let Ok(bytes) = handle.with_metadata(|m| m.torrent_bytes.to_vec()) {
            if let Err(e) = self.cache.write(&hash, &bytes) {
                log::warn!("Failed to cache torrent metadata for {hash}: {e}");
            }
        }
    }

    async fn evict_foreground_orphan(&self, old_hash: &str) {
        if validate_hash(old_hash).is_err() {
            return;
        }

        // Do not evict background-managed torrents or torrents still marked as background in library.
        if let Some(entry) = self.library.get(old_hash) {
            if entry.background {
                return;
            }
        }

        let persist = self.settings.read().await.torrent_persist;

        if let Ok(handle) = self.handle(old_hash) {
            // Match original intent: when persisting, keep files/store metadata but remove active torrent;
            // when not persisting, destroy stored data for the evicted foreground torrent.
            let delete_files = !persist;
            if let Err(e) = self.session.delete(handle.info_hash().into(), delete_files).await {
                log::warn!("Failed to evict foreground torrent {old_hash}: {e}");
            }
        }

        if !persist {
            self.library.remove(old_hash);
            self.cache.remove(old_hash);
        }

        // Drop in-memory parsed media caches for the evicted torrent.
        self.media_meta
            .write()
            .await
            .retain(|(h, _), _| h != old_hash);
        self.attachments
            .write()
            .await
            .retain(|(h, _, _), _| h != old_hash);
    }
}

#[async_trait]
impl TorrentBackend for RqbitBackend {
    async fn add_torrent(
        &self,
        source: TorrentSource,
        media_id: i64,
        episode: i64,
        background: bool,
    ) -> Result<Vec<TorrentFile>, String> {
        let output_folder = self.download_dir.read().await.clone();
        let settings = self.settings.read().await.clone();

        let opts = AddTorrentOptions {
            overwrite: true,
            output_folder: Some(output_folder.to_string_lossy().into_owned()),
            peer_limit: Some(settings.max_conns.max(1)),
            ratelimits: LimitsConfig {
                download_bps: download_limit(&settings),
                upload_bps: upload_limit(&settings),
            },
            ..Default::default()
        };

        let add = match source {
            TorrentSource::Url(url) => AddTorrent::from_url(url),
            TorrentSource::Bytes(bytes) => AddTorrent::from_bytes(bytes),
        };

        let response = tokio::time::timeout(
            METADATA_TIMEOUT,
            self.session.add_torrent(add, Some(opts)),
        )
        .await
        .map_err(|_| "Timed out resolving torrent metadata".to_string())?
        .map_err(|e| format!("Failed to add torrent: {e}"))?;

        let handle = response
            .into_handle()
            .ok_or_else(|| "Torrent was not started".to_string())?;

        // `add_torrent` resolves magnet metadata, but ensure storage is ready
        // before exposing file URLs.
        let _ = tokio::time::timeout(Duration::from_secs(30), handle.wait_until_initialized()).await;

        let hash = handle.info_hash().as_string();

        // Preserve background status if this torrent was already managed as a background download.
        let existing_background = self.library.get(&hash).map(|e| e.background).unwrap_or(false);
        let effective_background = background || existing_background;

        // Single-session foreground behavior (Electron preload uses a fixed session):
        // switching foreground playback evicts the previous foreground torrent if appropriate.
        if !background {
            let old = {
                let mut fg = self.foreground_hash.write().await;
                let old = fg.clone();
                *fg = Some(hash.clone());
                old
            };
            if let Some(old_hash) = old {
                if !old_hash.eq_ignore_ascii_case(&hash) {
                    self.evict_foreground_orphan(&old_hash).await;
                }
            }
        } else {
            // Background adds should not displace the current foreground playback session.
        }

        self.save_library_entry(&handle, media_id, episode, effective_background);
        Ok(self.build_files(&handle, &hash))
    }

    async fn active_torrents(&self) -> Result<Vec<TorrentInfo>, String> {
        let out = self.session.with_torrents(|torrents| {
            let mut out = Vec::new();
            for (_, handle) in torrents {
                let hash = handle.info_hash().as_string();
                out.push(self.build_info(handle, &hash));
            }
            out
        });
        Ok(out)
    }

    async fn torrent_info(&self, hash: &str) -> Result<TorrentInfo, String> {
        let handle = self.handle(hash)?;
        Ok(self.build_info(&handle, hash))
    }

    async fn file_info(&self, hash: &str) -> Result<Vec<FileInfo>, String> {
        let handle = self.handle(hash)?;
        let stats = handle.stats();
        let selected = handle.only_files();
        handle
            .with_metadata(|m| {
                m.file_infos
                    .iter()
                    .enumerate()
                    .map(|(id, fi)| {
                        let downloaded = stats.file_progress.get(id).copied().unwrap_or(0);
                        FileInfo {
                            name: fi
                                .relative_filename
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| {
                                    fi.relative_filename.to_string_lossy().into_owned()
                                }),
                            size: fi.len,
                            progress: if fi.len > 0 {
                                (downloaded as f64 / fi.len as f64).min(1.0)
                            } else {
                                0.0
                            },
                            selections: if selected
                                .as_ref()
                                .map(|v| v.contains(&id))
                                .unwrap_or(false)
                            {
                                1
                            } else {
                                0
                            },
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .map_err(|e| e.to_string())
    }

    async fn peer_info(&self, hash: &str) -> Result<Vec<PeerInfo>, String> {
        let handle = self.handle(hash)?;
        let Some(live) = handle.live() else {
            return Ok(vec![]);
        };
        let total_pieces = handle
            .with_metadata(|m| m.info.lengths().total_pieces() as f64)
            .unwrap_or(0.0);
        let snapshot = live.per_peer_stats_snapshot(PeerStatsFilter::default());
        let peers = snapshot
            .peers
            .into_iter()
            .map(|(addr, p)| {
                let mut flags = Vec::with_capacity(2);
                if p.counters.incoming_connections > 0 {
                    flags.push("incoming".to_string());
                } else {
                    flags.push("outgoing".to_string());
                }
                if let Some(kind) = p.conn_kind {
                    let kind = kind.to_string();
                    if kind.contains("utp") {
                        flags.push("utp".to_string());
                    }
                }
                let progress = if total_pieces > 0.0 {
                    (p.counters.downloaded_and_checked_pieces as f64 / total_pieces).min(1.0)
                } else {
                    0.0
                };
                PeerInfo {
                    ip: normalize_peer_ip(&addr),
                    seeder: progress >= 1.0,
                    client: p.client_name.unwrap_or_default(),
                    progress,
                    size: SizeStats {
                        total: 0,
                        downloaded: p.counters.fetched_bytes,
                        uploaded: p.counters.uploaded_bytes,
                    },
                    speed: SpeedStats::default(),
                    flags,
                    time: 0,
                }
            })
            .collect();
        Ok(peers)
    }

    async fn trackers(&self, hash: &str) -> Result<BTreeMap<String, TrackerStats>, String> {
        let handle = self.handle(hash)?;
        let mut out = BTreeMap::new();

        // Active/shared tracker URLs (best-effort).
        for url in &handle.shared().trackers {
            out.insert(url.to_string(), TrackerStats::default());
        }

        // Fallback to metadata announce/announce-list URLs if available.
        // TorrentMetadata in librqbit 9.1.0 does not directly expose announce lists on `m.info`;
        // shared tracker URLs above are used as the best-effort source. Kept as a hook for
        // future parsing from `m.torrent_bytes` if needed.

        Ok(out)
    }

    async fn protocol_status(&self, hash: &str) -> Result<ProtocolStatus, String> {
        let handle = self.handle(hash)?;
        let settings = self.settings.read().await;
        let is_private = handle
            .with_metadata(|m| m.info.info().private)
            .unwrap_or(false);
        let listen = self.session.listen_addr().is_some();
        let has_incoming = handle
            .stats()
            .live
            .as_ref()
            .map(|l| l.snapshot.peer_stats.seen > 0)
            .unwrap_or(false);
        Ok(ProtocolStatus {
            dht: self.session.get_dht().is_some(),
            // librqbit does not expose LSD state directly; best-effort placeholder.
            lsd: !is_private,
            pex: !is_private && !settings.torrent_pex,
            nat: listen,
            forwarding: listen || has_incoming,
            persisting: settings.torrent_persist,
            streaming: settings.torrent_streamed_download,
        })
    }

    async fn library(&self) -> Result<Vec<LibraryEntry>, String> {
        let mut entries = self.library.list();
        for entry in &mut entries {
            if let Ok(handle) = self.handle(&entry.hash) {
                let stats = handle.stats();
                entry.size = stats.total_bytes;
                entry.progress = if stats.total_bytes > 0 {
                    stats.progress_bytes as f64 / stats.total_bytes as f64
                } else {
                    0.0
                };
                if let Some(name) = handle.name() {
                    entry.name = name;
                }
            }
        }
        // Original store iteration order is filesystem-dependent; UI primarily keys by hash/id.
        // Keep a stable, recency-biased order for deterministic results.
        entries.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.hash.cmp(&b.hash)));
        Ok(entries)
    }

    async fn cached_torrents(&self) -> Result<Vec<String>, String> {
        // Return hashes of cached `.torrent` metadata files available for rescan/load.
        let dir = self.cache.dir_path();
        let mut hashes = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(self.library.list().into_iter().map(|e| e.hash).collect());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("torrent") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if validate_hash(stem).is_ok() {
                    hashes.push(stem.to_string());
                }
            }
        }
        if hashes.is_empty() {
            // Fallback for environments where cache dir listing is unavailable.
            Ok(self.library.list().into_iter().map(|e| e.hash).collect())
        } else {
            Ok(hashes)
        }
    }

    async fn rescan_torrents(&self, hashes: &[String]) -> Result<(), String> {
        let output_folder = self.download_dir.read().await.clone();
        for hash in hashes {
            validate_hash(hash)?;
            if self.is_active(hash) {
                continue;
            }
            let Some(bytes) = self.cache.read(hash) else {
                continue;
            };
            let opts = AddTorrentOptions {
                paused: true,
                overwrite: true,
                output_folder: Some(output_folder.to_string_lossy().into_owned()),
                ..Default::default()
            };
            let result = tokio::time::timeout(
                RESCAN_TIMEOUT,
                self.session
                    .add_torrent(AddTorrent::from_bytes(bytes), Some(opts)),
            )
            .await;
            let Ok(Ok(response)) = result else {
                continue;
            };
            if let Some(handle) = response.into_handle() {
                let stats = handle.stats();
                self.library.update_rescan_state(
                    hash,
                    if stats.total_bytes > 0 {
                        stats.progress_bytes as f64 / stats.total_bytes as f64
                    } else {
                        0.0
                    },
                    stats.total_bytes,
                    now_ms(),
                );
                let _ = self
                    .session
                    .delete(handle.info_hash().into(), false)
                    .await;
            }
        }
        Ok(())
    }

    async fn delete_torrents(&self, hashes: &[String]) -> Result<(), String> {
        let foreground = self.foreground_hash.read().await.clone();
        for hash in hashes {
            validate_hash(hash)?;
            // Mirror original behavior: do not delete torrents that are currently in an active session.
            if foreground
                .as_ref()
                .is_some_and(|fg| fg.eq_ignore_ascii_case(hash))
            {
                continue;
            }

            if let Ok(handle) = self.handle(hash) {
                self.session
                    .delete(handle.info_hash().into(), true)
                    .await
                    .map_err(|e| format!("Failed to delete torrent {hash}: {e}"))?;
            }
            self.library.remove(hash);
            self.cache.remove(hash);
            self.media_meta
                .write()
                .await
                .retain(|(h, _), _| !h.eq_ignore_ascii_case(hash));
            self.attachments
                .write()
                .await
                .retain(|(h, _, _), _| !h.eq_ignore_ascii_case(hash));
        }
        Ok(())
    }

    async fn remove_background_torrents(&self, hashes: &[String]) -> Result<(), String> {
        let foreground = self.foreground_hash.read().await.clone();
        let persist = self.settings.read().await.torrent_persist;
        for hash in hashes {
            validate_hash(hash)?;
            // Mirror original behavior: do not remove torrents currently active in a session.
            if foreground
                .as_ref()
                .is_some_and(|fg| fg.eq_ignore_ascii_case(hash))
            {
                continue;
            }

            let Some(entry) = self.library.get(hash) else {
                continue;
            };
            if !entry.background {
                continue;
            }

            if let Ok(handle) = self.handle(hash) {
                // If persisting, keep stored files; otherwise destroy data for the removed background torrent.
                let delete_files = !persist;
                let _ = self
                    .session
                    .delete(handle.info_hash().into(), delete_files)
                    .await;
            }

            if persist {
                let mut updated = entry;
                updated.background = false;
                self.library.upsert(updated);
            } else {
                self.library.remove(hash);
                self.cache.remove(hash);
                self.media_meta
                    .write()
                    .await
                    .retain(|(h, _), _| !h.eq_ignore_ascii_case(hash));
                self.attachments
                    .write()
                    .await
                    .retain(|(h, _, _), _| !h.eq_ignore_ascii_case(hash));
            }
        }
        Ok(())
    }

    async fn check_available_space(&self, path: Option<&str>) -> Result<u64, String> {
        let target = match path {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => self.download_dir.read().await.clone(),
        };
        let canonical = target.canonicalize().unwrap_or_else(|_| target.clone());
        let disks = sysinfo::Disks::new_with_refreshed_list();

        fn normalize_for_match(p: &std::path::Path) -> std::path::PathBuf {
            let s = p.to_string_lossy();
            // Windows extended-length paths (\\?\C:\...) won't prefix-match mount points like C:\.
            let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
            std::path::PathBuf::from(stripped)
        }

        let candidates = [normalize_for_match(&canonical), normalize_for_match(&target)];
        let mut best: Option<(usize, u64)> = None;
        for disk in disks.iter() {
            let mount = normalize_for_match(disk.mount_point());
            for candidate in &candidates {
                #[cfg(windows)]
                let matches = {
                    let c = candidate.to_string_lossy().to_lowercase();
                    let m = mount.to_string_lossy().to_lowercase();
                    c.starts_with(&m)
                };
                #[cfg(not(windows))]
                let matches = candidate.starts_with(&mount);

                if matches {
                    let len = mount.as_os_str().len();
                    if best.map(|(l, _)| len >= l).unwrap_or(true) {
                        best = Some((len, disk.available_space()));
                    }
                }
            }
        }

        best.map(|(_, space)| space)
            .ok_or_else(|| "Could not determine available disk space".to_string())
    }

    async fn check_incoming_connections(&self, port: u16) -> Result<bool, String> {
        // Full parity with the Electron implementation requires a DHT announce +
        // NAT-PMP/UPnP round trip. Until that is ported, provide a best-effort
        // heuristic: if the session is already listening on the requested port (or
        // any port when 0 is passed), assume reachability may be possible.
        if let Some(addr) = self.session.listen_addr() {
            if port == 0 || addr.port() == port {
                return Ok(true);
            }
        }

        if port == 0 {
            return Ok(false);
        }

        match tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
            Ok(listener) => {
                drop(listener);
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }

    async fn create_nzb(&self, _hash: &str, _url: &str) -> Result<(), String> {
        Err("NZB webseed support is not implemented in the Tauri backend yet".into())
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

    async fn update_settings(&self, settings: &ClientSettings) -> Result<(), String> {
        self.session
            .ratelimits
            .set_download_bps(download_limit(settings));
        self.session
            .ratelimits
            .set_upload_bps(upload_limit(settings));
        *self.settings.write().await = settings.clone();
        Ok(())
    }

    async fn set_download_dir(&self, path: &str) -> Result<(), String> {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("Download path must be absolute".into());
        }
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|e| format!("Failed to create download directory: {e}"))?;
        *self.download_dir.write().await = path;
        Ok(())
    }

    async fn file_disk_path(&self, hash: &str, id: usize) -> Result<Option<PathBuf>, String> {
        self.resolve_file_disk_path(hash, id).await
    }

    async fn attachments(&self, hash: &str, id: usize) -> Result<Vec<Attachment>, String> {
        validate_hash(hash)?;
        let meta = self.ensure_media_meta(hash, id).await?;
        let port = self.stream_port;
        let token = &self.stream_token;
        let mut out = Vec::with_capacity(meta.attachment_ids.len());
        for att_id in meta.attachment_ids {
            if let Some(att) = self
                .attachments
                .read()
                .await
                .get(&(hash.to_string(), id, att_id))
                .cloned()
            {
                out.push(Attachment {
                    filename: att.name.clone(),
                    mimetype: att.mime.clone(),
                    id: att_id,
                    url: super::media::attachment_url(port, token, hash, id, att_id, &att.name),
                });
            }
        }
        Ok(out)
    }

    async fn tracks(&self, hash: &str, id: usize) -> Result<Vec<SubtitleTrack>, String> {
        validate_hash(hash)?;
        let meta = self.ensure_media_meta(hash, id).await?;
        Ok(meta.tracks)
    }

    async fn chapters(&self, hash: &str, id: usize) -> Result<Vec<Chapter>, String> {
        validate_hash(hash)?;
        let meta = self.ensure_media_meta(hash, id).await?;
        Ok(meta.chapters)
    }

    async fn update_peer_counts(&self, hashes: &[String]) -> Result<Vec<PeerCountScrape>, String> {
        let mut out = Vec::with_capacity(hashes.len());
        for hash in hashes {
            validate_hash(hash)?;
            // librqbit does not expose direct tracker scrape in the current integration;
            // return best-effort zeros in the expected contract shape.
            let (complete, incomplete) = if let Ok(handle) = self.handle(hash) {
                if let Some(live) = handle.stats().live {
                    let peers = &live.snapshot.peer_stats;
                    let seeders = peers.live_seeders.max(0) as u64;
                    let leechers = peers.live.saturating_sub(peers.live_seeders).max(0) as u64;
                    (seeders.to_string(), leechers.to_string())
                } else {
                    ("0".into(), "0".into())
                }
            } else {
                ("0".into(), "0".into())
            };
            out.push(PeerCountScrape {
                hash: hash.clone(),
                complete,
                downloaded: "0".into(),
                incomplete,
            });
        }
        Ok(out)
    }
}

/// Start the librqbit session and the local streaming HTTP server.
pub async fn start_rqbit(
    settings: ClientSettings,
    download_dir: PathBuf,
    library_path: PathBuf,
    cache_dir: PathBuf,
) -> Result<Arc<RqbitBackend>, String> {
    tokio::fs::create_dir_all(&download_dir)
        .await
        .map_err(|e| format!("Failed to create download directory: {e}"))?;

    let dht = if settings.torrent_dht {
        None
    } else {
        Some(DhtSessionConfig {
            port: if settings.dht_port == 0 {
                None
            } else {
                Some(settings.dht_port)
            },
            ..Default::default()
        })
    };

    let opts = SessionOptions {
        dht,
        persistence: None,
        ratelimits: LimitsConfig {
            download_bps: download_limit(&settings),
            upload_bps: upload_limit(&settings),
        },
        peer_limit: Some(settings.max_conns.max(1)),
        listen: Some(ListenerOptions {
            listen_addr: SocketAddr::from((Ipv4Addr::UNSPECIFIED, settings.torrent_port)),
            enable_upnp_port_forwarding: false,
            ipv4_only: true,
            ..Default::default()
        }),
        client_name_and_version: Some(format!(
            "Hayase/{} (Tauri)",
            env!("CARGO_PKG_VERSION")
        )),
        ..Default::default()
    };

    let session = Session::new_with_opts(download_dir.clone(), opts)
        .await
        .map_err(|e| format!("Failed to start torrent session: {e}"))?;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|e| format!("Failed to bind streaming server: {e}"))?;
    let stream_port = listener
        .local_addr()
        .map_err(|e| format!("Failed to read streaming server address: {e}"))?
        .port();
    let token = random_token();

    let backend = Arc::new(RqbitBackend {
        session,
        download_dir: RwLock::new(download_dir),
        library: LibraryStore::load(library_path),
        cache: TorrentCache::new(cache_dir),
        stream_port,
        stream_token: token.clone(),
        settings: RwLock::new(settings),
        attachments: RwLock::new(HashMap::new()),
        media_meta: RwLock::new(HashMap::new()),
        foreground_hash: RwLock::new(None),
    });

    let app = stream_server::router(Arc::clone(&backend), token);
    tauri::async_runtime::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("Torrent stream server stopped: {e}");
        }
    });

    Ok(backend)
}
