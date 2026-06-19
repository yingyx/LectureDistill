//! SPA serving handlers: embedded assets (rust-embed) + filesystem fallback.

use axum::{
    body::Body,
    extract::Path,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;
use std::fs;
use std::path::Path as FsPath;

// ---------------------------------------------------------------------------
// Embedded frontend assets (production)
// ---------------------------------------------------------------------------

/// Embedded React/Vite build output.
///
/// When `web/dist/` exists at compile time, `rust-embed` captures it.
/// When it does not, the struct is still valid but `get()` returns `None`
/// for every path, and the server falls back to filesystem serving.
#[derive(RustEmbed)]
#[folder = "web/dist/"]
pub struct WebAssets;

// ---------------------------------------------------------------------------
// SPA serving (embedded via rust-embed, filesystem fallback)
// ---------------------------------------------------------------------------

/// Guess the MIME type from a file extension.
pub(crate) fn mime_from_path(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "eot" => "application/vnd.ms-fontobject",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serve an asset from the SPA build (embedded or filesystem).
pub(crate) async fn serve_spa_asset(Path(path): Path<String>) -> Response {
    let asset_path = path.trim_start_matches('/');
    let embedded_path = if asset_path.starts_with("assets/") {
        asset_path.to_string()
    } else {
        format!("assets/{asset_path}")
    };

    // 1. Try embedded assets.
    if let Some(asset) = WebAssets::get(&embedded_path) {
        let mime = mime_from_path(&embedded_path);
        return Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "public, max-age=3600")
            .body(Body::from(asset.data.into_owned()))
            .unwrap();
    }

    // 2. Fall back to filesystem.
    let fs_path = FsPath::new("web/dist").join(&embedded_path);
    if fs_path.exists() && fs_path.is_file() {
        match fs::read(&fs_path) {
            Ok(data) => {
                let mime = mime_from_path(&embedded_path);
                return Response::builder()
                    .header(header::CONTENT_TYPE, mime)
                    .header(header::CACHE_CONTROL, "public, max-age=3600")
                    .body(Body::from(data))
                    .unwrap();
            }
            Err(_) => {}
        }
    }

    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// Serve the SPA index.html (embedded or filesystem), or a startup-error page
/// if nothing is available.
pub(crate) async fn serve_spa_index() -> Response {
    serve_index_html()
}

/// Catch-all fallback for SPA client-side routing.
///
/// If the path looks like an API/action route that wasn't matched, return 404.
/// Otherwise serve `index.html` so React Router can handle it.
pub(crate) async fn serve_spa_fallback(_uri: Uri, Path(path): Path<String>) -> Response {
    let path = path.trim_start_matches('/');

    // Known non-SPA paths that should 404 instead of serving index.html.
    if path.is_empty() {
        return serve_index_html();
    }

    // If the path has a file extension, try to serve it as a static asset.
    if path.contains('.') {
        return serve_spa_asset(Path(path.to_string())).await;
    }

    // Otherwise, serve index.html for client-side routing.
    serve_index_html()
}

/// Try embedded, then filesystem, then show a clear error page.
pub(crate) fn serve_index_html() -> Response {
    // 1. Try embedded.
    if let Some(asset) = WebAssets::get("index.html") {
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(asset.data.into_owned()))
            .unwrap();
    }

    // 2. Try filesystem.
    let fs_path = FsPath::new("web/dist/index.html");
    if fs_path.exists() {
        match fs::read_to_string(&fs_path) {
            Ok(content) => {
                return Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .body(Body::from(content))
                    .unwrap();
            }
            Err(_) => {}
        }
    }

    // 3. Neither is available — show a clear error page.
    let version = env!("CARGO_PKG_VERSION");
    let body = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>lecture-distill — GUI not built</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 640px; margin: 4rem auto; padding: 0 1.5rem; line-height: 1.6; color: #212529; }}
  h1 {{ font-size: 1.25rem; }}
  code {{ background: #f1f3f5; padding: 0.15em 0.4em; border-radius: 4px; font-size: 0.9em; }}
  pre {{ background: #f8f9fa; border: 1px solid #dee2e6; border-radius: 6px; padding: 1rem; overflow-x: auto; }}
  .badge {{ display: inline-block; padding: 0.2em 0.6em; border-radius: 9999px; font-size: 0.75rem; font-weight: 500; background: #fee2e2; color: #dc2626; }}
</style>
</head>
<body>
<h1>lecture-distill v{version}</h1>
<p><span class="badge">GUI Not Built</span></p>
<p>The React frontend has not been built or embedded.</p>
<p>To build the GUI:</p>
<pre>cd web/
npm install
npm run build
cd ..
cargo build --release</pre>
<p>After building, restart the server. The dashboard will then be served at <a href="/">/</a>.</p>
<p><small>If you are a developer, you can also run the Vite dev server separately:
<code>cd web/ && npm run dev</code> and use it as a standalone frontend during development.</small></p>
</body>
</html>"#,
        version = version,
    );
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}
