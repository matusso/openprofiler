//! File browsing for the Open dialog: lists a directory on the daemon's machine, marking what
//! the engine can open. Paths are those of the machine running the daemon, like `/sessions/open`.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Most entries listed for one directory.
const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub dir: bool,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Modification time, seconds since the Unix epoch.
    pub modified: Option<u64>,
    /// What the engine opens it as: `profile`, `heap`, `code`, or none.
    pub kind: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    /// The listed directory (canonical).
    pub path: String,
    pub parent: Option<String>,
    /// The user's home directory, for a shortcut.
    pub home: Option<String>,
    /// Directories first, then files, each sorted by name.
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

/// What a file name looks like to the engine (by extension; the content decides when opening).
pub fn kind_of(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".hprof") || n.ends_with(".hprof.gz") || n.ends_with(".phd") || n.ends_with(".phd.gz") {
        return Some("heap");
    }
    let ext = n.rsplit_once('.').map(|(_, e)| e)?;
    Some(match ext {
        "jfr" | "opsnap" | "oplink" => "profile",
        "apk" | "apks" | "apkm" | "xapk" | "aab" | "dex" | "jar" | "war" | "ear" | "jmod" | "aar"
        | "class" => "code",
        _ => return None,
    })
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// Lists `path` (a file lists its directory); the home directory when `None` or empty. Hidden
/// entries (`.name`) only with `hidden`.
pub fn list(path: Option<&str>, hidden: bool) -> Result<Listing> {
    let start = match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => expand(p),
        None => home().or_else(|| std::env::current_dir().ok()).context("no home directory")?,
    };
    let mut dir =
        std::fs::canonicalize(&start).with_context(|| format!("cannot open {}", start.display()))?;
    if !dir.is_dir() {
        dir = dir.parent().map(Path::to_path_buf).context("no parent directory")?;
    }
    let mut entries = vec![];
    let mut truncated = false;
    for e in std::fs::read_dir(&dir).with_context(|| format!("cannot list {}", dir.display()))? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if !hidden && name.starts_with('.') {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        // Follow symlinks, so linked directories can be entered.
        let meta = std::fs::metadata(e.path()).or_else(|_| e.metadata());
        let Ok(meta) = meta else { continue };
        let is_dir = meta.is_dir();
        entries.push(Entry {
            kind: if is_dir { None } else { kind_of(&name) },
            path: e.path().to_string_lossy().into_owned(),
            name,
            dir: is_dir,
            size: if is_dir { 0 } else { meta.len() },
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
        });
    }
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(Listing {
        parent: dir.parent().map(|p| p.to_string_lossy().into_owned()),
        home: home().map(|h| h.to_string_lossy().into_owned()),
        path: dir.to_string_lossy().into_owned(),
        entries,
        truncated,
    })
}

/// `~` and `~/x` to the home directory.
fn expand(p: &str) -> PathBuf {
    match (p.strip_prefix('~'), home()) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => {
            h.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_directories_first() {
        let dir = std::env::temp_dir().join(format!("op-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.jar"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"xy").unwrap();
        std::fs::write(dir.join(".hidden"), b"").unwrap();
        let l = list(Some(dir.to_str().unwrap()), false).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["sub", "a.jar", "b.txt"]);
        assert_eq!(l.entries[1].kind, Some("code"));
        assert_eq!(l.entries[2].size, 2);
        assert!(l.parent.is_some());
        // A file lists its directory; hidden entries on request.
        let l = list(Some(dir.join("a.jar").to_str().unwrap()), true).unwrap();
        assert_eq!(l.entries.len(), 4);
        assert!(list(Some("/definitely/not/here"), false).is_err());
        assert_eq!(kind_of("heap.hprof.gz"), Some("heap"));
        assert_eq!(kind_of("heapdump.20261017.123456.1.0001.phd"), Some("heap"));
        assert_eq!(kind_of("x.JFR"), Some("profile"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
