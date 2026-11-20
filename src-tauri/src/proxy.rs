//! Controlled fetch proxy for first-party APIs that do not send CORS headers.
//!
//! This is deliberately **not** a general-purpose proxy. Only exact hosts in
//! [`ALLOWED_PROXY_HOSTS`] may be reached, credentials in URLs are rejected,
//! and requests/responses are size-limited. The webview keeps using normal
//! `fetch` for everything else (streaming, assets, extensions, etc.).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, LOCATION, REFERER,
};
use reqwest::{Client, Method, StatusCode, Url};
use serde::{Deserialize, Serialize};

/// Hosts used by the first-party interface that are known to be CORS-blocked.
/// Keep this in sync with `PROXY_HOSTS` in `src-tauri/init-bridge.js` and the
/// extension worker fetch shim in `interface/src/lib/modules/extensions/worker.ts`.
const ALLOWED_PROXY_HOSTS: &[&str] = &[
    "graphql.anilist.co",
    "hayase.ani.zip",
    "api.animethemes.moe",
    "api.aniskip.com",
    "api.trace.moe",
    "api.myanimelist.net",
    "myanimelist.net",
    "api.simkl.com",
    "kitsu.app",
    // First-party extension hosts (trusted extensions only).
    "nekobt.to",
    "feed.animetosho.net",
    "amenzb.moe",
    "aninzb.moe",
    "api.tsukihime.org",
    "storage.tsukihime.org",
];

/// Request headers that are safe and useful to forward. Browser-controlled
/// headers such as `Origin`, `Referer`, `Cookie`, and `Host` are never taken
/// from the webview; host-specific referers are added below.
const ALLOWED_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "authorization",
    "client-id",
    "content-type",
    "simkl-api-key",
    "x-api-key",
];

/// Response headers that are safe to expose to `Response.headers`.
const ALLOWED_RESPONSE_HEADERS: &[&str] = &[
    "cache-control",
    "content-language",
    "content-type",
    "etag",
    "expires",
    "last-modified",
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
];

const MAX_REQUEST_BYTES: usize = 12 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
pub struct ProxyRequest {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body_base64: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProxyResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: HashMap<String, String>,
    pub body_base64: String,
}

fn default_method() -> String {
    "GET".to_string()
}

fn client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            // Redirects are followed manually so every hop is revalidated.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("Hayase/0.1 (+https://github.com/Apfelchorle/anesa)")
            .build()
            .expect("failed to build proxy HTTP client")
    })
}

fn validate_proxy_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| "Invalid proxy URL".to_string())?;
    if url.scheme() != "https" {
        return Err("Proxy only allows https URLs".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Proxy URL must not contain credentials".into());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "Proxy URL is missing a host".to_string())?
        .to_ascii_lowercase();
    if !ALLOWED_PROXY_HOSTS.iter().any(|allowed| *allowed == host) {
        return Err(format!("Proxy host is not allowed: {host}"));
    }
    if url.port().is_some_and(|port| port != 443) {
        return Err("Proxy URL must use the default https port".into());
    }
    Ok(url)
}

/// Credentials are scoped to the API that issued them. A compromised renderer
/// (or a redirected request) must not be able to replay a token against another
/// allowlisted host.
fn header_allowed_for_host(name: &str, host: &str) -> bool {
    match name {
        "authorization" => matches!(
            host,
            "graphql.anilist.co" | "api.myanimelist.net" | "api.simkl.com" | "kitsu.app"
        ),
        "simkl-api-key" | "client-id" => host == "api.simkl.com",
        _ => true,
    }
}

fn build_headers(input: &HashMap<String, String>, host: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in input {
        let name = name.to_ascii_lowercase();
        if !ALLOWED_REQUEST_HEADERS.contains(&name.as_str())
            || !header_allowed_for_host(&name, host)
        {
            continue;
        }
        if value.len() > MAX_HEADER_VALUE_BYTES {
            continue;
        }
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        headers.insert(name, value);
    }

    // The webview's `Response` does not see `content-encoding`, so force
    // identity encoding and hand it plain bytes.
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    // AniZip rejects requests without a Hayase referer. This is a fixed,
    // host-specific header; the webview's own referer is never forwarded.
    if host == "hayase.ani.zip" && !headers.contains_key(REFERER) {
        headers.insert(REFERER, HeaderValue::from_static("https://hayase.watch/"));
    }

    headers
}

fn response_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for name in ALLOWED_RESPONSE_HEADERS {
        if let Some(value) = headers.get(*name) {
            if let Ok(value) = value.to_str() {
                out.insert((*name).to_string(), value.to_string());
            }
        }
    }
    // Some APIs return a JSON body with a generic or missing content type.
    // The webview's `Response` can still parse it; don't guess here.
    out
}

