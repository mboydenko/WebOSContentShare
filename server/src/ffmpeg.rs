use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use tokio::{process::Command, time::timeout};

use crate::config::Config;

pub const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

#[derive(Clone, Debug, Serialize)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    pub fn locate(cfg: &Config) -> Self {
        let ffmpeg = find_tool("ffmpeg", &cfg.ffmpeg_path);
        let ffprobe = if !cfg.ffprobe_path.is_empty() {
            PathBuf::from(&cfg.ffprobe_path)
        } else if !cfg.ffmpeg_path.is_empty() {
            ffmpeg
                .parent()
                .map(|d| d.join(exe_name("ffprobe")))
                .unwrap_or_else(|| PathBuf::from(exe_name("ffprobe")))
        } else {
            find_tool("ffprobe", "")
        };
        Self { ffmpeg, ffprobe }
    }
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Configured path, then next to our executable, then PATH.
fn find_tool(name: &str, configured: &str) -> PathBuf {
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    let file = exe_name(name);
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        for c in [dir.join(&file), dir.join("ffmpeg").join("bin").join(&file), dir.join("ffmpeg").join(&file)] {
            if c.is_file() {
                return c;
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&paths) {
            let c = d.join(&file);
            if c.is_file() {
                return c;
            }
        }
    }
    PathBuf::from(file)
}

pub fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

/// "file:" prefix stops ffmpeg from interpreting parts of the name as a protocol.
pub fn file_arg(path: &Path) -> String {
    format!("file:{}", path.to_string_lossy())
}

/// What the installed ffmpeg can do on this machine.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub detected: bool,
    pub ffmpeg_ok: bool,
    pub ffprobe_ok: bool,
    pub version: String,
    /// Working hardware encoder families, best first.
    pub hw: Vec<String>,
    /// zscale + tonemap filters are available (HDR -> SDR).
    pub tonemap: bool,
}

const HW_CANDIDATES: &[&str] = if cfg!(windows) { &["nvenc", "qsv", "amf"] } else { &["nvenc", "vaapi", "qsv"] };

async fn run_ok(cmd: &mut Command, secs: u64) -> Option<std::process::Output> {
    match timeout(Duration::from_secs(secs), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => Some(out),
        _ => None,
    }
}

pub async fn detect_caps(tools: &Tools) -> Caps {
    let mut caps = Caps::default();
    if let Some(out) = run_ok(command(&tools.ffmpeg).arg("-version"), 10).await {
        caps.ffmpeg_ok = true;
        caps.version = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string();
    }
    caps.ffprobe_ok = run_ok(command(&tools.ffprobe).arg("-version"), 10).await.is_some();
    if caps.ffmpeg_ok {
        if let Some(out) = run_ok(command(&tools.ffmpeg).args(["-hide_banner", "-filters"]), 10).await {
            let text = String::from_utf8_lossy(&out.stdout);
            caps.tonemap = text.contains(" zscale ") && text.contains(" tonemap ");
        }
        for hw in HW_CANDIDATES {
            if test_encoder(tools, hw).await {
                caps.hw.push(hw.to_string());
            }
        }
    }
    caps.detected = true;
    caps
}

async fn test_encoder(tools: &Tools, hw: &str) -> bool {
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    if hw == "vaapi" {
        args.extend(["-vaapi_device".into(), VAAPI_DEVICE.into()]);
    }
    args.extend(["-f", "lavfi", "-i", "color=c=black:s=640x360:d=0.5"].map(String::from));
    if hw == "vaapi" {
        args.extend(["-vf".into(), "format=nv12,hwupload".into()]);
    }
    args.extend(["-c:v".into(), format!("h264_{hw}"), "-f".into(), "null".into(), "-".into()]);
    run_ok(command(&tools.ffmpeg).args(&args), 20).await.is_some()
}

