//! Live HLS sessions: one ffmpeg process per playing client, writing segments
//! into its own temp directory. ffmpeg is suspended when it gets too far ahead
//! of the player so a remux of a 60 GB file does not fill the disk.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use axum::{
    body::Body,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Child,
    time::sleep,
};
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use crate::{
    ffmpeg,
    util::{ApiError, ApiResult, random_id},
};

/// Pause ffmpeg once this many segments are ready beyond the player position (~80 s).
const PAUSE_AHEAD: i64 = 20;
const RESUME_AHEAD: i64 = 10;
const KEEP_BEHIND: i64 = 3;
/// Segments listed in the playlist beyond the last one the player fetched.
const PLAYLIST_LOOKAHEAD: i64 = 10;
/// Before the first segment is fetched, list only a few so live-mode players start at 0.
const INITIAL_SEGMENTS: i64 = 3;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const START_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Session {
    pub id: String,
    pub client: String,
    pub title: String,
    pub mode: &'static str,
    pub encoder: String,
    pub start: f64,
    /// Length of the output when segment boundaries are known in advance (transcode/audio):
    /// the playlist is then served as a complete VOD list instead of a growing EVENT one.
    vod_duration: Option<f64>,
    created: Instant,
    dir: PathBuf,
    pid: Option<u32>,
    child: Mutex<Child>,
    last_access: Mutex<Instant>,
    last_segment: AtomicI64,
    cleaned: AtomicI64,
    suspended: AtomicBool,
    finished: AtomicBool,
    log: Arc<Mutex<VecDeque<String>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    id: String,
    client: String,
    title: String,
    mode: &'static str,
    encoder: String,
    start: f64,
    age_secs: u64,
    position_secs: f64,
    suspended: bool,
    finished: bool,
}

impl Session {
    pub fn touch(&self) {
        *self.last_access.lock().unwrap() = Instant::now();
    }

    fn seg_path(&self, n: i64) -> PathBuf {
        self.dir.join(format!("seg{n:05}.ts"))
    }

    /// Some(success) once ffmpeg has exited.
    fn poll_exit(&self) -> Option<bool> {
        let status = self.child.lock().unwrap().try_wait();
        match status {
            Ok(Some(st)) => {
                self.finished.store(true, SeqCst);
                Some(st.success())
            }
            Ok(None) => None,
            Err(_) => {
                self.finished.store(true, SeqCst);
                Some(false)
            }
        }
    }

    fn set_suspended(&self, suspend: bool) {
        if self.suspended.swap(suspend, SeqCst) != suspend
            && let Some(pid) = self.pid {
                debug!(session = %self.id, suspend, "throttle ffmpeg");
                proc_ctl::set_suspended(pid, suspend);
            }
    }

    fn kill(&self) {
        let _ = self.child.lock().unwrap().start_kill();
    }

    fn log_tail(&self) -> String {
        let log = self.log.lock().unwrap();
        log.iter().rev().take(6).rev().cloned().collect::<Vec<_>>().join("\n")
    }

    fn throttle(&self) {
        if self.finished.load(SeqCst) {
            return;
        }
        let last = self.last_segment.load(SeqCst).max(0);
        if !self.suspended.load(SeqCst) {
            if self.seg_path(last + PAUSE_AHEAD).exists() {
                self.set_suspended(true);
            }
        } else if !self.seg_path(last + RESUME_AHEAD).exists() {
            self.set_suspended(false);
        }
    }

    fn cleanup_played(&self) {
        let upto = self.last_segment.load(SeqCst) - KEEP_BEHIND;
        let mut c = self.cleaned.load(SeqCst);
        while c < upto {
            let _ = std::fs::remove_file(self.seg_path(c));
            c += 1;
        }
        self.cleaned.store(c, SeqCst);
    }

    pub async fn serve(&self, file: &str) -> ApiResult<Response> {
        self.touch();
        if file == "index.m3u8" {
            return self.serve_playlist().await;
        }
        let n = file
            .strip_prefix("seg")
            .and_then(|r| r.strip_suffix(".ts"))
            .and_then(|n| n.parse::<i64>().ok())
            .ok_or_else(|| ApiError::not_found("Нет такого файла"))?;
        self.serve_segment(n).await
    }