fn decode_body(body_base64: Option<String>) -> Result<Option<Vec<u8>>, String> {
    let Some(encoded) = body_base64 else {
        return Ok(None);
    };
    if encoded.is_empty() {
        return Ok(None);
    }
    if encoded.len() > MAX_REQUEST_BYTES * 2 {
        return Err("Proxy request body is too large".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Proxy request body is not valid base64".to_string())?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err("Proxy request body is too large".into());
    }
    Ok(Some(bytes))
}

fn parse_method(raw: &str) -> Result<Method, String> {
    let method = Method::from_bytes(raw.trim().to_ascii_uppercase().as_bytes())
        .map_err(|_| "Invalid HTTP method".to_string())?;
    match method {
        Method::GET
        | Method::POST
        | Method::PUT
        | Method::PATCH
        | Method::DELETE
        | Method::HEAD
        | Method::OPTIONS => Ok(method),
        _ => Err("HTTP method is not allowed".into()),
    }
}

async fn read_response(response: reqwest::Response) -> Result<ProxyResponse, String> {
    let status = response.status();
    let status_text = status.canonical_reason().unwrap_or("").to_string();
    let headers = response_headers(response.headers());

    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("Proxy response is too large".into());
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("Failed to read proxy response: {error}"))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("Proxy response is too large".into());
    }

    Ok(ProxyResponse {
        status: status.as_u16(),
        status_text,
        headers,
        body_base64: STANDARD.encode(bytes),
    })
}

