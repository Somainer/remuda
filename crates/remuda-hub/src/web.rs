//! Embedded Web assets (`rust-embed`) with a 404 fallback.

use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use std::path::{Path, PathBuf};

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/web-dist"]
struct WebAssets;

/// Apply to the entire router, including API errors and WebSocket upgrades.
pub async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    // React layouts and xterm generate inline styles; executable scripts remain
    // restricted to same-origin assets (no inline scripts or eval).
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self' blob:; frame-ancestors 'none'; object-src 'none'; base-uri 'none'; form-action 'self'",
    ));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

/// Serve an embedded or on-disk asset; unknown paths fall back to `index.html`.
pub async fn static_handler(uri: Uri, web_root: Option<PathBuf>) -> Response {
    let raw = uri.path().strip_prefix('/').unwrap_or(uri.path());
    if !safe_relative_path(raw) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some(root) = web_root {
        let Ok(root) = root.canonicalize() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        for asset in [path, "index.html"] {
            if let Ok(candidate) = root.join(asset).canonicalize() {
                // Check the resolved path, including the SPA fallback, so an
                // in-root symlink cannot expose a file outside the web root.
                if !candidate.starts_with(&root) {
                    return StatusCode::NOT_FOUND.into_response();
                }
                if candidate.is_file()
                    && let Ok(bytes) = std::fs::read(candidate)
                {
                    return file_response(asset, bytes);
                }
            }
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

fn safe_relative_path(path: &str) -> bool {
    !path.contains(['\0', '\\'])
        && !path.starts_with('/')
        && !Path::new(path).is_absolute()
        && !path.split('/').any(|segment| segment == "..")
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
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "webmanifest" => "application/manifest+json",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::safe_relative_path;

    #[test]
    fn rejects_unsafe_relative_paths() {
        for path in [
            "../secret",
            "assets/../secret",
            "a/..",
            "/etc/passwd",
            "a\0b",
            "a\\b",
            "C:\\secret",
        ] {
            assert!(!safe_relative_path(path), "{path:?}");
        }
        for path in ["", "index.html", "assets/main.js", "a..b"] {
            assert!(safe_relative_path(path), "{path:?}");
        }
    }
}
