//! Recording and playback of MCAP telemetry.
//!
//! Playback is memory-mapped and decodes chunks on demand: each recording is
//! `mmap`ed and indexed by chunk time-bounds, and a read decodes only the few
//! chunks overlapping the requested window. Resident memory is bounded by the
//! decoded-chunk cache (512 MB default), the per-channel min/max envelope
//! (16384 buckets/channel) used for near-whole-file overviews, and retained
//! text — so recordings larger than RAM play back. Caveats: text/log channels
//! are still retained in full, and very-wide zooms serve a decimated min/max
//! overview rather than exact samples.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

pub mod lazy;
pub mod mqtt_schema;
pub mod playback;
pub mod queue;
pub mod writer;

pub use queue::{record_channel, RecordMsg, QUEUE_CAP};

pub struct RecordHandle {
    pub gap_count: Arc<AtomicU64>,
    pub record_failed: Arc<AtomicBool>,
    stop_tx: crossbeam_channel::Sender<()>,
}

impl Drop for RecordHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
    }
}

pub fn start_recording(
    output_dir: &std::path::Path,
    registry: &crate::config::ChannelRegistry,
    schema_bytes: Vec<u8>,
    receiver: crossbeam_channel::Receiver<RecordMsg>,
) -> anyhow::Result<RecordHandle> {
    let gap_count = Arc::new(AtomicU64::new(0));
    let record_failed = Arc::new(AtomicBool::new(false));
    let stop_tx = writer::spawn_recorder(
        output_dir,
        registry,
        &schema_bytes,
        receiver,
        gap_count.clone(),
        record_failed.clone(),
    )?;
    Ok(RecordHandle {
        gap_count,
        record_failed,
        stop_tx,
    })
}
