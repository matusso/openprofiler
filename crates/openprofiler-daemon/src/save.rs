//! Saving sessions as `.opsnap` snapshots.

use crate::engine::Session;
use anyhow::{Result, bail};
use std::path::Path;

/// Writes the session's profile (if any) and heap dump (if the session was opened from an HPROF
/// or a snapshot with a heap) to `dest`.
pub fn save_snapshot(session: &Session, dest: &Path) -> Result<()> {
    let profile = session.profile();
    let heap_file = session.heap().and_then(|h| crate::heap::hprof_path(&h));
    if profile.is_none() && heap_file.is_none() {
        bail!("session {} has nothing to save", session.id);
    }
    let empty = openprofiler_model::Profile::new("openprofiler");
    match profile {
        Some(p) => openprofiler_snapshot::write_snapshot(dest, &p.read(), heap_file.as_deref()),
        None => openprofiler_snapshot::write_snapshot(dest, &empty, heap_file.as_deref()),
    }
}
