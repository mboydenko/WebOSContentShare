//! Server control panel: manage media roots and settings. Reachable only from
//! the machine itself (loopback address and a localhost Host header).

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Path as AxPath, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Shared,
    config::{Root, TranscodeSettings, TvProfile},
    ffmpeg::Tools,
    library,
    util::{self, ApiError, ApiResult},
};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/admin", get(page))
        .route("/api/admin/state", get(state))
        .route("/api/admin/roots", post(add_root))
        .route("/api/admin/roots/{id}", delete(remove_root))
        .route("/api/admin/fs", get(fs_list))
        .route("/api/admin/settings", post(save_settings))
        .route("/api/admin/redetect", post(redetect))
        .route("/api/admin/sessions/{id}", delete(kill_session))
        .route_layer(middleware::from_fn(local_only))
}

fn host_is_local(host: &str) -> bool {
    let name = if host.starts_with('[') {
        host.split(']').next().map(|h| &h[1..]).unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

async fn local_only(ConnectInfo(addr): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    // The Host check also defeats DNS-rebinding from web pages open in a local browser.
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_none_or(host_is_local);
    if !addr.ip().is_loopback() || !host_ok {
        return (
            StatusCode::FORBIDDEN,
            "Панель управления доступна только на компьютере-сервере: http://localhost",
        )
            .into_response();
    }
    next.run(req).await
}

async fn page() -> Html<&'static str> {
    Html(include_str!("../admin/index.html"))
}

async fn state(State(s): State<Shared>) -> Json<Value> {
    let cfg = s.cfg();
    let urls: Vec<String> = util::local_ip().map(|ip| format!("http://{ip}:{}", cfg.port)).into_iter().collect();
    let roots: Vec<Value> = cfg
        .roots
        .iter()
        .map(|r| json!({ "id": r.id, "name": r.name, "path": r.path, "exists": r.path.is_dir() }))
        .collect();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "server_name": cfg.server_name,
        "port": cfg.port,
        "urls": urls,
        "config_path": s.config_path,
        "roots": roots,
        "tv": cfg.tv,
        "transcode": cfg.transcode,
        "ffmpeg_path": cfg.ffmpeg_path,
        "ffprobe_path": cfg.ffprobe_path,
        "tools": s.tools(),
        "caps": s.caps(),
        "sessions": s.sessions.list(),
    }))
}

#[derive(Deserialize)]
struct AddRoot {
    path: String,
    #[serde(default)]
    name: String,
}

async fn add_root(State(s): State<Shared>, Json(b): Json<AddRoot>) -> ApiResult<Json<Value>> {
    let path = PathBuf::from(b.path.trim());
    if !path.is_dir() {
        return Err(ApiError::bad_request("Папка не существует"));
    }
    let canon = std::fs::canonicalize(&path)?;
    let name = match b.name.trim() {
        "" => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().trim_end_matches(['\\', '/']).to_string()),
        n => n.to_string(),
    };
    let mut cfg = s.config.write().unwrap();
    if cfg.roots.iter().any(|r| std::fs::canonicalize(&r.path).ok().as_ref() == Some(&canon)) {
        return Err(ApiError::bad_request("Эта папка уже добавлена"));
    }
    let id = util::random_id()[..8].to_string();
    cfg.roots.push(Root { id: id.clone(), name, path });
    cfg.save(&s.config_path)?;
    Ok(Json(json!({ "id": id })))
}

