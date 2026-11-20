use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_opener::OpenerExt;
use url::Url;

use crate::events::EVENT_NAVIGATE;

#[derive(Debug, Clone, Serialize)]
pub struct NavigatePayload {
    pub target: String,
    pub value: Option<String>,
}

/// Strictly parse and handle a `hayase://...` deep link.
///
/// Mirrors Electron behavior as closely as practical:
/// - raw form `hayase://<target>/<value...>` (value may include `?`/`#` for OAuth callbacks)
/// - `donate` opens the sponsors page externally; `devtools` is debug-only
/// - all valid parsed targets are forwarded to the renderer as navigate events
///
/// Security: reject credentials, cap length, normalize target, and never execute shell commands.
pub fn handle_deep_link<R: Runtime>(app: &AppHandle<R>, raw: &str) {
    if raw.len() > 4096 {
        log::warn!("Ignoring oversized deep link");
        return;
    }
    let parsed = match Url::parse(raw) {
        Ok(u) => u,
        Err(e) => {
            log::warn!("Failed to parse deep link: {e}");
            return;
        }
    };

    if parsed.scheme() != "hayase" {
        return;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        log::warn!("Ignoring deep link with credentials");
        return;
    }

    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();

    // Reconstruct the remainder (path + query + fragment) to preserve OAuth payloads
    // like `hayase://authorize/#access_token=...` or `hayase://authorize/?code=...`.
    let mut remainder = parsed.path().to_string();
    if let Some(q) = parsed.query() {
        remainder.push('?');
        remainder.push_str(q);
    }
    if let Some(f) = parsed.fragment() {
        remainder.push('#');
        remainder.push_str(f);
    }
    let remainder = remainder.trim_start_matches('/').to_string();

    let (target, value) = if !host.is_empty() {
        if remainder.is_empty() {
            (host, None)
        } else {
            (host, Some(remainder))
        }
    } else if remainder.is_empty() {
        return;
    } else {
        // Fallback: hayase:///target/value
        let mut parts = remainder.splitn(2, '/');
        let target = parts.next().unwrap_or_default().to_string();
        let value = parts.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
        (target, value)
    };

    let target = target.to_ascii_lowercase();
    if target.is_empty() {
        return;
    }

    match target.as_str() {
        "donate" => {
            let _ = app
                .opener()
                .open_url("https://github.com/sponsors/ThaUnknown/", None::<String>);
        }
        "devtools" => {
            if let Some(window) = app.get_webview_window("main") {
                window.open_devtools();
            }
        }
        _ => {}
    }

    // Electron forwards all parsed protocol navigations to the renderer.
    let _ = app.emit(EVENT_NAVIGATE, NavigatePayload { target, value });
}
