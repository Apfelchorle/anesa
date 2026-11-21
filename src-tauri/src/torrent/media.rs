//! Best-effort MKV/EBML media metadata extraction (attachments, subtitle tracks, chapters).
//!
//! Two sources are supported:
//! - a completed file on disk (fast path), and
//! - the live torrent byte stream, which lets `Tracks`/`Attachments`/`Chapters`
//!   be read from the swarm while the file is still downloading.
//!
//! Embedded subtitle *cues* are handled separately by [`super::subtitles`],
//! which taps the player's read stream.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use matroska::{Matroska, Tracktype};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

use super::ebml;
use super::{Chapter, SubtitleTrack};

/// How much of the file is scanned for `TimestampScale` when the high-level
/// `matroska` crate (which does not expose it) has already parsed the rest.
const TIMESTAMP_PROBE_BYTES: u64 = 2 * 1024 * 1024;
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct ExtractedAttachment {
    pub filename: String,
    pub mimetype: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct MediaMetadata {
    pub attachments: Vec<ExtractedAttachment>,
    pub tracks: Vec<SubtitleTrack>,
    pub chapters: Vec<Chapter>,
    /// Nanoseconds per Matroska timestamp tick (normally 1_000_000).
    pub timestamp_scale: u64,
}

/// Parse MKV metadata from a reader.
pub fn parse_mkv<R: Read + Seek>(reader: &mut R) -> Result<MediaMetadata, String> {
    let mut mkv = Matroska::open(&mut *reader).map_err(|e| format!("Failed to parse Matroska: {e}"))?;

    let attachments = std::mem::take(&mut mkv.attachments)
        .into_iter()
        .map(|a| ExtractedAttachment {
            filename: a.name,
            mimetype: a.mime_type,
            data: a.data,
        })
        .collect();

    let tracks = mkv
        .tracks
        .iter()
        .filter(|t| t.tracktype == Tracktype::Subtitle)
        .map(|t| {
            let codec_id = t.codec_id.as_str();
            let track_type = codec_id
                .strip_prefix("S_TEXT/")
                .unwrap_or(codec_id)
                .to_lowercase();

            let language = t
                .language
                .as_ref()
                .map(|l| normalize_language(&l.to_string()))
                .unwrap_or_else(|| "eng".into());

            let header = t
                .codec_private
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok());

            SubtitleTrack {
                number: t.number as i64,
                language,
                track_type,
                // Not exposed by the matroska 0.30.1 high-level API. The live
                // extractor sniffs zlib payloads instead.
                compressed: false,
                default: t.default,
                forced: t.forced,
                name: t.name.clone().filter(|s| !s.is_empty()),
                header,
            }
        })
        .collect();

    let chapters = extract_chapters(&mkv);
    let timestamp_scale = probe_timestamp_scale(reader).unwrap_or(1_000_000);

    Ok(MediaMetadata {
        attachments,
        tracks,
        chapters,
        timestamp_scale,
    })
}

/// Parse MKV metadata from an owned reader (used by the blocking stream adapter).
pub fn parse_mkv_owned<R: Read + Seek>(mut reader: R) -> Result<MediaMetadata, String> {
    parse_mkv(&mut reader)
}

/// Parse MKV metadata from an on-disk file path.
pub fn parse_mkv_file(path: &Path) -> Result<MediaMetadata, String> {
    let mut file = File::open(path).map_err(|e| format!("Failed to open media file: {e}"))?;
    parse_mkv(&mut file)
}

/// Whether a filename looks like a Matroska container.
pub fn is_matroska_name(path: impl AsRef<Path>) -> bool {
    path.as_ref()
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mkv" | "webm" | "mka" | "mk3d"
            )
        })
        .unwrap_or(false)
}

/// Synchronous `Read + Seek` adapter over a `tokio` stream.
///
/// `matroska::Matroska::open` is synchronous, but the torrent stream is async.
/// The adapter is used from `spawn_blocking`, where `Handle::block_on` is legal.
/// Each read is bounded so a cold torrent cannot wedge the blocking thread.
pub struct SyncStreamReader<S> {
    handle: tokio::runtime::Handle,
    stream: S,
    pos: u64,
    len: u64,
}

