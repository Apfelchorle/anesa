//! Local HTTP streaming server for torrent files.
//!
//! Replaces the WebTorrent `createServer()` endpoint from
//! `hayase-torrent-client/index.ts`. Differences / hardening:
//! - Binds loopback only (`127.0.0.1:0`).
//! - Every URL contains a random per-session token, so unrelated local apps or
//!   web pages cannot enumerate/download torrent files from the server.
//! - Only files that exist in the torrent metadata are served; the route takes
//!   an infohash + numeric file id, never a filesystem path.
//! - Supports HTTP Range requests (required for seeking in video players).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};

use super::rqbit::RqbitBackend;

#[derive(Clone)]
struct StreamState {
    backend: Arc<RqbitBackend>,
    token: String,
}

#[derive(Deserialize)]
struct StreamParams {
    token: String,
    hash: String,
    file_id: usize,
    filename: String,
}

#[derive(Deserialize)]
struct AttachmentParams {
    token: String,
    hash: String,
    file_id: usize,
    att_id: usize,
    filename: String,
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, message.to_string()).into_response()
}

fn parse_range(raw: Option<&HeaderValue>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(raw) = raw.and_then(|v| v.to_str().ok()) else {
        return Ok(None);
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return Ok(None);
    };
    // Only support a single range, like rqbit/WebTorrent.
    let Some((start, end)) = spec.split_once('-') else {
        return Err(());
    };
    let start: u64 = if start.is_empty() {
        // Suffix range: bytes=-N
        let suffix: u64 = end.parse().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        len.saturating_sub(suffix)
    } else {
        start.parse().map_err(|_| ())?
    };
    let end: u64 = if end.is_empty() {
        len
    } else {
        end.parse::<u64>()
            .map_err(|_| ())?
            .saturating_add(1)
            .min(len)
    };
    if start >= len || end <= start {
        return Err(());
    }
    Ok(Some((start, end)))
}

async fn stream_handler(
    State(state): State<StreamState>,
    Path(params): Path<StreamParams>,
    headers: HeaderMap,
) -> Response {
    if !constant_time_eq(&params.token, &state.token) {
        return error(StatusCode::FORBIDDEN, "Invalid stream token");
    }

    let filename = percent_decode_str(&params.filename)
        .decode_utf8_lossy()
        .into_owned();

    let (metadata_name, len) = match state.backend.file_meta(&params.hash, params.file_id) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::NOT_FOUND, &e),
    };

    let display_name = if filename.is_empty() || filename == "file" {
        metadata_name
    } else {
        filename
    };

    let mime = mime_guess::from_path(&display_name)
        .first_or_octet_stream()
        .to_string();

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap());
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Filenames are sanitized for the header (no CR/LF/quote injection).
    let safe_name: String = display_name
        .chars()
        .map(|c| if c == '"' || c == '\\' || c == '\r' || c == '\n' { '_' } else { c })
        .collect();
    if let Ok(v) = HeaderValue::from_str(&format!("inline; filename=\"{safe_name}\"")) {
        response_headers.insert(header::CONTENT_DISPOSITION, v);
    }

    match parse_range(headers.get(header::RANGE), len) {
        Ok(Some((start, end))) => {
            let to_take = end - start;
            let stream = match state
                .backend
                .open_file_stream(&params.hash, params.file_id, Some((start, end)))
                .await
            {
                Ok(s) => s,
                Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
            };
            response_headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&to_take.to_string()).unwrap(),
            );
            response_headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end - 1, len)).unwrap(),
            );
            let body = Body::from_stream(ReaderStream::with_capacity(stream, 64 * 1024));
            (StatusCode::PARTIAL_CONTENT, response_headers, body).into_response()
        }
        Ok(None) => {
            let stream = match state
                .backend
                .open_file_stream(&params.hash, params.file_id, None)
                .await
            {
                Ok(s) => s,
                Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
            };
            response_headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&len.to_string()).unwrap(),
            );
            let body = Body::from_stream(ReaderStream::with_capacity(stream, 64 * 1024));
            (StatusCode::OK, response_headers, body).into_response()
        }
        Err(()) => {
            response_headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
            );
            (StatusCode::RANGE_NOT_SATISFIABLE, response_headers).into_response()
        }
    }
}

async fn attachment_handler(
    State(state): State<StreamState>,
    Path(params): Path<AttachmentParams>,
) -> Response {
    if !constant_time_eq(&params.token, &state.token) {
        return error(StatusCode::FORBIDDEN, "Invalid attachment token");
    }

    let Some(att) = state
        .backend
        .get_attachment(&params.hash, params.file_id, params.att_id)
        .await
    else {
        return error(StatusCode::NOT_FOUND, "Attachment not found");
    };

    let filename = percent_decode_str(&params.filename)
        .decode_utf8_lossy()
        .into_owned();
    let display_name = if filename.is_empty() || filename == "file" {
        att.name.clone()
    } else {
        filename
    };

    let mut response_headers = HeaderMap::new();
    let mime = if att.mime.is_empty() {
        mime_guess::from_path(&display_name)
            .first_or_octet_stream()
            .to_string()
    } else {
        att.mime
    };
    response_headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap());
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let safe_name: String = display_name
        .chars()
        .map(|c| {
            if c == '"' || c == '\\' || c == '\r' || c == '\n' {
                '_'
            } else {
                c
            }
        })
        .collect();
    if let Ok(v) = HeaderValue::from_str(&format!("inline; filename=\"{safe_name}\"")) {
        response_headers.insert(header::CONTENT_DISPOSITION, v);
    }
    response_headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&att.data.len().to_string()).unwrap(),
    );

    (StatusCode::OK, response_headers, Body::from(att.data)).into_response()
}

/// Build the axum router. The caller binds the listener (loopback only).
pub fn router(backend: Arc<RqbitBackend>, token: String) -> Router {
    let state = StreamState { backend, token };
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::HEAD])
        .allow_headers(Any)
        .expose_headers([
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::CONTENT_TYPE,
        ]);

    Router::new()
        .route(
            "/stream/{token}/{hash}/{file_id}/{filename}",
            get(stream_handler),
        )
        .route(
            "/attachment/{token}/{hash}/{file_id}/{att_id}/{filename}",
            get(attachment_handler),
        )
        .layer(cors)
        .with_state(state)
}