    async fn serve_playlist(&self) -> ApiResult<Response> {
        let body = match self.vod_duration {
            Some(d) => vod_playlist(d),
            None => {
                let path = self.dir.join("index.m3u8");
                let deadline = Instant::now() + Duration::from_secs(60);
                let text = loop {
                    if let Ok(t) = tokio::fs::read_to_string(&path).await
                        && t.contains("#EXTINF")
                    {
                        break t;
                    }
                    if self.finished.load(SeqCst) || Instant::now() > deadline {
                        return Err(ApiError::internal(format!("Поток не готов. {}", self.log_tail())));
                    }
                    sleep(Duration::from_millis(200)).await;
                };
                let limit = match self.last_segment.load(SeqCst) {
                    n if n < 0 => INITIAL_SEGMENTS,
                    n => n + 1 + PLAYLIST_LOOKAHEAD,
                };
                limit_playlist(&text, limit as usize)
            }
        };
        let mut resp = body.into_response();
        let h = resp.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.apple.mpegurl"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-store"));
        Ok(resp)
    }

    async fn serve_segment(&self, n: i64) -> ApiResult<Response> {
        self.last_segment.store(n, SeqCst);
        if self.suspended.load(SeqCst) && !self.seg_path(n + RESUME_AHEAD).exists() {
            self.set_suspended(false);
        }
        let path = self.seg_path(n);
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            // With -hls_flags temp_file a segment appears under its final name only when complete.
            if let Ok(f) = tokio::fs::File::open(&path).await {
                let len = f.metadata().await?.len();
                let mut resp = Body::from_stream(ReaderStream::with_capacity(f, 256 * 1024)).into_response();
                let h = resp.headers_mut();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp2t"));
                h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
                return Ok(resp);
            }
            if n < self.cleaned.load(SeqCst) || self.finished.load(SeqCst) || Instant::now() > deadline {
                return Err(ApiError::not_found("Сегмент недоступен"));
            }
            self.touch();
            sleep(Duration::from_millis(150)).await;
        }
    }
}

/// Complete playlist for output cut into fixed SEGMENT_SECONDS pieces (keyframes are forced
/// at those points). Players treat it as an ordinary film: they start at 0 and know the end,
/// unlike a growing EVENT list, which browsers handle like live TV (start near the edge, stall).
/// Segments not encoded yet are simply waited for when requested.
fn vod_playlist(duration: f64) -> String {
    let seg = crate::plan::SEGMENT_SECONDS as f64;
    let count = ((duration / seg).ceil() as usize).max(1);
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n",
        crate::plan::SEGMENT_SECONDS + 1
    );
    for i in 0..count {
        let len = if i + 1 == count { (duration - seg * i as f64).max(0.1) } else { seg };
        out.push_str(&format!("#EXTINF:{len:.3},\nseg{i:05}.ts\n"));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// Keeps at most `max_segments` segments and adds EXT-X-START so players begin at 0.
fn limit_playlist(text: &str, max_segments: usize) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut count = 0;
    for line in text.lines() {
        if line.starts_with("#EXTINF") && count >= max_segments {
            break; // also drops #EXT-X-ENDLIST: the player keeps polling
        }
        out.push_str(line);
        out.push('\n');
        if line == "#EXTM3U" {
            out.push_str("#EXT-X-START:TIME-OFFSET=0\n");
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            count += 1;
        }
    }
    out
}

pub struct SpawnSpec {
    pub ffmpeg: PathBuf,
    pub client: String,
    pub title: String,
    pub mode: &'static str,
    pub encoder: String,
    pub start: f64,
    pub vod_duration: Option<f64>,
    pub args: Vec<String>,
}

pub struct Sessions {
    root: PathBuf,
    map: Mutex<HashMap<String, Arc<Session>>>,
}

impl Sessions {
    pub fn new(root: PathBuf) -> Self {
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
        Self { root, map: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.map.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self
            .map
            .lock()
            .unwrap()
            .values()
            .map(|s| SessionInfo {
                id: s.id.clone(),
                client: s.client.clone(),
                title: s.title.clone(),
                mode: s.mode,
                encoder: s.encoder.clone(),
                start: s.start,
                age_secs: s.created.elapsed().as_secs(),
                position_secs: s.start + (s.last_segment.load(SeqCst).max(0) * crate::plan::SEGMENT_SECONDS as i64) as f64,
                suspended: s.suspended.load(SeqCst),
                finished: s.finished.load(SeqCst),
            })
            .collect();
        list.sort_by_key(|s| s.age_secs);
        list
    }

    pub async fn stop(&self, id: &str) {
        let removed = self.map.lock().unwrap().remove(id);
        let Some(s) = removed else { return };
        s.kill();
        for _ in 0..30 {
            if s.poll_exit().is_some() {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        // Windows may keep files locked for a moment after the process exits.
        for _ in 0..10 {
            match tokio::fs::remove_dir_all(&s.dir).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => sleep(Duration::from_millis(300)).await,
                _ => break,
            }
        }
        info!(session = %id, "stream stopped");
    }

    pub async fn stop_client(&self, client: &str) {
        let ids: Vec<String> = self.map.lock().unwrap().values().filter(|s| s.client == client).map(|s| s.id.clone()).collect();
        for id in ids {
            self.stop(&id).await;
        }
    }

    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.map.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.stop(&id).await;
        }
    }

    async fn enforce_limit(&self, keep: usize) {
        loop {
            let oldest = {
                let map = self.map.lock().unwrap();
                if map.len() <= keep {
                    return;
                }
                map.values().min_by_key(|s| s.created).map(|s| s.id.clone())
            };
            match oldest {
                Some(id) => self.stop(&id).await,
                None => return,
            }
        }
    }

