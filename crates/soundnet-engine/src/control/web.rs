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

pub async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(file.data.into_owned()))
                .unwrap()
        }
        None => {
            // SPA fallback — anything that isn't a real asset returns index.html
            // so client-side routing can take over.
            if let Some(idx) = Assets::get("index.html") {
                let mut resp = Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
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