#[derive(Clone, Debug, Default)]
pub struct Stream {
    pub index: usize,
    pub codec_type: String,
    pub codec: String,
    pub profile: String,
    pub width: u32,
    pub height: u32,
    pub pix_fmt: String,
    pub field_order: String,
    pub color_transfer: String,
    pub dovi: bool,
    pub bits: u32,
    /// H.264 max_num_ref_frames from the SPS (0 = unknown).
    pub refs: u32,
    pub fps: f64,
    pub channels: u32,
    pub language: String,
    pub title: String,
    pub default: bool,
    pub forced: bool,
    pub attached_pic: bool,
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn f64_of(v: &Value, k: &str) -> f64 {
    match v.get(k) {
        Some(Value::String(s)) => s.parse().unwrap_or(0.0),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn rate(s: &str) -> f64 {
    match s.split_once('/') {
        Some((a, b)) => {
            let (a, b) = (a.parse::<f64>().unwrap_or(0.0), b.parse::<f64>().unwrap_or(0.0));
            if b > 0.0 { a / b } else { 0.0 }
        }
        None => s.parse().unwrap_or(0.0),
    }
}

impl Stream {
    fn from_json(v: &Value) -> Self {
        let disp = v.get("disposition");
        let flag = |k: &str| disp.and_then(|d| d.get(k)).and_then(Value::as_i64).unwrap_or(0) == 1;
        let tags = v.get("tags");
        let tag = |k: &str| {
            tags.and_then(|t| t.get(k).or_else(|| t.get(k.to_uppercase())))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let pix_fmt = str_of(v, "pix_fmt");
        let raw_bits = f64_of(v, "bits_per_raw_sample") as u32;
        let bits = if raw_bits > 0 {
            raw_bits
        } else if pix_fmt.contains("p10") || pix_fmt.contains("p010") {
            10
        } else if pix_fmt.contains("p12") {
            12
        } else {
            8
        };
        let mut fps = rate(&str_of(v, "avg_frame_rate"));
        if fps <= 0.0 {
            fps = rate(&str_of(v, "r_frame_rate"));
        }
        let dovi = v
            .get("side_data_list")
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|sd| str_of(sd, "side_data_type").contains("DOVI")));
        let codec = str_of(v, "codec_name");
        let refs = if codec == "h264" {
            crate::h264::max_ref_frames(&crate::h264::parse_hexdump(&str_of(v, "extradata"))).unwrap_or(0)
        } else {
            0
        };
        Self {
            index: v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            codec_type: str_of(v, "codec_type"),
            codec,
            profile: str_of(v, "profile"),
            width: f64_of(v, "width") as u32,
            height: f64_of(v, "height") as u32,
            pix_fmt,
            field_order: str_of(v, "field_order"),
            color_transfer: str_of(v, "color_transfer"),
            dovi,
            bits,
            refs,
            fps,
            channels: f64_of(v, "channels") as u32,
            language: tag("language"),
            title: tag("title"),
            default: flag("default"),
            forced: flag("forced"),
            attached_pic: flag("attached_pic"),
        }
    }

    pub fn hdr(&self) -> Option<&'static str> {
        match self.color_transfer.as_str() {
            "smpte2084" if self.dovi => Some("Dolby Vision"),
            "smpte2084" => Some("HDR10"),
            "arib-std-b67" => Some("HLG"),
            _ if self.dovi => Some("Dolby Vision"),
            _ => None,
        }
    }

    pub fn interlaced(&self) -> bool {
        matches!(self.field_order.as_str(), "tt" | "bb" | "tb" | "bt")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub format: String,
    pub duration: f64,
    pub start_time: f64,
    pub size: u64,
    pub bit_rate: u64,
    pub streams: Vec<Stream>,
}

impl Probe {
    fn from_json(v: &Value) -> Self {
        let fmt = v.get("format").cloned().unwrap_or(Value::Null);
        Self {
            format: str_of(&fmt, "format_name"),
            duration: f64_of(&fmt, "duration"),
            start_time: f64_of(&fmt, "start_time"),
            size: f64_of(&fmt, "size") as u64,
            bit_rate: f64_of(&fmt, "bit_rate") as u64,
            streams: v
                .get("streams")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(Stream::from_json).collect())
                .unwrap_or_default(),
        }
    }

    pub fn video(&self) -> Option<&Stream> {
        self.streams.iter().find(|s| s.codec_type == "video" && !s.attached_pic)
    }

    pub fn audio(&self) -> impl Iterator<Item = &Stream> {
        self.streams.iter().filter(|s| s.codec_type == "audio")
    }

    pub fn subtitles(&self) -> impl Iterator<Item = &Stream> {
        self.streams.iter().filter(|s| s.codec_type == "subtitle")
    }

    pub fn stream(&self, index: usize) -> Option<&Stream> {
        self.streams.iter().find(|s| s.index == index)
    }

    pub fn default_audio(&self) -> Option<&Stream> {
        self.audio().find(|s| s.default).or_else(|| self.audio().next())
    }
}

#[derive(Default)]
pub struct ProbeCache {
    map: Mutex<HashMap<PathBuf, (u64, u64, Arc<Probe>)>>,
}

impl ProbeCache {
    pub async fn get(&self, tools: &Tools, path: &Path) -> Result<Arc<Probe>> {
        let md = tokio::fs::metadata(path).await?;
        let mtime = md
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some((m, l, p)) = self.map.lock().unwrap().get(path)
            && *m == mtime && *l == md.len() {
                return Ok(p.clone());
            }
        let probe = Arc::new(run_probe(tools, path).await?);
        let mut map = self.map.lock().unwrap();
        if map.len() > 4000 {
            map.clear();
        }
        map.insert(path.to_path_buf(), (mtime, md.len(), probe.clone()));
        Ok(probe)
    }
}

async fn run_probe(tools: &Tools, path: &Path) -> Result<Probe> {
    let out = timeout(
        Duration::from_secs(60),
        command(&tools.ffprobe)
            // -show_data adds codec extradata, which carries the H.264 SPS.
            .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams", "-show_data"])
            .arg(file_arg(path))
            .output(),
    )
    .await
    .context("ffprobe: превышено время ожидания")?
    .context("Не удалось запустить ffprobe (установлен ли ffmpeg?)")?;
    if !out.status.success() {
        bail!("ffprobe: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("ffprobe вернул некорректный JSON")?;
    Ok(Probe::from_json(&v))
}

/// Timestamp (relative to file start) of the last video keyframe at or before `t`.
/// Stream copy can only start on a keyframe, so this is the real start of a remux.
pub async fn keyframe_before(tools: &Tools, path: &Path, stream: usize, start_time: f64, t: f64) -> Option<f64> {
    let target = start_time + t;
    for window in [8.0, 40.0] {
        let from = (target - window).max(0.0);
        let interval = format!("{from:.3}%{:.3}", target + 0.05);
        let out = timeout(
            Duration::from_secs(20),
            command(&tools.ffprobe)
                .args(["-v", "error", "-select_streams", &stream.to_string()])
                .args(["-show_entries", "packet=pts_time,flags", "-of", "csv=p=0"])
                .args(["-read_intervals", &interval])
                .arg(file_arg(path))
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let best = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(',');
                let pts: f64 = fields.next()?.trim().parse().ok()?;
                let flags = fields.next_back().unwrap_or("");
                flags.starts_with('K').then_some(pts)
            })
            .filter(|pts| *pts <= target + 0.05)
            .fold(None, |acc: Option<f64>, p| Some(acc.map_or(p, |a| a.max(p))));
        if let Some(k) = best {
            return Some((k - start_time).max(0.0));
        }
    }
    None
}
