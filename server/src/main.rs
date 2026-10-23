mod admin;
mod config;
mod discovery;
mod ffmpeg;
mod library;
mod media_api;
mod plan;
mod sessions;
mod static_files;
mod subs;
mod thumbs;
mod util;

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use anyhow::Context;
use axum::Router;
use tokio::sync::Semaphore;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

pub struct AppState {
    pub config: RwLock<config::Config>,
    pub config_path: PathBuf,
    /// Port actually listened on (the configured one may change until restart).
    pub http_port: u16,
    pub cache_dir: PathBuf,
    pub tools: RwLock<ffmpeg::Tools>,
    pub caps: RwLock<ffmpeg::Caps>,
    pub probes: ffmpeg::ProbeCache,
    pub sessions: Arc<sessions::Sessions>,
    pub thumb_sem: Semaphore,
    pub subs_sem: Semaphore,
}

pub type Shared = Arc<AppState>;

impl AppState {
    pub fn cfg(&self) -> config::Config {
        self.config.read().unwrap().clone()
    }
    pub fn tools(&self) -> ffmpeg::Tools {
        self.tools.read().unwrap().clone()
    }
    pub fn caps(&self) -> ffmpeg::Caps {
        self.caps.read().unwrap().clone()
    }
}

pub fn spawn_caps_detection(state: Shared) {
    tokio::spawn(async move {
        let caps = ffmpeg::detect_caps(&state.tools()).await;
        if caps.ffmpeg_ok {
            info!(
                "{}; аппаратные кодировщики: {}",
                caps.version,
                if caps.hw.is_empty() { "нет (будет использован CPU)".into() } else { caps.hw.join(", ") }
            );
        } else {
            warn!("ffmpeg не найден ({}). Будут работать только файлы, которые ТВ воспроизводит напрямую.", state.tools().ffmpeg.display());
        }
        *state.caps.write().unwrap() = caps;
    });
}

struct Args {
    config: Option<PathBuf>,
    port: Option<u16>,
    no_browser: bool,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args { config: None, port: None, no_browser: false };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => args.config = Some(it.next().context("--config требует путь")?.into()),
            "--port" => args.port = Some(it.next().context("--port требует число")?.parse()?),
            "--no-browser" => args.no_browser = true,
            "-h" | "--help" => {
                println!("webos-content-share [--config FILE] [--port N] [--no-browser]");
                std::process::exit(0);
            }
            other => anyhow::bail!("Неизвестный аргумент: {other}"),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = parse_args()?;
    let config_path = args.config.unwrap_or_else(config::default_path);
    let mut cfg = config::Config::load(&config_path)?;
    // Persists defaults filled in for missing fields (notably a newly generated server_id).
    cfg.save(&config_path)?;
    if let Some(p) = args.port {
        cfg.port = p;
    }
    let cache_dir = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("WebOSContentShare");
    std::fs::create_dir_all(&cache_dir)?;

    let port = cfg.port;
    let first_run = cfg.roots.is_empty();
    let state: Shared = Arc::new(AppState {
        tools: RwLock::new(ffmpeg::Tools::locate(&cfg)),
        config: RwLock::new(cfg),
        config_path: config_path.clone(),
        http_port: port,
        cache_dir,
        caps: RwLock::new(ffmpeg::Caps::default()),
        probes: ffmpeg::ProbeCache::default(),
        sessions: Arc::new(sessions::Sessions::new(std::env::temp_dir().join("WebOSContentShare-streams"))),
        thumb_sem: Semaphore::new(2),
        subs_sem: Semaphore::new(2),
    });
    spawn_caps_detection(state.clone());
    tokio::spawn(state.sessions.clone().monitor());
    tokio::spawn(discovery::run(state.clone()));

    let app = Router::new()
        .merge(media_api::router())
        .merge(admin::router())
        .merge(static_files::router())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port)))
        .await
        .with_context(|| format!("Порт {port} занят. Укажите другой: --port N"))?;

    info!("Настройки: {}", config_path.display());
    info!("Панель управления: http://localhost:{port}/admin");
    if let Some(ip) = util::local_ip() {
        info!("Адрес для телевизора: {ip}:{port}  (веб-версия клиента: http://{ip}:{port}/app/)");
    }
    if first_run && !args.no_browser {
        util::open_browser(&format!("http://localhost:{port}/admin"));
    }

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    info!("Остановка…");
    state.sessions.stop_all().await;
    Ok(())
}
