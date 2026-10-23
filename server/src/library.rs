use std::{
    cmp::Ordering,
    iter::Peekable,
    path::{Path, PathBuf},
    str::Chars,
};

use serde::Serialize;

use crate::{config::Config, util::ApiError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Dir,
    Video,
    Audio,
    Image,
    Subtitle,
    Other,
}

const VIDEO: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "asf", "flv", "f4v", "ts", "m2ts", "mts",
    "m2t", "mpg", "mpeg", "vob", "3gp", "ogv", "divx", "xvid", "rm", "rmvb", "mxf", "dv",
];
const AUDIO: &[&str] = &[
    "mp3", "flac", "aac", "m4a", "ogg", "oga", "opus", "wav", "wma", "ape", "alac", "aiff", "aif",
    "dsf", "dff", "mka", "wv", "ac3", "dts", "mpc", "tta",
];
const IMAGE: &[&str] = &["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff"];
pub const SUBTITLE: &[&str] = &["srt", "ass", "ssa", "vtt"];
const SKIP_DIRS: &[&str] = &["$RECYCLE.BIN", "System Volume Information", "lost+found"];

pub fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

pub fn kind_of(p: &Path) -> Kind {
    let ext = ext_of(p);
    let e = ext.as_str();
    if VIDEO.contains(&e) {
        Kind::Video
    } else if AUDIO.contains(&e) {
        Kind::Audio
    } else if IMAGE.contains(&e) {
        Kind::Image
    } else if SUBTITLE.contains(&e) {
        Kind::Subtitle
    } else {
        Kind::Other
    }
}

/// Splits a client-supplied relative path, rejecting anything that could escape the root.
pub fn split_rel(rel: &str) -> Result<Vec<&str>, ApiError> {
    let mut out = Vec::new();
    for c in rel.split(['/', '\\']) {
        match c {
            "" | "." => continue,
            ".." => return Err(ApiError::bad_request("Недопустимый путь")),
            c if c.contains(':') => return Err(ApiError::bad_request("Недопустимый путь")),
            c => out.push(c),
        }
    }
    Ok(out)
}

pub fn normalize_rel(rel: &str) -> Result<String, ApiError> {
    Ok(split_rel(rel)?.join("/"))
}

/// Maps (root id, relative path) to an absolute path that is guaranteed to live inside the root.
pub fn resolve(cfg: &Config, root: &str, rel: &str) -> Result<PathBuf, ApiError> {
    let r = cfg
        .roots
        .iter()
        .find(|r| r.id == root)
        .ok_or_else(|| ApiError::not_found("Корневая папка не найдена"))?;
    let mut p = r.path.clone();
    for c in split_rel(rel)? {
        p.push(c);
    }
    let canon_root = std::fs::canonicalize(&r.path)
        .map_err(|_| ApiError::not_found("Корневая папка недоступна"))?;
    let canon = std::fs::canonicalize(&p).map_err(|_| ApiError::not_found("Файл не найден"))?;
    if !canon.starts_with(&canon_root) {
        return Err(ApiError::forbidden("Путь вне медиапапки"));
    }
    Ok(p)
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub modified: u64,
}

fn is_hidden(name: &str, _md: &std::fs::Metadata) -> bool {
    if name.starts_with('.') || SKIP_DIRS.contains(&name) {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        if _md.file_attributes() & (HIDDEN | SYSTEM) != 0 {
            return true;
        }
    }
    false
}

pub fn list_dir(abs: &Path, rel: &str) -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for de in std::fs::read_dir(abs)?.flatten() {
        let name = de.file_name().to_string_lossy().into_owned();
        let Ok(md) = std::fs::metadata(de.path()) else { continue };
        if is_hidden(&name, &md) {
            continue;
        }
        let kind = if md.is_dir() { Kind::Dir } else { kind_of(&de.path()) };
        if matches!(kind, Kind::Other | Kind::Subtitle) {
            continue;
        }
        let path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        entries.push(Entry {
            size: if md.is_dir() { 0 } else { md.len() },
            modified: crate::util::mtime_secs(&md),
            name,
            path,
            kind,
        });
    }
    entries.sort_by(|a, b| {
        (b.kind == Kind::Dir)
            .cmp(&(a.kind == Kind::Dir))
            .then_with(|| natural_cmp(&a.name, &b.name))
    });
    Ok(entries)
}

/// Case-insensitive comparison where digit runs compare numerically ("Ep 2" < "Ep 10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (na, nb) = (take_num(&mut ai), take_num(&mut bi));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

fn take_num(it: &mut Peekable<Chars>) -> String {
    let mut s = String::new();
    while let Some(&c) = it.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        s.push(c);
        it.next();
    }
    s
}

#[derive(Debug, Clone)]
pub struct ExtSub {
    pub rel: String,
    pub label: String,
    pub ext: String,
}

/// Subtitle files next to a video whose name starts with the video's name (movie.rus.srt).
pub fn external_subs(video_abs: &Path, video_rel: &str) -> Vec<ExtSub> {
    let (Some(dir), Some(stem)) = (video_abs.parent(), video_abs.file_stem()) else {
        return Vec::new();
    };
    let stem = stem.to_string_lossy().to_lowercase();
    let rel_dir = match video_rel.rfind('/') {
        Some(i) => &video_rel[..i + 1],
        None => "",
    };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<ExtSub> = rd
        .flatten()
        .filter_map(|de| {
            let name = de.file_name().to_string_lossy().into_owned();
            let ext = ext_of(Path::new(&name));
            let lower = name.to_lowercase();
            if !SUBTITLE.contains(&ext.as_str()) || !lower.starts_with(&stem) {
                return None;
            }
            let middle = &name[stem.len()..name.len() - ext.len()];
            let label = middle.trim_matches(|c: char| c == '.' || c == '_' || c == '-' || c == ' ');
            Some(ExtSub {
                rel: format!("{rel_dir}{name}"),
                label: if label.is_empty() { "Внешние".into() } else { label.to_string() },
                ext,
            })
        })
        .collect();
    out.sort_by(|a, b| natural_cmp(&a.rel, &b.rel));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        assert_eq!(natural_cmp("Ep 2", "Ep 10"), Ordering::Less);
        assert_eq!(natural_cmp("ep 02", "Ep 2"), Ordering::Equal);
        assert_eq!(natural_cmp("a", "B"), Ordering::Less);
    }

    #[test]
    fn rejects_traversal() {
        assert!(split_rel("a/../b").is_err());
        assert!(split_rel("C:/Windows").is_err());
        assert_eq!(split_rel("/a//b/./c").unwrap(), vec!["a", "b", "c"]);
    }
}
