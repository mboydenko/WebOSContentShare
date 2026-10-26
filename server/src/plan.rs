//! Decides how a file reaches the TV: as-is (direct), remuxed with the video
//! stream copied, or fully transcoded. Builds the ffmpeg command for the last two.

use std::path::Path;

use serde::Serialize;

use crate::{
    config::{Config, TvProfile},
    ffmpeg::{self, Caps, Probe, Stream, VAAPI_DEVICE},
    library::{self, ExtSub},
};

/// HDR (PQ/HLG) -> SDR BT.709 conversion; needs ffmpeg built with zimg.
pub const TONEMAP: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p";
pub const SEGMENT_SECONDS: u32 = 4;

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn codec_name(codec: &str) -> String {
    match codec {
        "h264" => "H.264",
        "hevc" => "HEVC",
        "mpeg2video" => "MPEG-2",
        "mpeg4" => "MPEG-4 (DivX/Xvid)",
        "msmpeg4v3" | "msmpeg4v2" => "MS MPEG-4",
        "vc1" => "VC-1",
        "wmv3" | "wmv2" | "wmv1" => "WMV",
        "vp9" => "VP9",
        "vp8" => "VP8",
        "av1" => "AV1",
        "aac" => "AAC",
        "ac3" => "AC3",
        "eac3" => "E-AC3",
        "dts" => "DTS",
        "truehd" => "TrueHD",
        "mp3" => "MP3",
        "mp2" => "MP2",
        "flac" => "FLAC",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "subrip" => "SRT",
        "ass" | "ssa" => "ASS",
        "hdmv_pgs_subtitle" => "PGS",
        "dvd_subtitle" => "VobSub",
        "dvb_subtitle" => "DVB",
        "mov_text" => "TX3G",
        other if other.starts_with("pcm_") => "PCM",
        other => return other.to_uppercase(),
    }
    .to_string()
}

fn describe_video(v: &Stream) -> String {
    let mut s = codec_name(&v.codec);
    if !v.profile.is_empty() && v.profile != "unknown" {
        s.push_str(&format!(" ({})", v.profile));
    }
    s
}

/// `for_ts`: the stream must also fit into MPEG-TS for HLS remuxing (no VP9/AV1).
fn video_ok(v: &Stream, tv: &TvProfile, for_ts: bool) -> Result<(), String> {
    if v.width > tv.max_width || v.height > tv.max_height {
        return Err(format!("Разрешение {}x{} выше поддерживаемого ТВ", v.width, v.height));
    }
    // Copying the stream would not help here: the decoder load is the problem, so this rules out remux too.
    if v.codec == "h264" && tv.h264_max_refs > 0 && v.refs > tv.h264_max_refs {
        return Err(format!(
            "H.264 с {} опорными кадрами (ТВ справляется с {}) — воспроизведение тормозило бы",
            v.refs, tv.h264_max_refs
        ));
    }
    let chroma_ok = !(v.pix_fmt.contains("422") || v.pix_fmt.contains("444"));
    let ok = match v.codec.as_str() {
        "h264" => v.bits <= 8 && chroma_ok,
        "hevc" => tv.hevc && v.bits <= 10 && chroma_ok,
        "mpeg2video" => chroma_ok,
        "vp9" => !for_ts && tv.vp9 && v.bits <= 10,
        "av1" => !for_ts && tv.av1,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("Видео {} {}-бит не поддерживается ТВ", describe_video(v), v.bits))
    }
}

fn audio_ok(a: &Stream, tv: &TvProfile, audio_file: bool, for_ts: bool) -> bool {
    match a.codec.as_str() {
        "aac" | "mp3" | "ac3" | "eac3" => true,
        "mp2" => for_ts || audio_file,
        "dts" => tv.dts,
        "truehd" => tv.truehd && !for_ts,
        "opus" | "vorbis" => (audio_file || tv.opus) && !for_ts,
        "flac" | "alac" => audio_file && !for_ts,
        c if c.starts_with("pcm_") => audio_file && !for_ts,
        _ => false,
    }
}

