use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Receiver;

use crate::config::ChannelRegistry;
use crate::record::queue::RecordMsg;

struct McapRecorder {
    writer: mcap::Writer<'static, BufWriter<File>>,
    channel_ids: HashMap<String, u16>,
    last_flush: Instant,
    sequence: u32,
}

impl McapRecorder {
    fn new(path: &Path, registry: &ChannelRegistry, schema_bytes: &[u8]) -> anyhow::Result<Self> {
        let file = BufWriter::new(File::create(path)?);
        let mut writer = mcap::Writer::new(file)?;

        let schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(schema_bytes.to_vec()),
        });

        let mut channel_ids = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();

        for id in registry.iter_ids() {
            let topic = registry.config(id).topic.clone();
            if seen.insert(topic.clone()) {
                let channel = mcap::Channel {
                    topic: topic.clone(),
                    schema: Some(schema.clone()),
                    message_encoding: "protobuf".to_string(),
                    metadata: BTreeMap::new(),
                };
                let channel_id = writer.add_channel(&channel)?;
                channel_ids.insert(topic, channel_id);
            }
        }

        Ok(Self {
            writer,
            channel_ids,
            last_flush: Instant::now(),
            sequence: 0,
        })
    }

    fn write_msg(&mut self, topic: &str, data: &[u8], log_time_ns: i64) -> anyhow::Result<()> {
        let Some(&channel_id) = self.channel_ids.get(topic) else {
            return Ok(()); // unknown topic, skip
        };

        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);

        self.writer.write_to_known_channel(
            &mcap::records::MessageHeader {
                channel_id,
                sequence: seq,
                log_time: log_time_ns as u64,
                publish_time: log_time_ns as u64,
            },
            data,
        )?;

        if self.last_flush.elapsed() >= Duration::from_secs(1) {
            self.writer.flush()?;
            self.last_flush = Instant::now();
        }

        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        self.writer.finish()?;
        Ok(())
    }
}

fn recorder_thread_fn(
    recorder: McapRecorder,
    record_rx: Receiver<RecordMsg>,
    stop_rx: crossbeam_channel::Receiver<()>,
    record_failed: Arc<AtomicBool>,
) {
    if let Err(e) = recorder_loop(recorder, record_rx, stop_rx) {
        eprintln!("recorder: write error: {e}");
        record_failed.store(true, Ordering::Relaxed);
    }
}

fn recorder_loop(
    mut recorder: McapRecorder,
    record_rx: Receiver<RecordMsg>,
    stop_rx: crossbeam_channel::Receiver<()>,
) -> anyhow::Result<()> {
    loop {
        crossbeam_channel::select! {
            recv(record_rx) -> result => match result {
                Ok((topic, data, ts)) => recorder.write_msg(&topic, &data, ts)?,
                Err(_) => break,
            },
            recv(stop_rx) -> _ => break,
        }
    }
    // Drain any messages that arrived before stop.
    while let Ok((topic, data, ts)) = record_rx.try_recv() {
        recorder.write_msg(&topic, &data, ts)?;
    }
    recorder.finish()
}

/// Called from `src/record/mod.rs::start_recording`.
pub(super) fn spawn_recorder(
    output_dir: &Path,
    registry: &ChannelRegistry,
    schema_bytes: &[u8],
    receiver: Receiver<RecordMsg>,
    _gap_count: Arc<AtomicU64>,
    record_failed: Arc<AtomicBool>,
) -> anyhow::Result<crossbeam_channel::Sender<()>> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = format!("recording_{secs}.mcap");
    let path = output_dir.join(filename);

    let recorder = McapRecorder::new(&path, registry, schema_bytes)?;
    let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let rf = record_failed.clone();
    std::thread::spawn(move || recorder_thread_fn(recorder, receiver, stop_rx, rf));
    Ok(stop_tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::record::queue::record_channel;
    use crate::record::start_recording;

    fn minimal_registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."x"]
topic = "accel"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#,
        )
        .unwrap()
    }

    #[test]
    fn roundtrip_write_read() {
        let dir = tempfile::tempdir().unwrap();
        let registry = minimal_registry();
        let schema_bytes = b"fake_fds_bytes".to_vec();

        let (tx, rx) = record_channel();
        let handle = start_recording(dir.path(), &registry, schema_bytes, rx).unwrap();

        tx.try_send((Arc::from("accel"), vec![0x01, 0x02], 1_000_000_000))
            .unwrap();
        tx.try_send((Arc::from("accel"), vec![0x03, 0x04], 2_000_000_000))
            .unwrap();

        // Drop sender and handle to stop recording and flush file.
        drop(tx);
        drop(handle);

        // Give recorder thread time to finish writing.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Find the written file.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "mcap").unwrap_or(false))
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one .mcap file");

        let mcap_path = entries[0].path();
        let bytes = std::fs::read(&mcap_path).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].channel.topic, "accel");
        assert_eq!(messages[0].data.as_ref(), &[0x01u8, 0x02]);
        assert_eq!(messages[0].log_time, 1_000_000_000u64);
        assert_eq!(messages[1].data.as_ref(), &[0x03u8, 0x04]);
        assert_eq!(messages[1].log_time, 2_000_000_000u64);
    }

    #[test]
    fn unknown_topic_messages_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let registry = minimal_registry();
        let (tx, rx) = record_channel();
        let handle = start_recording(dir.path(), &registry, vec![], rx).unwrap();

        // "gyro" is not in the registry — should be silently dropped
        tx.try_send((Arc::from("gyro"), vec![0xFF], 1_000)).unwrap();
        tx.try_send((Arc::from("accel"), vec![0x01], 2_000)).unwrap();
        drop(tx);
        drop(handle);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "mcap").unwrap_or(false))
            .collect();
        let bytes = std::fs::read(&entries[0].path()).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].channel.topic, "accel");
    }
}

