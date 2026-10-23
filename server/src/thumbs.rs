use std::{path::Path, time::Duration};

use crate::{AppState, ffmpeg, library::Kind, plan, util};

/// JPEG preview (video frame, album cover or downscaled photo), cached on disk.
/// Returns None when the file has no picture to show.
pub async fn get(state: &AppState, abs: &Path, kind: Kind, size: u32) -> anyhow::Result<Option<Vec<u8>>> {
    let size = size.clamp(64, 1920);
    let md = tokio::fs::metadata(abs).await?;
    let key = util::stable_hash(&[
        abs.to_string_lossy().as_bytes(),
        &util::mtime_secs(&md).to_le_bytes(),
        &md.len().to_le_bytes(),
        &size.to_le_bytes(),
    ]);
    let dir = state.cache_dir.join("thumbs");
    tokio::fs::create_dir_all(&dir).await?;
    let out = dir.join(format!("{key:016x}.jpg"));
    let fail = dir.join(format!("{key:016x}.fail"));
    if let Ok(b) = tokio::fs::read(&out).await {
        return Ok(Some(b));
    }
    if fail.exists() {
        return Ok(None);
    }

    let _permit = state.thumb_sem.acquire().await?;
    if let Ok(b) = tokio::fs::read(&out).await {
        return Ok(Some(b));
    }

    let tools = state.tools();
    let scale = format!("scale={size}:{size}:force_original_aspect_ratio=decrease");
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"].map(String::from).to_vec();
    let vf = match kind {
        Kind::Video => {
            let probe = state.probes.get(&tools, abs).await?;
            let Some(v) = probe.video() else {
                tokio::fs::write(&fail, b"").await?;
                return Ok(None);
            };
            let t = if probe.duration > 0.0 { (probe.duration * 0.1).min(120.0) } else { 0.0 };
            args.extend(["-ss".into(), format!("{t:.2}"), "-i".into(), ffmpeg::file_arg(abs)]);
            args.extend(["-map".into(), format!("0:{}", v.index)]);
            if v.hdr().is_some() && state.caps().tonemap { format!("{},{scale}", plan::TONEMAP) } else { scale }
        }
        Kind::Audio => {
            args.extend(["-i".into(), ffmpeg::file_arg(abs), "-map".into(), "0:v:0".into(), "-an".into()]);
            scale
        }
        _ => {
            args.extend(["-i".into(), ffmpeg::file_arg(abs)]);
            scale
        }
    };
    let tmp = dir.join(format!("{key:016x}.{}.tmp", util::random_id()));
    args.extend(["-frames:v".into(), "1".into(), "-vf".into(), vf]);
    args.extend(["-q:v", "3", "-f", "image2", "-update", "1", "-c:v", "mjpeg"].map(String::from));
    args.push(tmp.to_string_lossy().into_owned());

    let status = tokio::time::timeout(
        Duration::from_secs(60),
        ffmpeg::command(&tools.ffmpeg).args(&args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status(),
    )
    .await;
    let ok = matches!(status, Ok(Ok(s)) if s.success()) && tmp.exists();
    if !ok {
        let _ = tokio::fs::remove_file(&tmp).await;
        tokio::fs::write(&fail, b"").await?;
        return Ok(None);
    }
    tokio::fs::rename(&tmp, &out).await?;
    Ok(Some(tokio::fs::read(&out).await?))
}
