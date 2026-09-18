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
        // A miss under /assets/ is a stale hashed request from a superseded
        // build: return 404 rather than falling through to index.html, so a
        // module request never receives HTML (a 200 that dies on the MIME
        // check is what turned a missing asset into a silent white page).
        let fallbacks: &[&str] = if is_asset_path(path) {
            &[]
        } else {
            &["index.html"]
        };
        for asset in std::iter::once(path).chain(fallbacks.iter().copied()) {
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
        if is_asset_path(path) {
            return StatusCode::NOT_FOUND.into_response();
        }
    }

    if let Some(file) = WebAssets::get(path) {
        return file_response(path, file.data.into_owned());
    }
    if !is_asset_path(path)
        && let Some(file) = WebAssets::get("index.html")
    {
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

/// Hashed build output under `/assets/`: its name changes per build, so it is
/// cached forever and a miss is a hard 404 (never the SPA HTML fallback).
fn is_asset_path(path: &str) -> bool {
    path.starts_with("assets/")
}

/// `Cache-Control` policy by asset class: the shell (`index.html`, `sw.js`)
/// must revalidate every load so a redeploy is picked up, while hashed
/// `/assets/*` are immutable and cached for a year.
fn cache_control_for(path: &str) -> &'static str {
    if is_asset_path(path) {
        "public, max-age=31536000, immutable"
    } else if path == "index.html" || path == "sw.js" {
        "no-cache"
    } else {
        // Everything else (manifest, icons, favicon) keeps today's behaviour:
        // no explicit directive.
        ""
    }
}

fn file_response(path: &str, body: Vec<u8>) -> Response {
    let mime = mime_of(path);
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    let cache_control = cache_control_for(path);
    if !cache_control.is_empty() {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        );
    }
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
