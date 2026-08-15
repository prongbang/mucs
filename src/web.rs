//! The SvelteKit console, served straight out of the binary.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::Embed)]
#[folder = "web/build"]
struct Assets;

/// Everything the API router didn't claim lands here.
///
/// A path with no extension is a client-side route, so it gets `index.html` and
/// lets the app resolve it — but a missing `.js` or `.css` stays a 404 rather
/// than quietly returning HTML, which is the kind of thing you debug at 3am.
pub async fn handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(file) => serve(path, file),
        None if !path.contains('.') => match Assets::get("index.html") {
            Some(file) => serve("index.html", file),
            None => missing_console(),
        },
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn serve(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();

    // Everything under _app/immutable is content-hashed by Vite, so it can be
    // cached forever. index.html must not be, or a deploy never reaches anyone.
    let cache = if path.starts_with("_app/immutable/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::CACHE_CONTROL, cache),
        ],
        file.data,
    )
        .into_response()
}

fn missing_console() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "console not built into this binary — run `cd web && bun run build`, then rebuild\n",
    )
        .into_response()
}
