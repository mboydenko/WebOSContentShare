//! Read-only API used by the TV client. Open to the LAN (CORS allowed, since
//! the packaged webOS app runs from a file:// origin).

use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxPath, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;
use tower_http::{cors::CorsLayer, services::ServeFile};
use tracing::{info, warn};

use crate::{
    Shared, ffmpeg, library,
    library::Kind,
    plan, sessions::SpawnSpec,
    subs, thumbs,
    util::{ApiError, ApiResult},
};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/info", get(server_info))
        .route("/api/browse", get(browse))
        .route("/api/media", get(media))
        .route("/api/raw/{root}/{*path}", get(raw))
        .route("/api/thumb", get(thumb))
        .route("/api/subs", get(subtitles))
        .route("/api/stream/start", get(stream_start))
        .route("/api/stream/{sid}/{file}", get(stream_file))
        .layer(CorsLayer::permissive())
}

async fn server_info(State(s): State<Shared>) -> Json<Value> {
    Json(json!({
        "app": "webos-content-share",
        "id": s.cfg().server_id,
        "name": s.cfg().server_name,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Deserialize)]
struct BrowseQuery {
    root: Option<String>,
    #[serde(default)]
    path: String,
}

async fn browse(State(s): State<Shared>, Query(q): Query<BrowseQuery>) -> ApiResult<Json<Value>> {
    let cfg = s.cfg();
    let Some(root) = q.root.filter(|r| !r.is_empty()) else {
        let entries: Vec<Value> = cfg
            .roots
            .iter()
            .map(|r| json!({ "name": r.name, "root": r.id, "path": "", "kind": "dir", "size": 0, "modified": 0 }))
            .collect();
        return Ok(Json(json!({ "root": null, "path": "", "title": cfg.server_name, "entries": entries })));
    };
    let abs = library::resolve(&cfg, &root, &q.path)?;
    let rel = library::normalize_rel(&q.path)?;
    let title = match rel.rsplit('/').next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ => cfg.roots.iter().find(|r| r.id == root).map(|r| r.name.clone()).unwrap_or_default(),
    };
    let rel2 = rel.clone();
    let entries = tokio::task::spawn_blocking(move || library::list_dir(&abs, &rel2)).await??;
    Ok(Json(json!({ "root": root, "path": rel, "title": title, "entries": entries })))
}

#[derive(Deserialize)]
struct FileQuery {
    root: String,
    path: String,
    device: Option<String>,
}

/// The TV profile describes the TV's hardware decoder. A desktop browser (used for testing
/// or watching on the PC) decodes heavy H.264 easily, so the reference-frame limit is lifted.
fn config_for_device(s: &Shared, device: Option<&str>) -> crate::config::Config {
    let mut cfg = s.cfg();
    if device == Some("browser") {
        cfg.tv.h264_max_refs = 0;
    }
    cfg
}

async fn media(State(s): State<Shared>, Query(q): Query<FileQuery>) -> ApiResult<Json<plan::MediaInfo>> {
    let cfg = config_for_device(&s, q.device.as_deref());
    let abs = library::resolve(&cfg, &q.root, &q.path)?;
    let kind = library::kind_of(&abs);
    if !matches!(kind, Kind::Video | Kind::Audio) {
        return Err(ApiError::bad_request("Это не видео и не аудио"));
    }
    let probe = s.probes.get(&s.tools(), &abs).await?;
    let ext_subs = if kind == Kind::Video {
        let (abs2, rel) = (abs.clone(), library::normalize_rel(&q.path)?);
        tokio::task::spawn_blocking(move || library::external_subs(&abs2, &rel)).await?
    } else {
        Vec::new()
    };
    Ok(Json(plan::media_info(&probe, &abs, &cfg.tv, ext_subs)))
}

/// Direct play: the original file with HTTP Range support. The URL ends with the
/// real file name so the TV's media pipeline can recognise the container.
async fn raw(State(s): State<Shared>, AxPath((root, path)): AxPath<(String, String)>, req: Request) -> ApiResult<Response> {
    let abs = library::resolve(&s.cfg(), &root, &path)?;
    if !abs.is_file() {
        return Err(ApiError::not_found("Файл не найден"));
    }
    let resp = match ServeFile::new(&abs).oneshot(req).await {
        Ok(r) => r,
        Err(e) => match e {},
    };
    Ok(resp.map(Body::new))
}

#[derive(Deserialize)]
struct ThumbQuery {
    root: String,
    path: String,
    size: Option<u32>,
}

async fn thumb(State(s): State<Shared>, Query(q): Query<ThumbQuery>) -> ApiResult<Response> {
    let abs = library::resolve(&s.cfg(), &q.root, &q.path)?;
    let kind = library::kind_of(&abs);
    if !matches!(kind, Kind::Video | Kind::Audio | Kind::Image) {
        return Err(ApiError::bad_request("Нет превью"));
    }
    match thumbs::get(&s, &abs, kind, q.size.unwrap_or(320)).await? {
        Some(bytes) => {
            let mut resp = bytes.into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("max-age=86400"));
            Ok(resp)
        }
        None => Err(ApiError::not_found("Нет превью")),
    }
}

