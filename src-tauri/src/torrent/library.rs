//! Persistent library + torrent metadata cache.
//!
//! Electron reference: `hayase-torrent-client/filesystem/store.ts` (per-torrent
//! bencoded files on disk). This implementation uses a single atomically-written
//! JSON index plus one `.torrent` file per hash, which is simpler to audit and
//! avoids custom binary formats.
//!
//! Security: every hash is validated as 40/64 hex before being used in a path,
//! so a malicious renderer cannot escape the cache directory.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::LibraryEntry;

fn validate_hash(hash: &str) -> Result<(), String> {
    let ok = (hash.len() == 40 || hash.len() == 64) && hash.bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err("Invalid infohash".into())
    }
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(data).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub struct LibraryStore {
    path: PathBuf,
    entries: Mutex<BTreeMap<String, LibraryEntry>>,
}

impl LibraryStore {
    pub fn load(path: PathBuf) -> Self {
        let entries = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<BTreeMap<String, LibraryEntry>>(&raw).unwrap_or_else(
                |e| {
                    log::warn!("Failed to parse library at {}: {e}", path.display());
                    BTreeMap::new()
                },
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                log::warn!("Failed to read library at {}: {e}", path.display());
                BTreeMap::new()
            }
        };
        Self {
            path,
            entries: Mutex::new(entries),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, LibraryEntry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, entries: &BTreeMap<String, LibraryEntry>) {
        match serde_json::to_vec_pretty(entries) {
            Ok(json) => {
                if let Err(e) = atomic_write(&self.path, &json) {
                    log::error!("Failed to persist library to {}: {e}", self.path.display());
                }
            }
            Err(e) => log::error!("Failed to serialize library: {e}"),
        }
    }

    pub fn upsert(&self, entry: LibraryEntry) {
        let mut guard = self.lock();
        guard.insert(entry.hash.clone(), entry);
        self.persist(&guard);
    }

    pub fn get(&self, hash: &str) -> Option<LibraryEntry> {
        self.lock().get(hash).cloned()
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.lock().contains_key(hash)
    }

    pub fn list(&self) -> Vec<LibraryEntry> {
        self.lock().values().cloned().collect()
    }

    pub fn remove(&self, hash: &str) {
        let mut guard = self.lock();
        if guard.remove(hash).is_some() {
            self.persist(&guard);
        }
    }

    /// Update progress/size for an existing entry without changing media metadata.
    pub fn update_progress(&self, hash: &str, progress: f64, size: u64) {
        let mut guard = self.lock();
        if let Some(entry) = guard.get_mut(hash) {
            entry.progress = progress;
            entry.size = size;
            self.persist(&guard);
        }
    }

    /// Update progress/size/date for rescan-like refreshes.
    pub fn update_rescan_state(&self, hash: &str, progress: f64, size: u64, date: i64) {
        let mut guard = self.lock();
        if let Some(entry) = guard.get_mut(hash) {
            entry.progress = progress;
            entry.size = size;
            entry.date = date;
            self.persist(&guard);
        }
    }
}

/// `.torrent` metadata cache used by `rescan_torrents`.
pub struct TorrentCache {
    dir: PathBuf,
}

impl TorrentCache {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self { dir }
    }

    pub fn dir_path(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, hash: &str) -> Result<PathBuf, String> {
        validate_hash(hash)?;
        Ok(self.dir.join(format!("{hash}.torrent")))
    }

    pub fn write(&self, hash: &str, bytes: &[u8]) -> Result<(), String> {
        atomic_write(&self.path_for(hash)?, bytes)
    }

    pub fn read(&self, hash: &str) -> Option<Vec<u8>> {
        fs::read(self.path_for(hash).ok()?).ok()
    }

    pub fn remove(&self, hash: &str) {
        if let Ok(path) = self.path_for(hash) {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_hashes() {
        let cache = TorrentCache::new(std::env::temp_dir().join("hayase-cache-test"));
        assert!(cache.write("../../etc/passwd", b"x").is_err());
        assert!(cache.write("nothex", b"x").is_err());
        assert!(cache.write("0123456789abcdef0123456789abcdef01234567", b"x").is_ok());
    }

    #[test]
    fn library_roundtrip() {
        let dir = std::env::temp_dir().join(format!("hayase-library-test-{}", std::process::id()));
        let path = dir.join("library.json");
        let store = LibraryStore::load(path.clone());
        store.upsert(LibraryEntry {
            media_id: 1,
            episode: 2,
            files: 1,
            hash: "0123456789abcdef0123456789abcdef01234567".into(),
            progress: 0.5,
            date: 1,
            size: 10,
            name: "x".into(),
            background: false,
        });
        let reloaded = LibraryStore::load(path);
        assert_eq!(reloaded.list().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