fn container_ok(p: &Probe, ext: &str, audio_file: bool) -> bool {
    let f = p.format.as_str();
    if audio_file {
        f.split(',').any(|n| ["mp3", "flac", "ogg", "wav", "aac"].contains(&n))
            || (f.contains("mp4") && ["m4a", "mp4", "aac"].contains(&ext))
    } else {
        f.contains("matroska") || f.contains("webm") || (f.contains("mp4") && ["mp4", "m4v", "mov"].contains(&ext))
    }
}

fn lang_name(code: &str) -> &str {
    match code {
        "rus" | "ru" => "Русский",
        "eng" | "en" => "English",
        "ukr" | "uk" => "Українська",
        "jpn" | "ja" => "日本語",
        "ger" | "deu" | "de" => "Deutsch",
        "fre" | "fra" | "fr" => "Français",
        "spa" | "es" => "Español",
        "ita" | "it" => "Italiano",
        "kor" | "ko" => "한국어",
        "chi" | "zho" | "zh" => "中文",
        "und" | "" => "",
        other => other,
    }
}

fn channels_name(ch: u32) -> String {
    match ch {
        1 => "моно".into(),
        2 => "стерео".into(),
        6 => "5.1".into(),
        8 => "7.1".into(),
        n => format!("{n} кан."),
    }
}