/// Proxy a single HTTP request through Rust. Invoked only by the trusted
/// `init-bridge.js` fetch shim; extension workers do not get this command.
#[tauri::command]
pub async fn proxy_fetch(request: ProxyRequest) -> Result<ProxyResponse, String> {
    let mut url = validate_proxy_url(&request.url)?;
    let mut method = parse_method(&request.method)?;
    let mut body = decode_body(request.body_base64)?;
    let mut host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let mut headers = build_headers(&request.headers, &host);

    for _ in 0..=MAX_REDIRECTS {
        let mut builder = client().request(method.clone(), url.clone());
        if !headers.is_empty() {
            builder = builder.headers(headers.clone());
        }
        if let Some(body) = &body {
            builder = builder.body(body.clone());
        }

        let response = builder
            .send()
            .await
            .map_err(|error| format!("Proxy request failed: {error}"))?;

        if response.status().is_redirection() {
            if let Some(location) = response.headers().get(LOCATION) {
                let location = location
                    .to_str()
                    .map_err(|_| "Proxy redirect location is invalid".to_string())?;
                let next = url
                    .join(location)
                    .map_err(|_| "Proxy redirect URL is invalid".to_string())?;
                url = validate_proxy_url(next.as_str())?;

                let next_host = url.host_str().unwrap_or_default().to_ascii_lowercase();
                if next_host != host {
                    // Never carry credentials across an origin boundary.
                    headers = build_headers(&request.headers, &next_host);
                    headers.remove(reqwest::header::AUTHORIZATION);
                    headers.remove("simkl-api-key");
                    host = next_host;
                }

                // 301/302/303 traditionally switch to GET; 307/308 preserve it.
                if matches!(
                    response.status(),
                    StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER
                ) && method != Method::GET
                    && method != Method::HEAD
                {
                    method = Method::GET;
                    body = None;
                }
                continue;
            }
        }

        return read_response(response).await;
    }

    Err("Proxy request exceeded the redirect limit".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_known_hosts_only() {
        assert!(validate_proxy_url("https://hayase.ani.zip/v1/episodes?anilist_id=21").is_ok());
        assert!(validate_proxy_url("https://graphql.anilist.co").is_ok());
        assert!(validate_proxy_url("https://example.com/").is_err());
        assert!(validate_proxy_url("https://hayase.ani.zip.evil.example/").is_err());
    }

    #[test]
    fn rejects_non_https_and_credentials() {
        assert!(validate_proxy_url("http://hayase.ani.zip/").is_err());
        assert!(validate_proxy_url("https://user:pass@hayase.ani.zip/").is_err());
        assert!(validate_proxy_url("https://hayase.ani.zip:8443/").is_err());
        assert!(validate_proxy_url("https://hayase.ani.zip:443/").is_ok());
    }

    #[test]
    fn forces_identity_encoding() {
        let headers = build_headers(&HashMap::new(), "hayase.ani.zip");
        assert_eq!(headers.get(ACCEPT_ENCODING).unwrap(), "identity");
    }

    #[test]
    fn filters_forwarded_headers() {
        let mut input = HashMap::new();
        input.insert("Authorization".into(), "Bearer token".into());
        input.insert("Origin".into(), "http://tauri.localhost".into());
        input.insert("Cookie".into(), "secret=1".into());
        let headers = build_headers(&input, "graphql.anilist.co");
        assert!(headers.contains_key("authorization"));
        assert!(!headers.contains_key("origin"));
        assert!(!headers.contains_key("cookie"));
    }

    #[test]
    fn scopes_credentials_to_their_host() {
        let mut input = HashMap::new();
        input.insert("Authorization".into(), "Bearer token".into());
        input.insert("simkl-api-key".into(), "key".into());

        let anizip = build_headers(&input, "hayase.ani.zip");
        assert!(!anizip.contains_key("authorization"));
        assert!(!anizip.contains_key("simkl-api-key"));

        let simkl = build_headers(&input, "api.simkl.com");
        assert!(simkl.contains_key("authorization"));
        assert!(simkl.contains_key("simkl-api-key"));
    }

    #[test]
    fn adds_anizip_referer_only_for_anizip() {
        let headers = build_headers(&HashMap::new(), "hayase.ani.zip");
        assert_eq!(headers.get(REFERER).unwrap(), "https://hayase.watch/");
        let headers = build_headers(&HashMap::new(), "api.aniskip.com");
        assert!(!headers.contains_key(REFERER));
    }

    #[test]
    fn rejects_oversized_body() {
        let encoded = STANDARD.encode(vec![0u8; MAX_REQUEST_BYTES + 1]);
        assert!(decode_body(Some(encoded)).is_err());
    }

    #[tokio::test]
    #[ignore = "live network"]
    async fn proxies_anizip_live() {
        let response = proxy_fetch(ProxyRequest {
            url: "https://hayase.ani.zip/v1/episodes?anilist_id=21".into(),
            method: "GET".into(),
            headers: HashMap::new(),
            body_base64: None,
        })
        .await
        .expect("AniZip request should succeed");

        assert_eq!(response.status, 200);
        let body = STANDARD.decode(response.body_base64).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("\"titles\""));
    }

    #[tokio::test]
    #[ignore = "live network"]
    async fn proxies_anilist_post_live() {
        let payload = serde_json::json!({
            "query": "query { Media(id: 21) { id title { userPreferred } } }"
        })
        .to_string();

        let response = proxy_fetch(ProxyRequest {
            url: "https://graphql.anilist.co".into(),
            method: "POST".into(),
            headers: HashMap::from([("content-type".into(), "application/json".into())]),
            body_base64: Some(STANDARD.encode(payload)),
        })
        .await
        .expect("AniList request should succeed");

        assert_eq!(response.status, 200);
        let body = STANDARD.decode(response.body_base64).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("ONE PIECE"));
    }
}
