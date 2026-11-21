//! Live EBML subtitle extraction from the player's torrent read stream.
//!
//! The HTTP stream server mirrors every chunk the video player reads into a
//! [`SubtitleTapRegistry`]. A single extractor per `(infohash, file_id)` parses
//! those chunks as an EBML/Matroska stream, so embedded subtitles work while
//! the episode is still downloading and without a second full download.
//!
//! This is the Rust equivalent of `hayase-torrent-client/ebml/metadata.ts`:
//! it filters by subtitle track, applies `TimestampScale`, inflates zlib
//! tracks, and resynchronises on the next cluster after a seek gap.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::Mutex;

use flate2::read::ZlibDecoder;
use tokio::sync::broadcast;

use super::ebml;
use super::SubtitleCue;

/// How much of the player's recent read stream is replayed to a late
/// subscriber. Only the startup race needs this; live chunks follow.
const TAP_HISTORY_BYTES: usize = 16 * 1024 * 1024;
const CUE_HISTORY: usize = 8192;
const CUE_CHANNEL: usize = 2048;
const MAX_BLOCK_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct TapChunk {
    pub offset: u64,
    pub data: Vec<u8>,
}

struct TapState {
    tap_tx: broadcast::Sender<TapChunk>,
    tap_history: VecDeque<TapChunk>,
    tap_history_bytes: usize,
    cue_tx: broadcast::Sender<SubtitleCue>,
    cues: Vec<SubtitleCue>,
    extractor_started: bool,
}

impl TapState {
    fn new() -> Self {
        let (tap_tx, _) = broadcast::channel(2048);
        let (cue_tx, _) = broadcast::channel(CUE_CHANNEL);
        Self {
            tap_tx,
            tap_history: VecDeque::new(),
            tap_history_bytes: 0,
            cue_tx,
            cues: Vec::new(),
            extractor_started: false,
        }
    }
}

/// Shared tap state keyed by torrent file.
#[derive(Default)]
pub struct SubtitleTapRegistry {
    inner: Mutex<HashMap<(String, usize), TapState>>,
}

impl SubtitleTapRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, usize), TapState>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn sender(&self, hash: &str, file_id: usize) -> broadcast::Sender<TapChunk> {
        self.lock()
            .entry((hash.to_string(), file_id))
            .or_insert_with(TapState::new)
            .tap_tx
            .clone()
    }

    pub fn publish(&self, hash: &str, file_id: usize, chunk: TapChunk) {
        if chunk.data.is_empty() {
            return;
        }
        let mut map = self.lock();
        let state = map
            .entry((hash.to_string(), file_id))
            .or_insert_with(TapState::new);

        state.tap_history_bytes = state.tap_history_bytes.saturating_add(chunk.data.len());
        state.tap_history.push_back(chunk.clone());
        while state.tap_history_bytes > TAP_HISTORY_BYTES {
            let Some(front) = state.tap_history.pop_front() else {
                break;
            };
            state.tap_history_bytes = state.tap_history_bytes.saturating_sub(front.data.len());
        }

        let _ = state.tap_tx.send(chunk);
    }

    pub fn subscribe(&self, hash: &str, file_id: usize) -> (Vec<TapChunk>, broadcast::Receiver<TapChunk>) {
        let mut map = self.lock();
        let state = map
            .entry((hash.to_string(), file_id))
            .or_insert_with(TapState::new);
        let history = state.tap_history.iter().cloned().collect();
        (history, state.tap_tx.subscribe())
    }

    pub fn publish_cues(&self, hash: &str, file_id: usize, cues: Vec<SubtitleCue>) {
        if cues.is_empty() {
            return;
        }
        let mut map = self.lock();
        let state = map
            .entry((hash.to_string(), file_id))
            .or_insert_with(TapState::new);
        for cue in &cues {
            let _ = state.cue_tx.send(cue.clone());
        }
        state.cues.extend(cues);
        if state.cues.len() > CUE_HISTORY {
            let drop = state.cues.len() - CUE_HISTORY;
            state.cues.drain(..drop);
        }
    }

    pub fn subscribe_cues(
        &self,
        hash: &str,
        file_id: usize,
    ) -> (Vec<SubtitleCue>, broadcast::Receiver<SubtitleCue>) {
        let mut map = self.lock();
        let state = map
            .entry((hash.to_string(), file_id))
            .or_insert_with(TapState::new);
        (state.cues.clone(), state.cue_tx.subscribe())
    }

    /// Claim the single extractor slot for a file. Returns `false` if one is
    /// already running.
    pub fn claim_extractor(&self, hash: &str, file_id: usize) -> bool {
        let mut map = self.lock();
        let state = map
            .entry((hash.to_string(), file_id))
            .or_insert_with(TapState::new);
        if state.extractor_started {
            false
        } else {
            state.extractor_started = true;
            true
        }
    }

    pub fn release_extractor(&self, hash: &str, file_id: usize) {
        if let Some(state) = self.lock().get_mut(&(hash.to_string(), file_id)) {
            state.extractor_started = false;
        }
    }

    pub fn clear(&self, hash: &str) {
        self.lock()
            .retain(|(h, _), _| !h.eq_ignore_ascii_case(hash));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Top,
    Cluster,
    BlockGroup,
}