    /// Starts ffmpeg and waits until the first segment is ready (or ffmpeg fails).
    pub async fn spawn(&self, spec: SpawnSpec, max_sessions: usize) -> anyhow::Result<Arc<Session>> {
        self.enforce_limit(max_sessions.max(1) - 1).await;
        let id = random_id();
        let dir = self.root.join(&id);
        tokio::fs::create_dir_all(&dir).await?;
        debug!(session = %id, args = ?spec.args, "spawn ffmpeg");
        let mut child = ffmpeg::command(&spec.ffmpeg)
            .args(&spec.args)
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Не удалось запустить ffmpeg (установлен ли он?)")?;

        let log = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr.take() {
            let (log, sid) = (log.clone(), id.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!(session = %sid, "ffmpeg: {line}");
                    let mut q = log.lock().unwrap();
                    if q.len() >= 40 {
                        q.pop_front();
                    }
                    q.push_back(line);
                }
            });
        }

        let session = Arc::new(Session {
            id: id.clone(),
            client: spec.client,
            title: spec.title,
            mode: spec.mode,
            encoder: spec.encoder,
            start: spec.start,
            vod_duration: spec.vod_duration,
            created: Instant::now(),
            dir: dir.clone(),
            pid: child.id(),
            child: Mutex::new(child),
            last_access: Mutex::new(Instant::now()),
            last_segment: AtomicI64::new(-1),
            cleaned: AtomicI64::new(0),
            suspended: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            log,
        });
        self.map.lock().unwrap().insert(id.clone(), session.clone());

        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            session.throttle();
            if playlist_ready(&dir).await {
                return Ok(session);
            }
            if session.poll_exit().is_some() {
                if playlist_ready(&dir).await {
                    return Ok(session);
                }
                sleep(Duration::from_millis(150)).await; // let the stderr reader catch up
                let tail = session.log_tail();
                self.stop(&id).await;
                bail!("ffmpeg завершился с ошибкой: {}", if tail.is_empty() { "нет вывода" } else { &tail });
            }
            if Instant::now() > deadline {
                self.stop(&id).await;
                bail!("ffmpeg не успел подготовить поток");
            }
            sleep(Duration::from_millis(150)).await;
        }
    }

    pub async fn monitor(self: Arc<Self>) {
        // Short interval: a stream-copy remux can produce dozens of segments per second.
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tick.tick().await;
            let sessions: Vec<Arc<Session>> = self.map.lock().unwrap().values().cloned().collect();
            for s in sessions {
                if s.last_access.lock().unwrap().elapsed() > IDLE_TIMEOUT {
                    info!(session = %s.id, "stream idle, stopping");
                    self.stop(&s.id).await;
                    continue;
                }
                let was_finished = s.finished.load(SeqCst);
                if let Some(ok) = s.poll_exit()
                    && !was_finished && !ok {
                        warn!(session = %s.id, "ffmpeg failed: {}", s.log_tail());
                    }
                s.throttle();
                s.cleanup_played();
            }
        }
    }
}

async fn playlist_ready(dir: &Path) -> bool {
    tokio::fs::read_to_string(dir.join("index.m3u8"))
        .await
        .is_ok_and(|t| t.contains("#EXTINF"))
}

mod proc_ctl {
    #[cfg(unix)]
    pub fn set_suspended(pid: u32, suspend: bool) {
        let sig = if suspend { libc::SIGSTOP } else { libc::SIGCONT };
        // SAFETY: plain signal to a child process we own.
        unsafe {
            libc::kill(pid as libc::pid_t, sig);
        }
    }

    /// Windows has no SIGSTOP: suspend/resume every thread of the process.
    #[cfg(windows)]
    pub fn set_suspended(pid: u32, suspend: bool) {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
            System::{
                Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next},
                Threading::{OpenThread, ResumeThread, SuspendThread, THREAD_SUSPEND_RESUME},
            },
        };
        // SAFETY: documented Win32 calls; every handle opened here is closed here.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return;
            }
            let mut te: THREADENTRY32 = std::mem::zeroed();
            te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut ok = Thread32First(snap, &mut te);
            while ok != 0 {
                if te.th32OwnerProcessID == pid {
                    let h = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID);
                    if !h.is_null() {
                        if suspend {
                            SuspendThread(h);
                        } else {
                            ResumeThread(h);
                        }
                        CloseHandle(h);
                    }
                }
                ok = Thread32Next(snap, &mut te);
            }
            CloseHandle(snap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::limit_playlist;

    const PL: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXTINF:4.0,\nseg00000.ts\n#EXTINF:4.0,\nseg00001.ts\n#EXTINF:2.0,\nseg00002.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn truncates_and_hides_endlist() {
        let out = limit_playlist(PL, 2);
        assert!(out.contains("seg00001.ts") && !out.contains("seg00002.ts") && !out.contains("ENDLIST"));
        assert!(out.contains("#EXT-X-START:TIME-OFFSET=0"));
    }

    #[test]
    fn vod_playlist_covers_whole_duration() {
        let pl = super::vod_playlist(10.5);
        assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:VOD") && pl.ends_with("#EXT-X-ENDLIST\n"));
        assert!(pl.contains("seg00002.ts") && !pl.contains("seg00003.ts"));
        assert!(pl.contains("#EXTINF:2.500,"));
    }

    #[test]
    fn full_list_keeps_endlist() {
        assert!(limit_playlist(PL, 10).contains("#EXT-X-ENDLIST"));
    }
}
