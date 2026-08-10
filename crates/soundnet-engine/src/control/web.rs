//! Serve the embedded Vite build output for the browser UI. In debug builds
//! rust-embed reads from `web/dist/` at runtime; in release builds the files
//! are baked into the binary.

use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist/"]
struct Assets;

/// `index.html` must be revalidated on every load. Its name never changes,
/// so a cached copy keeps pointing at a bundle the newly deployed binary no
/// longer contains — and the operator sees an unchanged UI and reasonably
/// concludes the deploy failed. `no-cache` still allows a 304, so this costs
/// one conditional request, not a re-download.
const NO_CACHE: &str = "no-cache";

/// Vite fingerprints bundle filenames with a content hash, so a given
/// `/assets/index-CMq-ZV0y.js` can never change meaning — it is safe to
/// cache forever, and a new build simply asks for a different name.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

fn cache_control_for(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        IMMUTABLE
    } else {
        NO_CACHE
    }
}

/// Does this path name a file, as opposed to a client-side route? Used to
/// decide between a 404 and the SPA fallback: `/assets/x.js` and
/// `/favicon.ico` are files, `/routes/abc` is a route.
fn looks_like_asset(path: &str) -> bool {
    path.starts_with("assets/")
        || path
            .rsplit('/')
            .next()
            .is_some_and(|last| last.contains('.'))
}

pub async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, cache_control_for(path))
                .body(Body::from(file.data.into_owned()))
                .unwrap()
        }
        None if looks_like_asset(path) => {
            // A missing file with an extension is a stale reference, not a
            // client-side route. Returning index.html for it (the SPA
            // fallback below) hands the browser HTML where it asked for
            // JavaScript, which fails as a syntax error deep in the console
            // and looks exactly like "the new build didn't take". A 404 says
            // what actually happened.
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from("not found"))
                .unwrap()
        }
        None => {
            // SPA fallback — anything that isn't a real asset returns index.html
            // so client-side routing can take over.
            if let Some(idx) = Assets::get("index.html") {
                let mut resp = Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .header(header::CACHE_CONTROL, NO_CACHE)
                    .body(Body::from(idx.data.into_owned()))
                    .unwrap();
                *resp.status_mut() = StatusCode::OK;
                resp
            } else {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    FALLBACK_HTML,
                )
                    .into_response()
            }
        }
    }
}

/// Rendered when no `web/dist/` has been built (e.g. running the engine
/// standalone before running `npm run build`). Shows a hint plus a link
/// to the JSON state so the operator can verify the engine works headless.
const FALLBACK_HTML: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>SoundNet engine</title>
<style>
body { font: 15px/1.5 system-ui; max-width: 640px; margin: 40px auto; padding: 0 16px;
       background: #111; color: #ddd; }
a { color: #6cf; }
code { background: #222; padding: 2px 6px; border-radius: 4px; }
</style>
<h1>SoundNet engine</h1>
<p>The web UI has not been built yet. Run:</p>
<pre><code>cd web && npm install && npm run build</code></pre>
<p>then restart <code>soundnet-engine</code>. In the meantime the JSON API is live:</p>
<ul>
  <li><a href="/api/state">/api/state</a> — full snapshot</li>
  <li><code>/ws</code> — WebSocket event stream</li>
  <li><code>POST /api/routes</code> — add a route</li>
</ul>
"#;
