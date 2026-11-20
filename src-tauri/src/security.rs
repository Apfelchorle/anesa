//! Security helpers: strict URL allowlisting, path validation, and spawn hardening.
//!
//! Mirrors and hardens behavior from `hayase-electron/src/main/ipc.ts`:
//! - Electron used `url.startsWith(whitelisted)` which is vulnerable to prefix confusion.
//! - Player spawning only accepted `http://localhost:` URLs; keep that, but parse strictly.

use std::path::{Component, Path, PathBuf};
use url::{Host, Url};

/// Origins permitted for `shell.openExternal`-style navigation.
/// Keep in sync with interface expectations; prefer exact hosts over prefixes.
const ALLOWED_EXTERNAL_HOSTS: &[&str] = &[
    "anilist.co",
    "github.com",
    "myanimelist.net",
    "miru.watch",
    "hayase.app",
    "hayase.watch",
    "thewiki.moe",
    "kitsu.app",
];

/// Subpaths that are explicitly allowed on multi-purpose hosts (e.g. GitHub sponsors only).
fn is_allowed_path(host: &str, path: &str) -> bool {
    match host {
        "github.com" => path.starts_with("/sponsors/ThaUnknown"),
        // AniList/MAL/Kitsu links are intentionally broad for auth/profile flows.
        "anilist.co" | "myanimelist.net" | "kitsu.app" => true,
        // Project sites: allow any path, but only over https.
        "miru.watch" | "hayase.app" | "hayase.watch" | "thewiki.moe" => true,
        _ => false,
    }
}

/// Strict external URL validation.
/// - Requires `https` (no `http`, `file`, `javascript`, custom schemes).
/// - Requires an exact host match (subdomains rejected unless listed).
/// - Rejects credentials in URL.
/// - Enforces path rules for shared hosts.
pub fn validate_external_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "Invalid URL".to_string())?;

    if url.scheme() != "https" {
        return Err("Only https URLs may be opened externally".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with credentials are not allowed".into());
    }

    let host = match url.host() {
        Some(Host::Domain(d)) => d.to_ascii_lowercase(),
        _ => return Err("Only domain hosts are allowed".into()),
    };

    if !ALLOWED_EXTERNAL_HOSTS.iter().any(|h| *h == host) {
        return Err(format!("Host not allowed: {host}"));
    }
    if !is_allowed_path(&host, url.path()) {
        return Err("Path not allowed for this host".into());
    }

    Ok(url)
}

/// Validate a player-spawn URL: must be plain HTTP to loopback only.
/// Matches Electron behavior (`url.startsWith('http://localhost:')`) but parses the URL.
pub fn validate_player_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "Invalid player URL".to_string())?;

    if url.scheme() != "http" {
        return Err("Player URL must use http".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Player URL must not contain credentials".into());
    }

    let host_ok = match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if !host_ok {
        return Err("Player URL must target localhost/loopback".into());
    }
    // Require an explicit port to avoid ambiguous defaults.
    if url.port().is_none() {
        return Err("Player URL must include a port".into());
    }

    Ok(url)
}

/// Validate a user-selected download directory.
/// - Must be absolute.
/// - Must not be a filesystem root (mirrors Electron's root-directory guard).
/// - Must exist and be a directory.
/// - Rejects traversal components and NUL bytes.
pub fn validate_download_dir(raw: &str) -> Result<PathBuf, String> {
    if raw.contains('\0') {
        return Err("Invalid path".into());
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err("Download path must be absolute".into());
    }
    if path.parent().is_none() {
        return Err("Cannot use a filesystem root as download location".into());
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Path traversal is not allowed".into());
    }
    let meta = std::fs::metadata(&path).map_err(|_| "Download path does not exist".to_string())?;
    if !meta.is_dir() {
        return Err("Download path must be a directory".into());
    }
    Ok(path)
}

