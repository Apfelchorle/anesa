//! Event names emitted from the Rust backend to the frontend.
//! The interface currently listens via native integrations; the bridge should
//! translate these Tauri events into `window.native`-compatible callbacks/events.

pub const EVENT_NAVIGATE: &str = "native://navigate"; // payload: { "target": string, "value": string | null }
pub const EVENT_ERROR: &str = "native://error"; // payload: { "source": string, "message": string, "details"?: any }
pub const EVENT_UPDATE_PROGRESS: &str = "native://update-progress"; // payload: number (0-100) or structured progress
pub const EVENT_DOWNLOAD_PROGRESS: &str = "native://download-progress"; // payload: number (0-100)
pub const EVENT_TORRENT_ERROR: &str = "torrent://error";
pub const EVENT_TORRENT_SUBTITLE: &str = "torrent://subtitle"; // payload: { hash, id, subtitle, trackNumber }
