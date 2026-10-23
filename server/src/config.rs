use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Root {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

/// What the TV can decode natively. Anything outside it is remuxed or transcoded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TvProfile {
    pub hevc: bool,
    pub vp9: bool,
    pub av1: bool,
    pub dts: bool,
    pub truehd: bool,
    /// Opus/Vorbis audio inside MKV/WebM video files.
    pub opus: bool,
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for TvProfile {
    fn default() -> Self {
        Self {
            hevc: true,
            vp9: true,
            av1: false,
            dts: false,
            truehd: false,
            opus: false,
            max_width: 4096,
            max_height: 2160,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscodeSettings {
    /// "auto", "none", "nvenc", "qsv", "amf", "vaapi"
    pub hw: String,
    /// "h264" or "hevc"
    pub codec: String,
    /// CRF-like quality, lower is better (16..30).
    pub quality: u32,
    pub max_bitrate_mbps: u32,
    pub max_height: u32,
    pub max_sessions: usize,
}

impl Default for TranscodeSettings {
    fn default() -> Self {
        Self {
            hw: "auto".into(),
            codec: "h264".into(),
            quality: 20,
            max_bitrate_mbps: 40,
            max_height: 2160,
            max_sessions: 3,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    pub server_name: String,
    pub roots: Vec<Root>,
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub tv: TvProfile,
    pub transcode: TranscodeSettings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 8200,
            server_name: crate::util::hostname(),
            roots: Vec::new(),
            ffmpeg_path: String::new(),
            ffprobe_path: String::new(),
            tv: TvProfile::default(),
            transcode: TranscodeSettings::default(),
        }
    }
}

pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WebOSContentShare")
        .join("config.json")
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data)
                .with_context(|| format!("Ошибка в файле настроек {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("Не удалось прочитать {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