/// Incremental Matroska cluster parser. Feed it tap chunks in offset order.
pub struct SubtitleExtractor {
    /// Subtitle track number -> whether the track declares zlib compression.
    tracks: HashMap<u64, bool>,
    timestamp_scale: u64,
    buf: Vec<u8>,
    buf_offset: u64,
    state: ParseState,
    cluster_time: i64,
    block_duration: Option<u64>,
    group_end: Option<u64>,
    group_cues: Vec<SubtitleCue>,
    group_frames: usize,
    skip_until: Option<u64>,
    resyncing: bool,
}

impl SubtitleExtractor {
    pub fn new(tracks: HashMap<u64, bool>, timestamp_scale: u64) -> Self {
        Self {
            tracks,
            timestamp_scale: if timestamp_scale == 0 {
                1_000_000
            } else {
                timestamp_scale
            },
            buf: Vec::new(),
            buf_offset: 0,
            state: ParseState::Top,
            cluster_time: 0,
            block_duration: None,
            group_end: None,
            group_cues: Vec::new(),
            group_frames: 0,
            skip_until: None,
            resyncing: false,
        }
    }

    /// Drop buffered state after a missed/lagged chunk. The next feed will
    /// scan forward to the next cluster.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.state = ParseState::Cluster;
        self.cluster_time = 0;
        self.block_duration = None;
        self.group_end = None;
        self.group_cues.clear();
        self.group_frames = 0;
        self.skip_until = None;
        self.resyncing = true;
    }

    pub fn push(&mut self, chunk: TapChunk) -> Vec<SubtitleCue> {
        let mut out = Vec::new();
        // Drain anything already buffered before a possible seek gap discards it.
        self.parse(&mut out);
        self.feed(chunk);
        self.parse(&mut out);
        out
    }

    fn feed(&mut self, chunk: TapChunk) {
        if chunk.data.is_empty() {
            return;
        }
        let chunk_start = chunk.offset;
        let chunk_end = chunk_start.saturating_add(chunk.data.len() as u64);
        let buf_end = self.buf_offset.saturating_add(self.buf.len() as u64);

        if self.buf.is_empty() {
            if chunk_end <= self.buf_offset {
                return;
            }
            if chunk_start > self.buf_offset {
                self.begin_resync(chunk);
                return;
            }
            let skip = (self.buf_offset - chunk_start) as usize;
            self.buf.extend_from_slice(&chunk.data[skip..]);
            return;
        }

        if chunk_start > buf_end {
            self.begin_resync(chunk);
            return;
        }
        if chunk_end <= buf_end {
            return;
        }
        let skip = buf_end.saturating_sub(chunk_start) as usize;
        if skip < chunk.data.len() {
            self.buf.extend_from_slice(&chunk.data[skip..]);
        }
    }

    fn begin_resync(&mut self, chunk: TapChunk) {
        self.buf.clear();
        self.buf.extend_from_slice(&chunk.data);
        self.buf_offset = chunk.offset;
        self.state = ParseState::Cluster;
        self.cluster_time = 0;
        self.block_duration = None;
        self.group_end = None;
        self.group_cues.clear();
        self.group_frames = 0;
        self.skip_until = None;
        self.resyncing = true;
    }

    fn scan_for_cluster(&mut self) -> bool {
        if let Some(pos) = ebml::find_bytes(&self.buf, &ebml::CLUSTER_ID_BYTES) {
            if pos > 0 {
                self.buf.drain(..pos);
                self.buf_offset = self.buf_offset.saturating_add(pos as u64);
            }
            true
        } else {
            let keep = self.buf.len().min(ebml::CLUSTER_ID_BYTES.len() - 1);
            let drop = self.buf.len() - keep;
            if drop > 0 {
                self.buf.drain(..drop);
                self.buf_offset = self.buf_offset.saturating_add(drop as u64);
            }
            false
        }
    }

    fn flush_group(&mut self, out: &mut Vec<SubtitleCue>) {
        if self.group_cues.is_empty() {
            self.group_frames = 0;
            return;
        }
        let scale = self.timestamp_scale as f64 / 1_000_000.0;
        let total = self
            .block_duration
            .map(|d| d as f64 * scale)
            .unwrap_or(0.0);
        let per_frame = total / (self.group_frames.max(1) as f64);
        for mut cue in self.group_cues.drain(..) {
            cue.duration = per_frame;
            out.push(cue);
        }
        self.group_frames = 0;
    }

    fn parse(&mut self, out: &mut Vec<SubtitleCue>) {
        loop {
            if let Some(end) = self.skip_until {
                let buf_end = self.buf_offset.saturating_add(self.buf.len() as u64);
                if buf_end <= end {
                    self.buf_offset = buf_end;
                    self.buf.clear();
                    return;
                }
                let drop = (end - self.buf_offset) as usize;
                self.buf.drain(..drop);
                self.buf_offset = end;
                self.skip_until = None;
            }

            if self.state == ParseState::BlockGroup {
                if let Some(end) = self.group_end {
                    if self.buf_offset >= end {
                        self.flush_group(out);
                        self.state = ParseState::Cluster;
                        self.group_end = None;
                        self.block_duration = None;
                    }
                }
            }

            if self.buf.is_empty() {
                return;
            }

            if self.resyncing {
                if !self.scan_for_cluster() {
                    return;
                }
                self.resyncing = false;
                self.state = ParseState::Cluster;
                self.cluster_time = 0;
            }

            let mut pos = 0usize;
            let Some(id) = ebml::read_id(&self.buf, &mut pos) else {
                return;
            };
            let Some((size, unknown)) = ebml::read_size(&self.buf, &mut pos) else {
                return;
            };
            let body_start = self.buf_offset.saturating_add(pos as u64);
            let body_end = body_start.saturating_add(size);

            match id {
                ebml::ID_CLUSTER => {
                    self.flush_group(out);
                    self.buf.drain(..pos);
                    self.buf_offset = body_start;
                    self.state = ParseState::Cluster;
                    self.cluster_time = 0;
                    self.block_duration = None;
                    self.group_end = None;
                }
                ebml::ID_SEGMENT => {
                    self.buf.drain(..pos);
                    self.buf_offset = body_start;
                    self.state = ParseState::Top;
                    self.group_end = None;
                    self.block_duration = None;
                }
                ebml::ID_TIMECODE if self.state == ParseState::Cluster => {
                    if unknown || size > 8 {
                        self.lose_sync(out);
                        continue;
                    }
                    if self.buf.len() < pos + size as usize {
                        return;
                    }
                    let mut p = pos;
                    let Some(tc) = ebml::read_uint(&self.buf, &mut p, size as usize) else {
                        return;
                    };
                    self.cluster_time = tc as i64;
                    self.buf.drain(..pos + size as usize);
                    self.buf_offset = body_end;
                }
                ebml::ID_SIMPLE_BLOCK if self.state == ParseState::Cluster => {
                    if unknown {
                        self.lose_sync(out);
                        continue;
                    }
                    if !self.handle_block(pos, size, false, out) {
                        return;
                    }
                }
                ebml::ID_BLOCK_GROUP if self.state == ParseState::Cluster => {
                    if unknown {
                        self.lose_sync(out);
                        continue;
                    }
                    self.buf.drain(..pos);
                    self.buf_offset = body_start;
                    self.state = ParseState::BlockGroup;
                    self.block_duration = None;
                    self.group_end = Some(body_end);
                }
                ebml::ID_BLOCK if self.state == ParseState::BlockGroup => {
                    if unknown {
                        self.lose_sync(out);
                        continue;
                    }
                    if !self.handle_block(pos, size, true, out) {
                        return;
                    }
                }
                ebml::ID_BLOCK_DURATION if self.state == ParseState::BlockGroup => {
                    if unknown || size > 8 {
                        self.lose_sync(out);
                        continue;
                    }
                    if self.buf.len() < pos + size as usize {
                        return;
                    }
                    let mut p = pos;
                    self.block_duration = ebml::read_uint(&self.buf, &mut p, size as usize);
                    self.buf.drain(..pos + size as usize);
                    self.buf_offset = body_end;
                }
                ebml::ID_EBML
                | ebml::ID_SEEKHEAD
                | ebml::ID_INFO
                | ebml::ID_TRACKS
                | ebml::ID_CUES
                | ebml::ID_CHAPTERS
                | ebml::ID_ATTACHMENTS
                | ebml::ID_TAGS => {
                    self.flush_group(out);
                    self.buf.drain(..pos);
                    self.buf_offset = body_start;
                    self.state = ParseState::Top;
                    self.group_end = None;
                    self.block_duration = None;
                    if unknown {
                        self.lose_sync(out);
                        continue;
                    }
                    if size == 0 {
                        continue;
                    }
                    self.skip_until = Some(body_end);
                }
                _ => {
                    if unknown {
                        self.lose_sync(out);
                        continue;
                    }
                    self.buf.drain(..pos);
                    self.buf_offset = body_start;
                    self.skip_until = Some(body_end);
                }
            }
        }
    }

    fn lose_sync(&mut self, out: &mut Vec<SubtitleCue>) {
        self.flush_group(out);
        self.resyncing = true;
        self.state = ParseState::Cluster;
        self.skip_until = None;
        self.group_end = None;
        self.group_cues.clear();
        self.group_frames = 0;
        self.block_duration = None;
    }

    fn handle_block(
        &mut self,
        header_len: usize,
        size: u64,
        in_group: bool,
        out: &mut Vec<SubtitleCue>,
    ) -> bool {
        if size < 4 || size > MAX_BLOCK_BYTES {
            self.consume(header_len, size);
            return true;
        }
        if self.buf.len() < header_len + 1 {
            return false;
        }

        let mut peek = header_len;
        let Some(track) = ebml::read_vint_value(&self.buf, &mut peek) else {
            self.consume(header_len, size);
            return true;
        };
        if !self.tracks.contains_key(&track) {
            self.consume(header_len, size);
            return true;
        }

        let total = header_len + size as usize;
        if self.buf.len() < total {
            return false;
        }

        let parsed = {
            let body = &self.buf[header_len..total];
            let mut p = 0usize;
            if ebml::read_vint_value(body, &mut p).is_none() || p + 3 > body.len() {
                None
            } else {
                let relative = i16::from_be_bytes([body[p], body[p + 1]]) as i64;
                let flags = body[p + 2];
                p += 3;
                let lacing = (flags >> 1) & 0x03;
                let frames = split_frames(&body[p..], lacing);

                let scale = self.timestamp_scale as f64 / 1_000_000.0;
                let time = (((self.cluster_time + relative) as f64) * scale).max(0.0);
                let duration = self
                    .block_duration
                    .map(|d| d as f64 * scale)
                    .unwrap_or(0.0);
                let frame_count = frames.len().max(1) as f64;
                let compressed = *self.tracks.get(&track).unwrap_or(&false);

                let mut cues = Vec::with_capacity(frames.len());
                for frame in frames {
                    let data = if compressed {
                        inflate(&frame)
                    } else {
                        match String::from_utf8(frame.clone()) {
                            Ok(_) => Some(frame),
                            Err(_) if looks_like_zlib(&frame) => inflate(&frame),
                            Err(_) => None,
                        }
                    };
                    let Some(data) = data else {
                        continue;
                    };
                    let Ok(text) = String::from_utf8(data) else {
                        continue;
                    };
                    let text = text
                        .trim_start_matches('\u{feff}')
                        .trim_end_matches('\0')
                        .to_string();
                    if text.trim().is_empty() {
                        continue;
                    }
                    cues.push(SubtitleCue {
                        track_number: track as i64,
                        text,
                        time,
                        duration: duration / frame_count,
                    });
                }
                Some(cues)
            }
        };

        let Some(cues) = parsed else {
            self.consume(header_len, size);
            return true;
        };

        self.consume(header_len, size);

        if in_group {
            self.group_frames = self.group_frames.saturating_add(cues.len());
            self.group_cues.extend(cues);
        } else {
            out.extend(cues);
        }
        true
    }

    fn consume(&mut self, header_len: usize, size: u64) {
        let body_start = self.buf_offset.saturating_add(header_len as u64);
        let total = header_len.saturating_add(size as usize);
        if total <= self.buf.len() {
            self.buf.drain(..total);
        } else {
            self.buf.clear();
        }
        self.buf_offset = body_start.saturating_add(size);
    }
}

