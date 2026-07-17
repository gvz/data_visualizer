use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

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

/// Stub — implemented in Task 3.
pub fn start_recording(
    _output_dir: &std::path::Path,
    _registry: &crate::config::ChannelRegistry,
    _schema_bytes: Vec<u8>,
    _receiver: crossbeam_channel::Receiver<RecordMsg>,
) -> anyhow::Result<RecordHandle> {
    Err(anyhow::anyhow!("start_recording: not yet implemented"))
}
