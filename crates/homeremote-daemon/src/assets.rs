//! Serving the built React app out of the binary.
//!
//! Embedding it is what makes the release a single file to double-click, with
//! no web server to install and nothing to point at a directory.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::Embed)]
#[folder = "../../ui/dist"]
struct Assets;

/// Serve an embedded file, falling back to `index.html`.
///
/// The fallback is what lets the UI use client-side routes: a deep link is
/// still the same single page.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return ([(header::CONTENT_TYPE, mime.as_ref())], file.data).into_response();
    }

    // An unknown /api path is a genuine 404; anything else is a UI route.
    if path.starts_with("api/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    match Assets::get("index.html") {
        Some(index) => (
            [(header::CONTENT_TYPE, "text/html")],
            index.data,
        )
            .into_response(),
        // The UI was not built into this binary. Say so plainly rather than
        // serving a blank page.
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html")],
            "<h1>UI not built</h1><p>Run <code>npm install &amp; npm run build</code> \
             in <code>ui/</code>, then rebuild the daemon.</p>",
        )
            .into_response(),
    }
}