fn looks_like_zlib(data: &[u8]) -> bool {
    data.first()
        .map(|b| (b & 0x0F) == 0x08 && (b >> 4) <= 7)
        .unwrap_or(false)
}

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut decoder = ZlibDecoder::new(data);
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

fn split_frames(payload: &[u8], lacing: u8) -> Vec<Vec<u8>> {
    if payload.is_empty() {
        return vec![];
    }
    match lacing {
        0 => vec![payload.to_vec()],
        1 => {
            let count = payload[0] as usize + 1;
            let mut sizes = Vec::with_capacity(count.saturating_sub(1));
            let mut p = 1usize;
            for _ in 0..count.saturating_sub(1) {
                let mut size = 0usize;
                loop {
                    let Some(&b) = payload.get(p) else {
                        return vec![];
                    };
                    p += 1;
                    size = size.saturating_add(b as usize);
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
            split_with_sizes(payload, p, &sizes)
        }
        2 => {
            let count = payload[0] as usize + 1;
            let rest = &payload[1..];
            if count == 0 || rest.len() % count != 0 {
                return vec![];
            }
            let size = rest.len() / count;
            if size == 0 {
                return vec![];
            }
            rest.chunks(size).map(|c| c.to_vec()).collect()
        }
        3 => {
            let count = payload[0] as usize + 1;
            let mut sizes = Vec::with_capacity(count.saturating_sub(1));
            let mut p = 1usize;
            for _ in 0..count.saturating_sub(1) {
                let Some(size) = ebml::read_vint_value(payload, &mut p) else {
                    return vec![];
                };
                sizes.push(size as usize);
            }
            split_with_sizes(payload, p, &sizes)
        }
        _ => vec![],
    }
}

fn split_with_sizes(payload: &[u8], mut p: usize, sizes: &[usize]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(sizes.len() + 1);
    for &size in sizes {
        if p.saturating_add(size) > payload.len() {
            return vec![];
        }
        out.push(payload[p..p + size].to_vec());
        p += size;
    }
    if p < payload.len() {
        out.push(payload[p..].to_vec());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_size(value: usize) -> Vec<u8> {
        // One-byte size (values < 127).
        assert!(value < 127);
        vec![0x80 | value as u8]
    }

    fn element(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(id);
        out.extend_from_slice(&encode_size(body.len()));
        out.extend_from_slice(body);
        out
    }

    fn cluster(timecode: u64, block: &[u8]) -> Vec<u8> {
        let tc = (timecode as u16).to_be_bytes();
        let mut body = element(&[0xE7], &tc);
        body.extend_from_slice(&element(&[0xA3], block));
        element(&[0x1F, 0x43, 0xB6, 0x75], &body)
    }

    #[test]
    fn extracts_simple_subtitle_block() {
        let mut block = vec![0x81]; // track 1
        block.extend_from_slice(&0i16.to_be_bytes());
        block.push(0); // no lacing
        block.extend_from_slice(b"Hello");

        let data = cluster(0, &block);
        let mut extractor = SubtitleExtractor::new(HashMap::from([(1, false)]), 1_000_000);
        let cues = extractor.push(TapChunk { offset: 0, data });
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello");
        assert_eq!(cues[0].time, 0.0);
    }

    #[test]
    fn applies_timestamp_scale_and_cluster_time() {
        let mut block = vec![0x81];
        block.extend_from_slice(&0i16.to_be_bytes());
        block.push(0);
        block.extend_from_slice(b"Second");

        let data = cluster(1000, &block);
        let mut extractor = SubtitleExtractor::new(HashMap::from([(1, false)]), 1_000_000);
        let cues = extractor.push(TapChunk { offset: 0, data });
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].time, 1000.0);
    }
}