/// Validate an executable path selected for external playback.
/// - Absolute, exists, is a file (or macOS `.app` bundle directory).
/// - Rejects traversal/NUL.
pub fn validate_player_executable(raw: &str) -> Result<PathBuf, String> {
    if raw.contains('\0') {
        return Err("Invalid executable path".into());
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err("Player path must be absolute".into());
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("Path traversal is not allowed".into());
    }
    let meta = std::fs::metadata(&path).map_err(|_| "Player executable does not exist".to_string())?;

    #[cfg(target_os = "macos")]
    {
        if meta.is_dir() && path.extension().and_then(|e| e.to_str()) == Some("app") {
            return Ok(path);
        }
    }

    if !meta.is_file() {
        return Err("Player path must be a file".into());
    }
    Ok(path)
}

/// Resolve the actual executable inside a macOS `.app` bundle, mirroring Electron behavior.
pub fn resolve_macos_app(path: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if path.extension().and_then(|e| e.to_str()) == Some("app") {
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            return path.join("Contents").join("MacOS").join(name);
        }
    }
    path.to_path_buf()
}

/// Validate a raw infohash (40 or 64 hex chars).
pub fn validate_infohash(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    let is_hex = trimmed.len() == 40 || trimmed.len() == 64;
    if is_hex && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("Invalid infohash format".into())
    }
}

/// Reject unexpected torrent identifiers before they reach the backend.
/// Accepts 40/64-char hex infohashes or magnet URIs (backend performs deeper validation).
pub fn validate_torrent_identifier(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Empty torrent identifier".into());
    }
    if trimmed.starts_with("magnet:?") {
        // Ensure it has an xt=urn:btih parameter; do not fully parse here.
        if !trimmed.to_ascii_lowercase().contains("xt=urn:btih:") {
            return Err("Magnet URI missing btih infohash".into());
        }
        return Ok(());
    }
    validate_infohash(trimmed)
}

/// Validate a remote URL used for NZB/webseed fetching (SSRF defense).
/// - Requires `http` or `https`.
/// - Rejects credentials.
/// - Blocks literal loopback/private/link-local IP hosts.
/// - Hostname-based private resolution checks belong in the fetcher (DNS rebinding defense).
pub fn validate_remote_fetch_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "Invalid URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only http(s) URLs may be fetched".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with credentials are not allowed".into());
    }

    if let Some(host) = url.host() {
        match host {
            Host::Ipv4(ip) => {
                if ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() {
                    return Err("Private/loopback addresses are not allowed".into());
                }
            }
            Host::Ipv6(ip) => {
                if ip.is_loopback() || ip.is_unspecified() {
                    return Err("Private/loopback addresses are not allowed".into());
                }
            }
            Host::Domain(_) => {}
        }
    }

    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_prefix_confusion() {
        assert!(validate_external_url("https://hayase.app.evil.example/").is_err());
        assert!(validate_external_url("https://hayase.app/").is_ok());
    }

    #[test]
    fn rejects_non_https_and_credentials() {
        assert!(validate_external_url("http://hayase.app/").is_err());
        assert!(validate_external_url("https://user:pass@hayase.app/").is_err());
        assert!(validate_external_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn github_only_sponsors_path() {
        assert!(validate_external_url("https://github.com/sponsors/ThaUnknown/").is_ok());
        assert!(validate_external_url("https://github.com/evil/repo").is_err());
    }

    #[test]
    fn player_url_loopback_only() {
        assert!(validate_player_url("http://localhost:7344/video").is_ok());
        assert!(validate_player_url("http://127.0.0.1:7344/video").is_ok());
        assert!(validate_player_url("http://evil.example:7344/video").is_err());
        assert!(validate_player_url("https://localhost:7344/video").is_err());
        assert!(validate_player_url("http://localhost/video").is_err());
    }

    #[test]
    fn torrent_identifier_shapes() {
        assert!(validate_torrent_identifier("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(validate_torrent_identifier("magnet:?xt=urn:btih:abc&dn=x").is_ok());
        assert!(validate_torrent_identifier("https://example.com/a.torrent").is_err());
        assert!(validate_torrent_identifier("nope").is_err());
    }

    #[test]
    fn remote_fetch_blocks_private_hosts() {
        assert!(validate_remote_fetch_url("https://example.com/a.nzb").is_ok());
        assert!(validate_remote_fetch_url("http://127.0.0.1:8080/a.nzb").is_err());
        assert!(validate_remote_fetch_url("http://192.168.1.10/a.nzb").is_err());
        assert!(validate_remote_fetch_url("file:///etc/passwd").is_err());
    }
}
