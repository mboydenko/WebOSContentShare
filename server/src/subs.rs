use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, bail};

use crate::{AppState, ffmpeg, library, util};

pub enum Source<'a> {
    /// Stream index inside the media file.
    Embedded(&'a Path, usize),
    /// Separate .srt/.ass/.vtt file.
    External(&'a Path),
}

/// Converts subtitles to WebVTT (cached), which the TV client renders itself.
pub async fn get_vtt(state: &AppState, src: Source<'_>) -> anyhow::Result<String> {
    let (file, index) = match src {
        Source::Embedded(p, i) => (p, i as i64),
        Source::External(p) => (p, -1),
    };
    let md = tokio::fs::metadata(file).await?;
    let key = util::stable_hash(&[
        file.to_string_lossy().as_bytes(),
        &util::mtime_secs(&md).to_le_bytes(),
        &md.len().to_le_bytes(),
        &index.to_le_bytes(),
    ]);
    let dir = state.cache_dir.join("subs");
    tokio::fs::create_dir_all(&dir).await?;
    let out = dir.join(format!("{key:016x}.vtt"));
    if let Ok(t) = tokio::fs::read_to_string(&out).await {
        return Ok(t);
    }
    let _permit = state.subs_sem.acquire().await?;

    let vtt = if index >= 0 {
        // Embedded tracks are interleaved through the whole file, so this reads all of it.
        run_ffmpeg(state, &dir, &ffmpeg::file_arg(file), Some(index as usize)).await?
    } else {
        let text = decode_text(&tokio::fs::read(file).await?);
        match library::ext_of(file).as_str() {
            "vtt" => text,
            "srt" => srt_to_vtt(&text),
            ext => {
                let input = dir.join(format!("{key:016x}.{}.{ext}", util::random_id()));
                tokio::fs::write(&input, text.as_bytes()).await?;
                let r = run_ffmpeg(state, &dir, &ffmpeg::file_arg(&input), None).await;
                let _ = tokio::fs::remove_file(&input).await;
                r?
            }
        }
    };
    tokio::fs::write(&out, vtt.as_bytes()).await?;
    Ok(vtt)
}

async fn run_ffmpeg(state: &AppState, dir: &Path, input: &str, index: Option<usize>) -> anyhow::Result<String> {
    let tmp = dir.join(format!("{}.vtt.tmp", util::random_id()));
    let mut cmd = ffmpeg::command(&state.tools().ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i", input]);
    if let Some(i) = index {
        cmd.args(["-map", &format!("0:{i}")]);
    }
    cmd.args(["-c:s", "webvtt", "-f", "webvtt"]).arg(&tmp).stdout(Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(900), cmd.output())
        .await
        .context("Извлечение субтитров заняло слишком много времени")??;
    let text = tokio::fs::read_to_string(&tmp).await.unwrap_or_default();
    let _ = tokio::fs::remove_file(&tmp).await;
    if !out.status.success() || text.is_empty() {
        bail!("Не удалось извлечь субтитры: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(text)
}

/// UTF-8/UTF-16 by BOM, otherwise UTF-8 if valid, otherwise Windows-1251 (typical for Russian .srt).
pub fn decode_text(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if b.starts_with(&[0xFF, 0xFE]) {
        return encoding_rs::UTF_16LE.decode(b).0.into_owned();
    }
    if b.starts_with(&[0xFE, 0xFF]) {
        return encoding_rs::UTF_16BE.decode(b).0.into_owned();
    }
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::WINDOWS_1251.decode(b).0.into_owned(),
    }
}

pub fn srt_to_vtt(s: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for line in s.lines() {
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp1251_srt() {
        let bytes = encoding_rs::WINDOWS_1251.encode("1\n00:00:01,000 --> 00:00:02,500\nПривет\n").0;
        let vtt = srt_to_vtt(&decode_text(&bytes));
        assert!(vtt.starts_with("WEBVTT"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.500"));
        assert!(vtt.contains("Привет"));
    }
}