fn join_label(parts: &[&str]) -> String {
    parts.iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoOut {
    pub index: usize,
    pub codec: String,
    pub label: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bit_depth: u32,
    pub hdr: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioOut {
    pub index: usize,
    pub codec: String,
    pub channels: u32,
    pub language: String,
    pub label: String,
    pub default: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubOut {
    /// "i:<stream index>" for embedded, "x:<relative path>" for external files.
    pub id: String,
    /// Stream index for burning image subtitles in, if embedded.
    pub index: Option<usize>,
    pub label: String,
    /// Text subtitles are rendered by the client; image ones must be burned in.
    pub text: bool,
    pub forced: bool,
    pub default: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub kind: &'static str,
    pub duration: f64,
    pub container: String,
    pub size: u64,
    pub bitrate: u64,
    pub video: Option<VideoOut>,
    pub audio: Vec<AudioOut>,
    pub subtitles: Vec<SubOut>,
    pub default_audio: Option<usize>,
    /// The TV can play the file as-is (with the default audio track).
    pub direct: bool,
    /// Why direct play is impossible.
    pub reasons: Vec<String>,
    /// What /api/stream/start will do in "auto" mode with the default audio track.
    pub stream_mode: &'static str,
}

const TEXT_SUBS: &[&str] = &["subrip", "srt", "ass", "ssa", "webvtt", "mov_text", "text"];

pub fn media_info(p: &Probe, abs: &Path, tv: &TvProfile, ext_subs: Vec<ExtSub>) -> MediaInfo {
    let ext = library::ext_of(abs);
    let video = p.video();
    let audio_file = video.is_none();
    let default_audio = p.default_audio();
    let mut reasons = Vec::new();

    if !container_ok(p, &ext, audio_file) {
        reasons.push(format!("Контейнер {} не воспроизводится ТВ напрямую", ext.to_uppercase()));
    }
    if let Some(v) = video
        && let Err(e) = video_ok(v, tv, false) {
            reasons.push(e);
        }
    if let Some(a) = default_audio
        && !audio_ok(a, tv, audio_file, false) {
            reasons.push(format!("Звук {} не поддерживается ТВ", codec_name(&a.codec)));
        }

    let stream_mode = match video {
        None => "audio",
        Some(v) if video_ok(v, tv, true).is_ok() => "remux",
        Some(_) => "transcode",
    };

    let audio = p
        .audio()
        .map(|a| AudioOut {
            index: a.index,
            codec: a.codec.clone(),
            channels: a.channels,
            language: a.language.clone(),
            label: join_label(&[
                lang_name(&a.language),
                &a.title,
                &format!("{} {}", codec_name(&a.codec), channels_name(a.channels)),
            ]),
            default: Some(a.index) == default_audio.map(|d| d.index),
        })
        .collect();

    let mut subtitles: Vec<SubOut> = p
        .subtitles()
        .map(|s| SubOut {
            id: format!("i:{}", s.index),
            index: Some(s.index),
            label: join_label(&[
                lang_name(&s.language),
                &s.title,
                &codec_name(&s.codec),
                if s.forced { "форсированные" } else { "" },
            ]),
            text: TEXT_SUBS.contains(&s.codec.as_str()),
            forced: s.forced,
            default: s.default,
        })
        .collect();
    subtitles.extend(ext_subs.into_iter().map(|x| SubOut {
        id: format!("x:{}", x.rel),
        index: None,
        label: join_label(&[&x.label, &x.ext.to_uppercase(), "файл"]),
        text: true,
        forced: false,
        default: false,
    }));

    MediaInfo {
        kind: if audio_file { "audio" } else { "video" },
        duration: p.duration,
        container: p.format.clone(),
        size: p.size,
        bitrate: p.bit_rate,
        video: video.map(|v| VideoOut {
            index: v.index,
            codec: v.codec.clone(),
            label: describe_video(v),
            width: v.width,
            height: v.height,
            fps: (v.fps * 1000.0).round() / 1000.0,
            bit_depth: v.bits,
            hdr: v.hdr(),
        }),
        audio,
        subtitles,
        default_audio: default_audio.map(|a| a.index),
        direct: reasons.is_empty(),
        reasons,
        stream_mode,
    }
}

pub struct StreamReq {
    pub start: f64,
    pub audio: Option<usize>,
    pub burn: Option<usize>,
    pub force_transcode: bool,
    /// The client cannot decode Dolby audio (desktop Firefox/Chrome via MSE): send AAC.
    pub aac_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Remux,
    Transcode,
    Audio,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Remux => "remux",
            Mode::Transcode => "transcode",
            Mode::Audio => "audio",
        }
    }
}

pub struct Attempt {
    pub mode: Mode,
    pub encoder: String,
    pub start: f64,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct Encoder {
    hw: Option<String>,
    hevc: bool,
}

impl Encoder {
    fn from_settings(cfg: &Config, caps: &Caps) -> Self {
        let hw = match cfg.transcode.hw.as_str() {
            "auto" => caps.hw.first().cloned(),
            "none" | "" => None,
            other => Some(other.to_string()),
        };
        Self { hw, hevc: cfg.transcode.codec == "hevc" && cfg.tv.hevc }
    }

    fn name(&self) -> String {
        match (&self.hw, self.hevc) {
            (None, false) => "libx264".into(),
            (None, true) => "libx265".into(),
            (Some(hw), false) => format!("h264_{hw}"),
            (Some(hw), true) => format!("hevc_{hw}"),
        }
    }
}

pub fn can_remux(p: &Probe, req: &StreamReq, tv: &TvProfile) -> bool {
    !req.force_transcode && burn_stream(p, req).is_none() && p.video().is_some_and(|v| video_ok(v, tv, true).is_ok())
}

fn selected_audio<'a>(p: &'a Probe, req: &StreamReq) -> Option<&'a Stream> {
    req.audio
        .and_then(|i| p.stream(i))
        .filter(|s| s.codec_type == "audio")
        .or_else(|| p.default_audio())
}

fn burn_stream<'a>(p: &'a Probe, req: &StreamReq) -> Option<&'a Stream> {
    req.burn.and_then(|i| p.stream(i)).filter(|s| s.codec_type == "subtitle")
}