impl<S> SyncStreamReader<S> {
    pub fn new(handle: tokio::runtime::Handle, stream: S, len: u64) -> Self {
        Self {
            handle,
            stream,
            pos: 0,
            len,
        }
    }
}

impl<S: AsyncRead + Unpin> Read for SyncStreamReader<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let handle = self.handle.clone();
        let stream = &mut self.stream;
        let n = handle.block_on(async {
            match tokio::time::timeout(STREAM_READ_TIMEOUT, stream.read(buf)).await {
                Ok(result) => result,
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out reading torrent stream",
                )),
            }
        })?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<S: AsyncSeek + Unpin> Seek for SyncStreamReader<S> {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let from = match from {
            SeekFrom::End(offset) => {
                let target = (self.len as i64).saturating_add(offset).max(0) as u64;
                SeekFrom::Start(target)
            }
            other => other,
        };
        let handle = self.handle.clone();
        let stream = &mut self.stream;
        let pos = handle.block_on(async { stream.seek(from).await })?;
        self.pos = pos;
        Ok(pos)
    }
}

fn normalize_language(lang: &str) -> String {
    let l = lang.trim();
    if l.is_empty() || l.eq_ignore_ascii_case("und") {
        return "eng".into();
    }
    match l.to_ascii_lowercase().as_str() {
        "en" => "eng".into(),
        "ja" | "jpn" => "jpn".into(),
        "zh" | "zho" | "chi" => "chi".into(),
        "ko" | "kor" => "kor".into(),
        "fr" | "fra" | "fre" => "fra".into(),
        "de" | "deu" | "ger" => "deu".into(),
        "es" | "spa" => "spa".into(),
        "it" | "ita" => "ita".into(),
        "pt" | "por" => "por".into(),
        "ru" | "rus" => "rus".into(),
        "ar" | "ara" => "ara".into(),
        other => other.to_string(),
    }
}

fn extract_chapters(mkv: &Matroska) -> Vec<Chapter> {
    if mkv.chapters.is_empty() {
        return vec![];
    }

    // Prefer default edition, else first.
    let edition = mkv
        .chapters
        .iter()
        .find(|e| e.default)
        .or_else(|| mkv.chapters.first());

    let Some(edition) = edition else {
        return vec![];
    };

    let mut atoms: Vec<&matroska::Chapter> = Vec::new();
    collect_chapter_atoms(&edition.chapters, &mut atoms);

    let duration_ms = mkv.info.duration.map(|d| d.as_nanos() as f64 / 1_000_000.0);

    let mut out = Vec::with_capacity(atoms.len());
    for (i, atom) in atoms.iter().enumerate() {
        let start_ms = atom.time_start.as_nanos() as f64 / 1_000_000.0;
        let end_ms = if let Some(end) = atom.time_end {
            end.as_nanos() as f64 / 1_000_000.0
        } else if let Some(next) = atoms.get(i + 1) {
            next.time_start.as_nanos() as f64 / 1_000_000.0
        } else if let Some(dur) = duration_ms {
            dur
        } else {
            start_ms
        };
        let end_ms = if end_ms < start_ms { start_ms } else { end_ms };

        let text = atom
            .display
            .first()
            .map(|d| d.string.clone())
            .unwrap_or_default();

        out.push(Chapter {
            start: start_ms,
            end: end_ms,
            text,
        });
    }

    out
}

fn collect_chapter_atoms<'a>(chapters: &'a [matroska::Chapter], out: &mut Vec<&'a matroska::Chapter>) {
    for ch in chapters {
        if !ch.hidden {
            out.push(ch);
        }
        // matroska 0.30.1 Chapter does not expose nested child atoms in the high-level API.
    }
}