async fn remove_root(State(s): State<Shared>, AxPath(id): AxPath<String>) -> ApiResult<StatusCode> {
    let mut cfg = s.config.write().unwrap();
    let before = cfg.roots.len();
    cfg.roots.retain(|r| r.id != id);
    if cfg.roots.len() == before {
        return Err(ApiError::not_found("Папка не найдена"));
    }
    cfg.save(&s.config_path)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct FsQuery {
    #[serde(default)]
    path: String,
}

/// Directory picker for adding roots. Empty path = drives (Windows) or common places.
async fn fs_list(Query(q): Query<FsQuery>) -> ApiResult<Json<Value>> {
    let entry = |name: String, path: &Path| json!({ "name": name, "path": path });
    if q.path.trim().is_empty() {
        let mut dirs = Vec::new();
        if let Some(home) = dirs::home_dir() {
            dirs.push(entry(format!("Домашняя папка ({})", home.display()), &home));
        }
        for (label, dir) in [("Видео", dirs::video_dir()), ("Музыка", dirs::audio_dir()), ("Изображения", dirs::picture_dir())] {
            if let Some(d) = dir.filter(|d| d.is_dir()) {
                dirs.push(entry(label.to_string(), &d));
            }
        }
        if cfg!(windows) {
            for letter in b'A'..=b'Z' {
                let d = PathBuf::from(format!("{}:\\", letter as char));
                if d.exists() {
                    dirs.push(entry(format!("Диск {}:", letter as char), &d));
                }
            }
        } else {
            for d in ["/", "/media", "/mnt", "/srv"] {
                if Path::new(d).is_dir() {
                    dirs.push(entry(d.to_string(), Path::new(d)));
                }
            }
        }
        return Ok(Json(json!({ "path": "", "parent": null, "dirs": dirs })));
    }

    let path = PathBuf::from(q.path.trim());
    let list_path = path.clone();
    let mut dirs: Vec<(String, PathBuf)> = tokio::task::spawn_blocking(move || {
        std::fs::read_dir(&list_path).map(|rd| {
            rd.flatten()
                .filter(|de| de.path().is_dir())
                .map(|de| (de.file_name().to_string_lossy().into_owned(), de.path()))
                .filter(|(n, _)| !n.starts_with('.') && !n.starts_with('$') && n != "System Volume Information")
                .collect()
        })
    })
    .await?
    .map_err(|e| ApiError::bad_request(format!("Не удалось открыть папку: {e}")))?;
    dirs.sort_by(|a, b| library::natural_cmp(&a.0, &b.0));
    let parent = path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(Json(json!({
        "path": path,
        "parent": parent,
        "dirs": dirs.iter().map(|(n, p)| entry(n.clone(), p)).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct Settings {
    server_name: Option<String>,
    port: Option<u16>,
    ffmpeg_path: Option<String>,
    ffprobe_path: Option<String>,
    tv: Option<TvProfile>,
    transcode: Option<TranscodeSettings>,
}

async fn save_settings(State(s): State<Shared>, Json(b): Json<Settings>) -> ApiResult<Json<Value>> {
    let (restart, tools_changed) = {
        let mut cfg = s.config.write().unwrap();
        let old_port = cfg.port;
        let old_tools = (cfg.ffmpeg_path.clone(), cfg.ffprobe_path.clone());
        if let Some(n) = b.server_name.filter(|n| !n.trim().is_empty()) {
            cfg.server_name = n.trim().to_string();
        }
        if let Some(p) = b.port.filter(|p| *p > 0) {
            cfg.port = p;
        }
        if let Some(p) = b.ffmpeg_path {
            cfg.ffmpeg_path = p.trim().to_string();
        }
        if let Some(p) = b.ffprobe_path {
            cfg.ffprobe_path = p.trim().to_string();
        }
        if let Some(tv) = b.tv {
            cfg.tv = tv;
        }
        if let Some(t) = b.transcode {
            cfg.transcode = t;
        }
        cfg.save(&s.config_path)?;
        let tools_changed = old_tools != (cfg.ffmpeg_path.clone(), cfg.ffprobe_path.clone());
        if tools_changed {
            *s.tools.write().unwrap() = Tools::locate(&cfg);
        }
        (cfg.port != old_port, tools_changed)
    };
    if tools_changed {
        crate::spawn_caps_detection(s.clone());
    }
    Ok(Json(json!({ "ok": true, "restart_required": restart })))
}

async fn redetect(State(s): State<Shared>) -> StatusCode {
    crate::spawn_caps_detection(s);
    StatusCode::NO_CONTENT
}

async fn kill_session(State(s): State<Shared>, AxPath(id): AxPath<String>) -> StatusCode {
    s.sessions.stop(&id).await;
    StatusCode::NO_CONTENT
}
