//! Minimal EBML primitives shared by the metadata probe and the live subtitle
//! extractor.
//!
//! This is deliberately small: we only need to walk element headers, read
//! unsigned integers and find the start of a cluster after a seek gap.

// Segment-level / global elements.
pub const ID_EBML: u32 = 0x1A45_DFA3;
pub const ID_SEGMENT: u32 = 0x1853_8067;
pub const ID_SEEKHEAD: u32 = 0x114D_9B74;
pub const ID_SEEK: u32 = 0x4DBB;
pub const ID_SEEK_ID: u32 = 0x53AB;
pub const ID_SEEK_POSITION: u32 = 0x53AC;
pub const ID_INFO: u32 = 0x1549_A966;
pub const ID_TIMESTAMP_SCALE: u32 = 0x2AD7_B1;
pub const ID_TRACKS: u32 = 0x1654_AE6B;
pub const ID_CUES: u32 = 0x1C53_BB6B;
pub const ID_CHAPTERS: u32 = 0x1043_A770;
pub const ID_ATTACHMENTS: u32 = 0x1941_A469;
pub const ID_TAGS: u32 = 0x1254_C367;

// Cluster / block elements.
pub const ID_CLUSTER: u32 = 0x1F43_B675;
pub const ID_TIMECODE: u32 = 0xE7;
pub const ID_SIMPLE_BLOCK: u32 = 0xA3;
pub const ID_BLOCK_GROUP: u32 = 0xA0;
pub const ID_BLOCK: u32 = 0xA1;
pub const ID_BLOCK_DURATION: u32 = 0x9B;

pub const CLUSTER_ID_BYTES: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];

/// Read an EBML element ID (1-4 bytes) and advance `pos`.
///
/// Unlike sizes/values, the length marker bits are part of an element ID
/// (e.g. Cluster is `0x1F43B675`), so the bytes are concatenated unchanged.
pub fn read_id(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let first = *buf.get(*pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || *pos + len > buf.len() {
        return None;
    }
    let mut value = first as u32;
    for i in 1..len {
        value = (value << 8) | buf[*pos + i] as u32;
    }
    *pos += len;
    Some(value)
}

/// Read an EBML size (1-8 bytes). Returns `(value, is_unknown)`.
pub fn read_size(buf: &[u8], pos: &mut usize) -> Option<(u64, bool)> {
    let first = *buf.get(*pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || *pos + len > buf.len() {
        return None;
    }
    let mask = 0xFFu64 >> len;
    let mut value = (first as u64) & mask;
    let mut all_ones = value == mask;
    for i in 1..len {
        let b = buf[*pos + i];
        value = (value << 8) | b as u64;
        all_ones &= b == 0xFF;
    }
    *pos += len;
    Some((value, all_ones))
}

/// Read a big-endian unsigned integer of `len` bytes.
pub fn read_uint(buf: &[u8], pos: &mut usize, len: usize) -> Option<u64> {
    if len == 0 || len > 8 || *pos + len > buf.len() {
        return None;
    }
    let mut value = 0u64;
    for i in 0..len {
        value = (value << 8) | buf[*pos + i] as u64;
    }
    *pos += len;
    Some(value)
}

/// Read an EBML variable-length integer value (track number, lacing sizes).
pub fn read_vint_value(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let first = *buf.get(*pos)?;
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || *pos + len > buf.len() {
        return None;
    }
    let mut value = (first as u64) & (0xFFu64 >> len);
    for i in 1..len {
        value = (value << 8) | buf[*pos + i] as u64;
    }
    *pos += len;
    Some(value)
}

pub fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
