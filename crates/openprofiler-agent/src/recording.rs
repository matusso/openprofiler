//! Offline `.oplink` recordings: the agent-link frames a live daemon would have received,
//! written to a file. Layout: `OPRFLINK` magic, a `RecordingHeader` frame, then `DataBatch`
//! frames (currently one, covering everything retained).

use crate::Agent;
use openprofiler_proto::{FrameType, RECORDING_MAGIC, encode_frame, v1};
use std::io::Write;
use std::path::Path;

pub fn write(a: &Agent, path: &Path) -> std::io::Result<()> {
    let hello = crate::server::hello(a);
    crate::events::drain_monitors(a);
    crate::probes::drain(a);
    let batch = a.collector.lock().cut_batch(&v1::Watermark::default(), true, false);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("oplink.tmp");
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        f.write_all(RECORDING_MAGIC)?;
        let header = v1::RecordingHeader { hello: Some(hello), created: crate::collector::now_nanos() };
        f.write_all(&encode_frame(FrameType::RecordingHeader, &header))?;
        if let Some(b) = batch {
            f.write_all(&encode_frame(FrameType::DataBatch, &b))?;
        }
        f.flush()?;
    }
    std::fs::rename(&tmp, path)
}