/// Ordered list of ways to produce the stream; the caller tries them until ffmpeg starts.
pub fn attempts(p: &Probe, abs: &Path, req: &StreamReq, cfg: &Config, caps: &Caps, remux_start: f64) -> Vec<Attempt> {
    let audio = selected_audio(p, req);
    let Some(video) = p.video() else {
        let Some(a) = audio else { return Vec::new() };
        let mut list = input_args(p, abs, req.start, None);
        list.extend(["-map".into(), format!("0:{}", a.index)]);
        list.extend(args(&["-vn", "-c:a", "aac", "-b:a", "320k", "-ac", "2"]));
        hls_output(&mut list);
        return vec![Attempt { mode: Mode::Audio, encoder: "aac".into(), start: req.start, args: list }];
    };

    let mut list = Vec::new();
    if can_remux(p, req, &cfg.tv) {
        let mut a = input_args(p, abs, remux_start, None);
        a.extend(["-map".into(), format!("0:{}", video.index), "-c:v".into(), "copy".into()]);
        if let Some(au) = audio {
            audio_args(&mut a, au, &cfg.tv, req.aac_only, false);
        }
        hls_output(&mut a);
        list.push(Attempt { mode: Mode::Remux, encoder: "copy".into(), start: remux_start, args: a });
    }

    let primary = Encoder::from_settings(cfg, caps);
    let software = Encoder { hw: None, hevc: false };
    let mut encoders = vec![primary.clone()];
    if primary != software {
        encoders.push(software);
    }
    for enc in encoders {
        list.push(Attempt {
            mode: Mode::Transcode,
            encoder: enc.name(),
            start: req.start,
            args: transcode_args(p, abs, req, cfg, caps, video, audio, &enc),
        });
    }
    list
}