#[derive(Deserialize)]
struct SubsQuery {
    root: String,
    path: String,
    id: String,
}

async fn subtitles(State(s): State<Shared>, Query(q): Query<SubsQuery>) -> ApiResult<Response> {
    let cfg = s.cfg();
    let video = library::resolve(&cfg, &q.root, &q.path)?;
    let text = if let Some(index) = q.id.strip_prefix("i:") {
        let index: usize = index.parse().map_err(|_| ApiError::bad_request("Неверный id субтитров"))?;
        subs::get_vtt(&s, subs::Source::Embedded(&video, index)).await?
    } else if let Some(rel) = q.id.strip_prefix("x:") {
        let file = library::resolve(&cfg, &q.root, rel)?;
        if library::kind_of(&file) != Kind::Subtitle {
            return Err(ApiError::bad_request("Это не файл субтитров"));
        }
        subs::get_vtt(&s, subs::Source::External(&file)).await?
    } else {
        return Err(ApiError::bad_request("Неверный id субтитров"));
    };
    let mut resp = text.into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/vtt; charset=utf-8"));
    Ok(resp)
}

#[derive(Deserialize)]
struct StartQuery {
    root: String,
    path: String,
    start: Option<f64>,
    audio: Option<usize>,
    burn: Option<usize>,
    mode: Option<String>,
    client: Option<String>,
    device: Option<String>,
    /// 1 when the client cannot decode AC3/E-AC3/DTS.
    aac: Option<u8>,
}

async fn stream_start(State(s): State<Shared>, Query(q): Query<StartQuery>) -> ApiResult<Json<Value>> {
    let cfg = config_for_device(&s, q.device.as_deref());
    let abs = library::resolve(&cfg, &q.root, &q.path)?;
    let tools = s.tools();
    let caps = s.caps();
    if !caps.ffmpeg_ok && caps.detected {
        return Err(ApiError::internal("ffmpeg не найден на сервере — потоковое воспроизведение недоступно"));
    }
    let probe = s.probes.get(&tools, &abs).await?;
    let max_start = (probe.duration - 1.0).max(0.0);
    let req = plan::StreamReq {
        start: q.start.unwrap_or(0.0).clamp(0.0, max_start),
        audio: q.audio,
        burn: q.burn,
        force_transcode: q.mode.as_deref() == Some("transcode"),
        aac_only: q.aac == Some(1),
    };

    let mut remux_start = req.start;
    if req.start > 0.5 && plan::can_remux(&probe, &req, &cfg.tv)
        && let Some(v) = probe.video()
            && let Some(k) = ffmpeg::keyframe_before(&tools, &abs, v.index, probe.start_time, req.start).await {
                remux_start = k;
            }

    let attempts = plan::attempts(&probe, &abs, &req, &cfg, &caps, remux_start);
    if attempts.is_empty() {
        return Err(ApiError::bad_request("В файле нет воспроизводимых дорожек"));
    }
    let client = q.client.unwrap_or_else(|| "anonymous".into());
    s.sessions.stop_client(&client).await;
    let title = abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    let mut last_err = String::new();
    for at in attempts {
        info!(file = %title, mode = at.mode.as_str(), encoder = %at.encoder, start = at.start, "starting stream");
        let spec = SpawnSpec {
            ffmpeg: tools.ffmpeg.clone(),
            client: client.clone(),
            title: title.clone(),
            mode: at.mode.as_str(),
            encoder: at.encoder.clone(),
            start: at.start,
            // Stream copy cuts segments wherever the source has keyframes, so only
            // re-encoded output has the fixed segment grid a VOD playlist needs.
            vod_duration: (at.mode != plan::Mode::Remux && probe.duration > 0.0).then(|| probe.duration - at.start),
            args: at.args,
        };
        match s.sessions.spawn(spec, cfg.transcode.max_sessions).await {
            Ok(sess) => {
                return Ok(Json(json!({
                    "id": sess.id,
                    "url": format!("/api/stream/{}/index.m3u8", sess.id),
                    "start": sess.start,
                    "mode": sess.mode,
                    "encoder": sess.encoder,
                })));
            }
            Err(e) => {
                warn!(encoder = %at.encoder, "stream attempt failed: {e:#}");
                last_err = format!("{e:#}");
            }
        }
    }
    Err(ApiError::internal(last_err))
}

async fn stream_file(State(s): State<Shared>, AxPath((sid, file)): AxPath<(String, String)>) -> ApiResult<Response> {
    let sess = s.sessions.get(&sid).ok_or_else(|| ApiError::not_found("Трансляция завершена"))?;
    match file.as_str() {
        "keepalive" => {
            sess.touch();
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        "stop" => {
            s.sessions.stop(&sid).await;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        _ => sess.serve(&file).await,
    }
}
