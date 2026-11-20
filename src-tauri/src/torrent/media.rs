//! Best-effort MKV/EBML media metadata extraction (attachments, subtitle tracks, chapters).
//!
//! Uses the `matroska` crate to parse container metadata from on-disk torrent files.
//! This is a parity scaffold: attachment/track/chapter extraction works for standard
//! MKV files, while embedded subtitle cue streaming (`torrent://subtitle`) remains a
//! separate, not-yet-ported path.

use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;

use matroska::{Matroska, Tracktype};

use super::{Chapter, SubtitleTrack};

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
}

/// Parse MKV metadata from a reader.
pub fn parse_mkv<R: Read + Seek>(reader: &mut R) -> Result<MediaMetadata, String> {
    let mut mkv = Matroska::open(reader).map_err(|e| format!("Failed to parse Matroska: {e}"))?;

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
                compressed: false, // not exposed by matroska 0.30.1 high-level API
                default: t.default,
                forced: t.forced,
                name: t.name.clone().filter(|s| !s.is_empty()),
                header,
            }
        })
        .collect();

    let chapters = extract_chapters(&mkv);

    Ok(MediaMetadata {
        attachments,
        tracks,
        chapters,
    })
}

/// Parse MKV metadata from an on-disk file path.
pub fn parse_mkv_file(path: &Path) -> Result<MediaMetadata, String> {
    let mut file = File::open(path).map_err(|e| format!("Failed to open media file: {e}"))?;
    parse_mkv(&mut file)
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