fn input_args(p: &Probe, abs: &Path, start: f64, hw: Option<&str>) -> Vec<String> {
    let mut a = args(&["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]);
    if hw == Some("vaapi") {
        a.extend(args(&["-vaapi_device", VAAPI_DEVICE]));
    }
    if p.format.split(',').any(|f| f == "avi") {
        a.extend(args(&["-fflags", "+genpts"]));
    }
    if start > 0.01 {
        a.extend(["-ss".into(), format!("{start:.3}")]);
    }
    a.extend(["-i".into(), ffmpeg::file_arg(abs)]);
    a
}

/// `exact_start`: the video is re-encoded from a mid-file seek point. A copied audio track
/// cannot be trimmed there and would start at the preceding keyframe (up to ~10 s early),
/// leaving the picture frozen until the sound catches up, so it is re-encoded instead.
fn audio_args(a: &mut Vec<String>, s: &Stream, tv: &TvProfile, aac_only: bool, exact_start: bool) {
    a.extend(["-map".into(), format!("0:{}", s.index)]);
    let codec_ok = if aac_only { matches!(s.codec.as_str(), "aac" | "mp3") } else { audio_ok(s, tv, false, true) };
    let copy_ok = codec_ok && !exact_start;
    if copy_ok {
        a.extend(args(&["-c:a", "copy"]));
    } else if s.channels > 2 && aac_only {
        a.extend(args(&["-c:a", "aac", "-b:a", "384k", "-ac", "6"]));
    } else if s.channels > 2 {
        // Keeps surround sound; AC3 is passed through to soundbars/receivers.
        a.extend(args(&["-c:a", "ac3", "-b:a", "640k"]));
        if s.channels > 6 {
            a.extend(args(&["-ac", "6"]));
        }
    } else {
        a.extend(args(&["-c:a", "aac", "-b:a", "256k", "-ac", "2"]));
    }
}

fn hls_output(a: &mut Vec<String>) {
    a.extend(args(&[
        "-sn", "-dn", "-map_metadata", "-1", "-map_chapters", "-1", "-max_muxing_queue_size", "4096",
        "-f", "hls", "-hls_time", "4", "-hls_list_size", "0", "-hls_playlist_type", "event",
        "-hls_flags", "temp_file", "-hls_segment_type", "mpegts", "-start_number", "0",
        "-hls_segment_filename", "seg%05d.ts", "index.m3u8",
    ]));
}

fn max_bitrate_kbps(out_height: u32, cfg: &Config) -> u32 {
    let by_res = match out_height {
        0..=576 => 4_000,
        577..=720 => 8_000,
        721..=1080 => 16_000,
        1081..=1440 => 25_000,
        _ => 45_000,
    };
    by_res.min(cfg.transcode.max_bitrate_mbps.max(2) * 1000)
}

#[allow(clippy::too_many_arguments)]
fn transcode_args(
    p: &Probe,
    abs: &Path,
    req: &StreamReq,
    cfg: &Config,
    caps: &Caps,
    v: &Stream,
    audio: Option<&Stream>,
    enc: &Encoder,
) -> Vec<String> {
    let hw = enc.hw.as_deref();
    let mut a = input_args(p, abs, req.start, hw);
    let ten_bit = enc.hevc && v.bits >= 10;
    let hdr = v.hdr();
    let tonemapped = hdr.is_some() && !ten_bit && caps.tonemap;

    let mut chain: Vec<String> = Vec::new();
    if v.interlaced() {
        chain.push("bwdif=0".into());
    }
    if tonemapped {
        chain.push(TONEMAP.into());
    }
    let max_h = cfg.transcode.max_height.min(cfg.tv.max_height).max(240);
    let max_w = (max_h * 16 / 9).min(cfg.tv.max_width.max(320));
    let mut out_h = v.height;
    if v.width > max_w || v.height > max_h {
        chain.push(format!(
            "scale=w={max_w}:h={max_h}:force_original_aspect_ratio=decrease:force_divisible_by=2"
        ));
        let r = (max_w as f64 / v.width.max(1) as f64).min(max_h as f64 / v.height.max(1) as f64);
        out_h = (v.height as f64 * r) as u32;
    } else if v.width % 2 == 1 || v.height % 2 == 1 {
        chain.push("scale=trunc(iw/2)*2:trunc(ih/2)*2".into());
    }
    let pix = match (hw, ten_bit) {
        (Some("vaapi"), true) => "p010,hwupload",
        (Some("vaapi"), false) => "nv12,hwupload",
        (None, true) => "yuv420p10le",
        (None, false) | (Some("nvenc"), false) => "yuv420p",
        (Some(_), true) => "p010le",
        (Some(_), false) => "nv12",
    };
    chain.push(format!("format={pix}"));
    let vf = chain.join(",");

    match burn_stream(p, req) {
        Some(sub) => a.extend([
            "-filter_complex".into(),
            format!("[0:{}][0:{}]overlay=eof_action=pass,{vf}[vout]", v.index, sub.index),
            "-map".into(),
            "[vout]".into(),
        ]),
        None => a.extend(["-map".into(), format!("0:{}", v.index), "-vf".into(), vf]),
    }

    let q = cfg.transcode.quality.clamp(10, 40);
    let maxrate = max_bitrate_kbps(out_h, cfg);
    let rate_cap = [
        "-maxrate".to_string(),
        format!("{maxrate}k"),
        "-bufsize".to_string(),
        format!("{}k", maxrate * 2),
    ];
    a.extend(["-c:v".into(), enc.name()]);
    match hw {
        None if enc.hevc => {
            a.extend(args(&["-preset", "veryfast", "-x265-params", "log-level=error"]));
            a.extend(["-crf".into(), (q + 2).to_string()]);
            a.extend(rate_cap);
        }
        None => {
            a.extend(args(&["-preset", "veryfast", "-profile:v", "high"]));
            a.extend(["-crf".into(), q.to_string()]);
            a.extend(rate_cap);
        }
        Some("nvenc") => {
            a.extend(args(&["-preset", "p5", "-tune", "hq", "-rc", "vbr", "-b:v", "0", "-forced-idr", "1"]));
            a.extend(["-cq".into(), q.to_string()]);
            let profile = match (enc.hevc, ten_bit) {
                (true, true) => "main10",
                (true, false) => "main",
                _ => "high",
            };
            a.extend(["-profile:v".into(), profile.into()]);
            a.extend(rate_cap);
        }
        Some("qsv") => {
            a.extend(args(&["-preset", "faster"]));
            a.extend(["-global_quality".into(), q.to_string()]);
        }
        Some("amf") => {
            a.extend(args(&["-usage", "transcoding", "-quality", "balanced", "-rc", "cqp"]));
            a.extend(["-qp_i".into(), q.to_string(), "-qp_p".into(), (q + 2).to_string()]);
            if !enc.hevc {
                a.extend(["-qp_b".into(), (q + 4).to_string()]);
            }
        }
        Some("vaapi") => a.extend(["-qp".into(), q.to_string()]),
        Some(_) => {}
    }
    // Fixed keyframe spacing gives evenly sized HLS segments.
    a.extend(["-force_key_frames".into(), format!("expr:gte(t,n_forced*{SEGMENT_SECONDS})")]);
    if tonemapped {
        a.extend(args(&["-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709"]));
    } else if let (Some(h), true) = (hdr, ten_bit) {
        let trc = if h == "HLG" { "arib-std-b67" } else { "smpte2084" };
        a.extend(args(&["-color_primaries", "bt2020", "-colorspace", "bt2020nc", "-color_trc", trc]));
    }

    if let Some(au) = audio {
        audio_args(&mut a, au, &cfg.tv, req.aac_only, req.start > 0.01);
    }
    hls_output(&mut a);
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(codec: &str, bits: u32) -> Stream {
        Stream {
            codec_type: "video".into(),
            codec: codec.into(),
            width: 1920,
            height: 1080,
            pix_fmt: if bits > 8 { "yuv420p10le".into() } else { "yuv420p".into() },
            bits,
            ..Default::default()
        }
    }

    fn audio(codec: &str, channels: u32) -> Stream {
        Stream { index: 1, codec_type: "audio".into(), codec: codec.into(), channels, default: true, ..Default::default() }
    }

    fn probe(format: &str, streams: Vec<Stream>) -> Probe {
        Probe { format: format.into(), duration: 100.0, streams, ..Default::default() }
    }

    #[test]
    fn mkv_h264_aac_is_direct() {
        let p = probe("matroska,webm", vec![video("h264", 8), audio("aac", 2)]);
        let info = media_info(&p, Path::new("a.mkv"), &TvProfile::default(), vec![]);
        assert!(info.direct, "{:?}", info.reasons);
    }

    #[test]
    fn avi_h264_is_remuxed() {
        let p = probe("avi", vec![video("h264", 8), audio("mp3", 2)]);
        let info = media_info(&p, Path::new("a.avi"), &TvProfile::default(), vec![]);
        assert!(!info.direct);
        assert_eq!(info.stream_mode, "remux");
    }

    #[test]
    fn dts_audio_blocks_direct_but_video_is_copied() {
        let p = probe("matroska,webm", vec![video("hevc", 10), audio("dts", 6)]);
        let info = media_info(&p, Path::new("a.mkv"), &TvProfile::default(), vec![]);
        assert!(!info.direct);
        assert_eq!(info.stream_mode, "remux");
        let req = StreamReq { start: 0.0, audio: None, burn: None, force_transcode: false, aac_only: false };
        let list = attempts(&p, Path::new("a.mkv"), &req, &Config::default(), &Caps::default(), 0.0);
        assert_eq!(list[0].mode, Mode::Remux);
        let joined = list[0].args.join(" ");
        assert!(joined.contains("-c:v copy") && joined.contains("-c:a ac3"), "{joined}");
    }

    #[test]
    fn heavy_h264_is_transcoded_not_remuxed() {
        let mut v = video("h264", 8);
        v.refs = 12;
        let p = probe("matroska,webm", vec![v, audio("ac3", 6)]);
        let info = media_info(&p, Path::new("a.mkv"), &TvProfile::default(), vec![]);
        assert!(!info.direct);
        assert_eq!(info.stream_mode, "transcode");
        let unlimited = TvProfile { h264_max_refs: 0, ..TvProfile::default() };
        assert!(media_info(&p, Path::new("a.mkv"), &unlimited, vec![]).direct);
    }

    #[test]
    fn transcode_after_seek_reencodes_audio() {
        let p = probe("avi", vec![video("mpeg4", 8), audio("ac3", 6)]);
        let at = |start: f64| {
            let req = StreamReq { start, audio: None, burn: None, force_transcode: false, aac_only: false };
            attempts(&p, Path::new("a.avi"), &req, &Config::default(), &Caps::default(), start)[0].args.join(" ")
        };
        assert!(at(0.0).contains("-c:a copy"));
        let seeked = at(610.0);
        assert!(!seeked.contains("-c:a copy") && seeked.contains("-c:a ac3"), "{seeked}");
    }

    #[test]
    fn hi10p_and_xvid_are_transcoded() {
        for v in [video("h264", 10), video("mpeg4", 8)] {
            let p = probe("matroska,webm", vec![v, audio("aac", 2)]);
            let info = media_info(&p, Path::new("a.mkv"), &TvProfile::default(), vec![]);
            assert_eq!(info.stream_mode, "transcode");
        }
    }
}