/// Read `TimestampScale` from the beginning of the file.
///
/// The `matroska` crate converts durations using the scale but does not expose
/// the raw value, and cue timestamps need it.
fn probe_timestamp_scale<R: Read + Seek>(reader: &mut R) -> Option<u64> {
    reader.seek(SeekFrom::Start(0)).ok()?;
    let mut buf = Vec::with_capacity(256 * 1024);
    reader
        .by_ref()
        .take(TIMESTAMP_PROBE_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    timestamp_scale_from_bytes(&buf)
}

/// Extract `TimestampScale` from a buffered EBML prefix.
pub fn timestamp_scale_from_bytes(data: &[u8]) -> Option<u64> {
    let mut pos = 0usize;
    let mut segment_start = 0usize;

    loop {
        let id = ebml::read_id(data, &mut pos)?;
        let (size, unknown) = ebml::read_size(data, &mut pos)?;
        let body_start = pos;

        match id {
            ebml::ID_SEGMENT => {
                // Descend into the segment rather than skipping it.
                segment_start = body_start;
            }
            ebml::ID_INFO => {
                let end = if unknown {
                    data.len()
                } else {
                    body_start.checked_add(size as usize)?.min(data.len())
                };
                return find_timestamp_scale(data, body_start, end);
            }
            ebml::ID_SEEKHEAD => {
                if unknown {
                    return None;
                }
                let end = body_start.checked_add(size as usize)?;
                if end > data.len() {
                    return None;
                }
                if let Some(offset) = find_seek_position(data, body_start, end, ebml::ID_INFO) {
                    let info_start = segment_start.checked_add(offset as usize)?;
                    if info_start < data.len() {
                        let mut p = info_start;
                        if let (Some(info_id), Some((info_size, info_unknown))) =
                            (ebml::read_id(data, &mut p), ebml::read_size(data, &mut p))
                        {
                            if info_id == ebml::ID_INFO {
                                let info_end = if info_unknown {
                                    data.len()
                                } else {
                                    p.checked_add(info_size as usize)?.min(data.len())
                                };
                                return find_timestamp_scale(data, p, info_end);
                            }
                        }
                    }
                }
                pos = end;
            }
            ebml::ID_CLUSTER => return None,
            _ => {
                if unknown {
                    return None;
                }
                let end = body_start.checked_add(size as usize)?;
                if end > data.len() {
                    return None;
                }
                pos = end;
            }
        }
    }
}

fn find_timestamp_scale(data: &[u8], mut pos: usize, end: usize) -> Option<u64> {
    while pos < end {
        let id = ebml::read_id(data, &mut pos)?;
        let (size, unknown) = ebml::read_size(data, &mut pos)?;
        if unknown {
            return None;
        }
        let body_end = pos.checked_add(size as usize)?;
        if body_end > end {
            return None;
        }
        if id == ebml::ID_TIMESTAMP_SCALE {
            return ebml::read_uint(data, &mut pos, size as usize);
        }
        pos = body_end;
    }
    None
}

fn find_seek_position(data: &[u8], mut pos: usize, end: usize, target: u32) -> Option<u64> {
    while pos < end {
        let id = ebml::read_id(data, &mut pos)?;
        let (size, unknown) = ebml::read_size(data, &mut pos)?;
        if unknown {
            return None;
        }
        let body_end = pos.checked_add(size as usize)?;
        if body_end > end {
            return None;
        }
        if id == ebml::ID_SEEK {
            let mut p = pos;
            let mut seek_id = None;
            let mut seek_pos = None;
            while p < body_end {
                let child = ebml::read_id(data, &mut p)?;
                let (child_size, child_unknown) = ebml::read_size(data, &mut p)?;
                if child_unknown {
                    return None;
                }
                let child_end = p.checked_add(child_size as usize)?;
                if child_end > body_end {
                    return None;
                }
                match child {
                    ebml::ID_SEEK_ID => {
                        seek_id = ebml::read_uint(data, &mut p, child_size as usize);
                        p = child_end;
                    }
                    ebml::ID_SEEK_POSITION => {
                        seek_pos = ebml::read_uint(data, &mut p, child_size as usize);
                        p = child_end;
                    }
                    _ => p = child_end,
                }
            }
            if seek_id == Some(target as u64) {
                return seek_pos;
            }
        }
        pos = body_end;
    }
    None
}

/// Build a streaming/attachment URL for an extracted attachment.
pub fn attachment_url(
    port: u16,
    token: &str,
    hash: &str,
    file_id: usize,
    att_id: usize,
    filename: &str,
) -> String {
    let encoded = percent_encoding::utf8_percent_encode(filename, percent_encoding::NON_ALPHANUMERIC)
        .to_string();
    format!("http://127.0.0.1:{port}/attachment/{token}/{hash}/{file_id}/{att_id}/{encoded}")
}
