//! File loaders for each supported format.

use crate::engine::{FileFormat, Loader, Session, SessionState};
use anyhow::Result;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct DefaultLoader;

/// Directory for derived data of `file` (`<file>.ember-idx/`), falling back to the user cache
/// directory when the file's directory is not writable.
pub fn index_dir(file: &Path) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".ember-idx");
    let beside = file.with_file_name(&name);
    if std::fs::create_dir_all(&beside).is_ok() {
        return beside;
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let hash = xxh(file.to_string_lossy().as_bytes());
    let dir = base.join("ember").join(format!("{hash:016x}-{}", name.to_string_lossy()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn xxh(b: &[u8]) -> u64 {
    // FNV-1a; only used to name cache directories.
    b.iter().fold(0xcbf29ce484222325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3))
}

impl Loader for DefaultLoader {
    fn load(&self, format: FileFormat, path: &Path, session: &Session) -> Result<()> {
        match format {
            FileFormat::Ember => {
                let reader = ember_snapshot::SnapshotReader::open(path)?;
                let profile = reader.read_profile()?;
                session.set_profile(Arc::new(RwLock::new(profile)));
                if reader.info.has_heap() {
                    session.set_state(SessionState::Loading, Some(0.5), None);
                    let dir = index_dir(path);
                    let hprof = dir.join("heap.hprof");
                    if !hprof.exists() {
                        reader.extract_heap(&hprof)?;
                    }
                    crate::heap::load_hprof(&hprof, session)?;
                }
                Ok(())
            }
            FileFormat::Jfr => {
                let profile = ember_jfr::parse_file(path)?;
                session.set_profile(Arc::new(RwLock::new(profile)));
                Ok(())
            }
            FileFormat::Hprof | FileFormat::HprofGz => crate::heap::load_hprof(path, session),
            FileFormat::EmberLink => {
                let profile = ember_link::replay_recording(path)?;
                session.set_profile(Arc::new(RwLock::new(profile)));
                Ok(())
            }
        }
    }
}
