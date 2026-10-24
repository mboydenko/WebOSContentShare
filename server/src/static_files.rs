//! The webOS client is also served at /app/ so it can be opened in the TV's
//! built-in browser (or a desktop browser) without installing the app.

use std::net::SocketAddr;

use axum::{
    Router,
    extract::{ConnectInfo, Path},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};

use crate::Shared;

macro_rules! client_files {
    ($($path:literal => $mime:literal),* $(,)?) => {
        &[$(($path, $mime, include_bytes!(concat!("../../webos-client/", $path)) as &[u8])),*]
    };
}

static CLIENT: &[(&str, &str, &[u8])] = client_files! {
    "index.html" => "text/html; charset=utf-8",
    "css/app.css" => "text/css; charset=utf-8",
    "js/api.js" => "application/javascript; charset=utf-8",
    "js/nav.js" => "application/javascript; charset=utf-8",
    "js/servers.js" => "application/javascript; charset=utf-8",
    "js/browser.js" => "application/javascript; charset=utf-8",
    "js/player.js" => "application/javascript; charset=utf-8",
    "js/viewer.js" => "application/javascript; charset=utf-8",
    "js/app.js" => "application/javascript; charset=utf-8",
    "js/hls.min.js" => "application/javascript; charset=utf-8",
    "icon.png" => "image/png",
    "largeIcon.png" => "image/png",
};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/", get(index))
        .route("/app", get(|| async { Redirect::permanent("/app/") }))
        .route("/app/", get(|| async { client_file("index.html") }))
        .route("/app/{*file}", get(|Path(file): Path<String>| async move { client_file(&file) }))
}

async fn index(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Redirect {
    if addr.ip().is_loopback() { Redirect::temporary("/admin") } else { Redirect::temporary("/app/") }
}

fn client_file(path: &str) -> Response {
    match CLIENT.iter().find(|(p, _, _)| *p == path) {
        Some((_, mime, data)) => {
            let mut resp = data.into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            resp
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
