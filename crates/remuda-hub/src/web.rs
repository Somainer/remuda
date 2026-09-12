//! Embedded Web assets (`rust-embed`) with a 404 fallback.

use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use std::path::PathBuf;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/web-dist"]
struct WebAssets;

/// Serve an embedded or on-disk asset; unknown paths fall back to `index.html`.
pub async fn static_handler(uri: Uri, web_root: Option<PathBuf>) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some(root) = web_root {
        let candidate = root.join(path);
        if candidate.is_file()
            && let Ok(bytes) = std::fs::read(&candidate)
        {
            return file_response(path, bytes);
        }
        let index = root.join("index.html");
        if index.is_file()
            && let Ok(bytes) = std::fs::read(index)
        {
            return file_response("index.html", bytes);
        }
    }

    if let Some(file) = WebAssets::get(path) {
        return file_response(path, file.data.into_owned());
    }
    if let Some(file) = WebAssets::get("index.html") {
        return file_response("index.html", file.data.into_owned());
    }
    (
        StatusCode::NOT_FOUND,
        "web ui is not embedded; build with --features embed-web after just web-build",
    )
        .into_response()
}

fn file_response(path: &str, body: Vec<u8>) -> Response {
    let mime = mime_of(path);
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    response
}

fn mime_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webmanifest" => "application/manifest+json",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
