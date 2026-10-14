//! JDK Flight Recorder (JFR) import.
//!
//! [`parse_file`] / [`parse_bytes`] convert a recording into an [`openprofiler_model::Profile`]:
//! chunks are parsed in parallel (bounded batches, so memory stays proportional to a few chunks
//! plus the merged profile), and the per-chunk results are merged in file order. The mapping from
//! JFR events to sample kinds and telemetry series is documented in [`convert`].
//!
//! [`JfrReader`] gives generic, metadata-driven access to every event of a recording, for event
//! browsers and ad-hoc analysis.
//!
//! Supported: file format 2.x (JDK 11 and later), compressed and uncompressed integers, multiple
//! chunks, streaming ("flush") chunks with several constant-pool records, and a trailing chunk
//! that is still being written (it is parsed as far as possible and reported as a warning in
//! `meta.properties["jfr.warnings"]`).

mod chunk;
pub mod convert;
mod metadata;
mod reader;
mod threaddump;

pub use chunk::{
    Chunk, ChunkHeader, Event, EventField, EventType, JfrReader, StackFrame, StackTrace, ThreadRef, Value,
};

use anyhow::{Result, bail};
use openprofiler_model::Profile;
use rayon::prelude::*;
use std::path::Path;

/// Parses a JFR recording from disk (memory-mapped).
pub fn parse_file(path: &Path) -> Result<Profile> {
    let reader = JfrReader::open(path)?;
    parse_bytes(reader.bytes())
}

/// Parses an in-memory JFR recording.
pub fn parse_bytes(data: &[u8]) -> Result<Profile> {
    let headers = chunk::chunk_headers(data)?;
    let n = headers.len();
    let mut merger = convert::Merger::new();
    let batch = rayon::current_num_threads().max(1) * 2;
    let mut first_error = None;
    for h in headers.iter().filter(|h| h.truncated) {
        merger.warn(format!("chunk at {}: truncated to {} bytes", h.offset, h.size));
    }
    let covered = headers.last().map_or(0, |h| h.offset + h.size);
    if covered < data.len() {
        merger.warn(format!("ignored {} trailing bytes at offset {covered}", data.len() - covered));
    }
    for (b, group) in headers.chunks(batch).enumerate() {
        let outs: Vec<Result<convert::ChunkOut>> = group
            .par_iter()
            .map(|h| Chunk::parse(data, h.clone()).map(|c| convert::convert_chunk(&c)))
            .collect();
        for (i, out) in outs.into_iter().enumerate() {
            match out {
                Ok(o) => merger.add(o),
                Err(e) => {
                    let idx = b * batch + i;
                    let msg = format!("chunk {}/{n}: {e:#}", idx + 1);
                    first_error.get_or_insert_with(|| msg.clone());
                    merger.warn(msg);
                }
            }
        }
    }
    if merger.chunks() == 0 {
        bail!("no readable chunks: {}", first_error.unwrap_or_default());
    }
    Ok(merger.finish())
}

/// Returns true if `data` starts like a JFR file.
pub fn sniff(data: &[u8]) -> bool {
    data.starts_with(b"FLR\0")
}
