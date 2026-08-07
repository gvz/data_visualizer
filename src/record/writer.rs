use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Receiver;

use crate::config::ChannelRegistry;
use crate::record::queue::RecordMsg;

/// Everything needed to open a fresh recording part without borrowing the
/// registry into the recorder thread.
struct RecorderCfg {
    output_dir: PathBuf,
    session_secs: u64,
    schema_bytes: Vec<u8>,
    /// Distinct ZMQ/Proto topics to pre-seed as channels in every part.
    zmq_topics: Vec<String>,
    /// On-disk size limit; `None` = never rotate, single un-suffixed file.
    max_bytes: Option<u64>,
}

impl RecorderCfg {
    fn part_path(&self, part: u32) -> PathBuf {
        let name = if self.max_bytes.is_some() {
            format!("recording_{}_{:03}.mcap", self.session_secs, part)
        } else {
            format!("recording_{}.mcap", self.session_secs)
        };
        self.output_dir.join(name)
    }
}

struct McapRecorder {
    writer: mcap::Writer<'static, BufWriter<File>>,
    /// ZMQ/Proto channels: pre-seeded from the ChannelRegistry at startup.
    channel_ids: HashMap<String, u16>,
    /// DynamicProto (MQTT) channels: registered on first sight, separate namespace.
    dynamic_channel_ids: HashMap<String, u16>,
    last_flush: Instant,
    sequence: u32,
    path: PathBuf,
    max_bytes: Option<u64>,
    /// Set true by the flush block once the file on disk reaches `max_bytes`.
    over_limit: bool,
}

impl McapRecorder {
    fn open(cfg: &RecorderCfg, part: u32) -> anyhow::Result<Self> {
        let path = cfg.part_path(part);
        let file = BufWriter::new(File::create(&path)?);
        let mut writer = mcap::Writer::new(file)?;

        let schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(cfg.schema_bytes.clone()),
        });

        let mut channel_ids = HashMap::new();
        for topic in &cfg.zmq_topics {
            let channel = mcap::Channel {
                topic: topic.clone(),
                schema: Some(schema.clone()),
                message_encoding: "protobuf".to_string(),
                metadata: BTreeMap::new(),
            };
            let channel_id = writer.add_channel(&channel)?;
            channel_ids.insert(topic.clone(), channel_id);
        }

        Ok(Self {
            writer,
            channel_ids,
            dynamic_channel_ids: HashMap::new(),
            last_flush: Instant::now(),
            sequence: 0,
            path,
            max_bytes: cfg.max_bytes,
            over_limit: false,
        })
    }

    /// Write raw bytes to a known channel id, incrementing the sequence counter
    /// and flushing periodically.
    fn write_to_channel(&mut self, channel_id: u16, data: &[u8], log_time_ns: i64) -> anyhow::Result<()> {
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
        if let Some(limit) = self.max_bytes {
            // Check size every 128 messages to catch the limit without per-message stat cost.
            if self.sequence.is_multiple_of(128) {
                self.writer.flush()?;
                // A stat failure just skips the check — never abort a session.
                if std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) >= limit {
                    self.over_limit = true;
                }
            }
        }

        Ok(())
    }

    fn write_msg(&mut self, topic: &str, data: &[u8], log_time_ns: i64) -> anyhow::Result<()> {
        let Some(&channel_id) = self.channel_ids.get(topic) else {
            return Ok(()); // unknown topic, skip
        };
        self.write_to_channel(channel_id, data, log_time_ns)
    }

    /// Write a message whose protobuf schema is supplied inline. Registers a new
    /// MCAP channel (with its own schema) the first time a topic is seen.
    /// Uses `dynamic_channel_ids` exclusively — never shares namespace with `channel_ids`.
    fn write_dynamic(
        &mut self,
        topic: &str,
        schema_bytes: &[u8],
        data: &[u8],
        log_time_ns: i64,
    ) -> anyhow::Result<()> {
        if !self.dynamic_channel_ids.contains_key(topic) {
            let schema = Arc::new(mcap::Schema {
                name: topic.to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(schema_bytes.to_vec()),
            });
            let channel = mcap::Channel {
                topic: topic.to_string(),
                schema: Some(schema),
                message_encoding: "protobuf".to_string(),
                metadata: BTreeMap::new(),
            };
            let channel_id = self.writer.add_channel(&channel)?;
            self.dynamic_channel_ids.insert(topic.to_string(), channel_id);
        }
        let &channel_id = self.dynamic_channel_ids.get(topic).unwrap();
        self.write_to_channel(channel_id, data, log_time_ns)
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        self.writer.finish()?;
        Ok(())
    }
}

fn recorder_thread_fn(
    cfg: RecorderCfg,
    recorder: McapRecorder,
    record_rx: Receiver<RecordMsg>,
    stop_rx: crossbeam_channel::Receiver<()>,
    record_failed: Arc<AtomicBool>,
) {
    if let Err(e) = recorder_loop(cfg, recorder, record_rx, stop_rx) {
        eprintln!("recorder: write error: {e}");
        record_failed.store(true, Ordering::Relaxed);
    }
}

/// Finalize the current part and open the next one.
fn rotate(recorder: McapRecorder, cfg: &RecorderCfg, part: u32) -> anyhow::Result<McapRecorder> {
    recorder.finish()?;
    McapRecorder::open(cfg, part)
}

fn recorder_loop(
    cfg: RecorderCfg,
    mut recorder: McapRecorder,
    record_rx: Receiver<RecordMsg>,
    stop_rx: crossbeam_channel::Receiver<()>,
) -> anyhow::Result<()> {
    let mut part: u32 = 0;
    loop {
        crossbeam_channel::select! {
            recv(record_rx) -> result => match result {
                Ok(msg) => {
                    write_record(&mut recorder, msg)?;
                    if recorder.over_limit {
                        part += 1;
                        recorder = rotate(recorder, &cfg, part)?;
                    }
                }
                Err(_) => break,
            },
            recv(stop_rx) -> _ => break,
        }
    }
    // Drain any messages that arrived before stop.
    while let Ok(msg) = record_rx.try_recv() {
        write_record(&mut recorder, msg)?;
        if recorder.over_limit {
            part += 1;
            recorder = rotate(recorder, &cfg, part)?;
        }
    }
    recorder.finish()
}

/// Dispatch a queued frame to the recorder based on its variant.
fn write_record(recorder: &mut McapRecorder, msg: RecordMsg) -> anyhow::Result<()> {
    match msg {
        RecordMsg::Proto { topic, data, ts } => recorder.write_msg(&topic, &data, ts),
        RecordMsg::DynamicProto { topic, schema, data, ts } => {
            recorder.write_dynamic(&topic, &schema, &data, ts)
        }
    }
}

/// Called from `src/record/mod.rs::start_recording`.
pub(super) fn spawn_recorder(
    output_dir: &Path,
    registry: &ChannelRegistry,
    schema_bytes: &[u8],
    receiver: Receiver<RecordMsg>,
    _gap_count: Arc<AtomicU64>,
    record_failed: Arc<AtomicBool>,
    max_bytes: Option<u64>,
) -> anyhow::Result<crossbeam_channel::Sender<()>> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Distinct ZMQ/Proto topics to pre-seed in every part (MQTT topics register
    // dynamically on first sight, per part).
    let mut seen: HashSet<String> = HashSet::new();
    let mut zmq_topics: Vec<String> = Vec::new();
    for id in registry.iter_ids() {
        if let Some(topic) = registry.config(id).topic.clone() {
            if seen.insert(topic.clone()) {
                zmq_topics.push(topic);
            }
        }
    }

    let cfg = RecorderCfg {
        output_dir: output_dir.to_path_buf(),
        session_secs: secs,
        schema_bytes: schema_bytes.to_vec(),
        zmq_topics,
        max_bytes,
    };
    let recorder = McapRecorder::open(&cfg, 0)?;
    let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let rf = record_failed.clone();
    std::thread::spawn(move || recorder_thread_fn(cfg, recorder, receiver, stop_rx, rf));
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
        let handle = start_recording(dir.path(), &registry, schema_bytes, rx, None).unwrap();

        tx.try_send(RecordMsg::Proto { topic: Arc::from("accel"), data: vec![0x01, 0x02], ts: 1_000_000_000 })
            .unwrap();
        tx.try_send(RecordMsg::Proto { topic: Arc::from("accel"), data: vec![0x03, 0x04], ts: 2_000_000_000 })
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
        let handle = start_recording(dir.path(), &registry, vec![], rx, None).unwrap();

        // "gyro" is not in the registry — should be silently dropped
        tx.try_send(RecordMsg::Proto { topic: Arc::from("gyro"), data: vec![0xFF], ts: 1_000 }).unwrap();
        tx.try_send(RecordMsg::Proto { topic: Arc::from("accel"), data: vec![0x01], ts: 2_000 }).unwrap();
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

    #[test]
    fn dynamic_proto_registers_per_topic_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let registry = minimal_registry();
        let (tx, rx) = record_channel();
        // Shared schema empty: MQTT topics carry their own.
        let handle = start_recording(dir.path(), &registry, vec![], rx, None).unwrap();

        tx.try_send(RecordMsg::DynamicProto {
            topic: Arc::from("home/temp"),
            schema: Arc::from(b"schema_A".as_slice()),
            data: vec![0x0A],
            ts: 1_000,
        })
        .unwrap();
        tx.try_send(RecordMsg::DynamicProto {
            topic: Arc::from("home/door"),
            schema: Arc::from(b"schema_B".as_slice()),
            data: vec![0x0B],
            ts: 2_000,
        })
        .unwrap();
        // Same topic again must NOT register a second channel/schema.
        tx.try_send(RecordMsg::DynamicProto {
            topic: Arc::from("home/temp"),
            schema: Arc::from(b"schema_A".as_slice()),
            data: vec![0x0C],
            ts: 3_000,
        })
        .unwrap();
        drop(tx);
        drop(handle);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "mcap").unwrap_or(false))
            .collect();
        let bytes = std::fs::read(entries[0].path()).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(messages.len(), 3);
        // Two distinct topics → two distinct channels, each with its own schema.
        let temp = messages.iter().find(|m| m.channel.topic == "home/temp").unwrap();
        let door = messages.iter().find(|m| m.channel.topic == "home/door").unwrap();
        assert_eq!(temp.channel.schema.as_ref().unwrap().data.as_ref(), b"schema_A");
        assert_eq!(door.channel.schema.as_ref().unwrap().data.as_ref(), b"schema_B");
        // The repeated topic reused the first channel.
        let temp_count = messages.iter().filter(|m| m.channel.topic == "home/temp").count();
        assert_eq!(temp_count, 2);
    }

    #[test]
    fn dynamic_proto_topic_colliding_with_registry_gets_own_schema() {
        // Registry pre-seeds ZMQ topic "accel" with the shared (empty) schema.
        // A DynamicProto message on the same topic string must be recorded under
        // its OWN schema, not the pre-seeded ZMQ schema.
        let dir = tempfile::tempdir().unwrap();
        let registry = minimal_registry();
        let (tx, rx) = record_channel();
        let handle = start_recording(dir.path(), &registry, vec![], rx, None).unwrap();

        let mqtt_schema: &[u8] = b"mqtt_schema_for_accel";
        tx.try_send(RecordMsg::DynamicProto {
            topic: Arc::from("accel"),
            schema: Arc::from(mqtt_schema),
            data: vec![0xDE, 0xAD],
            ts: 1_000_000,
        })
        .unwrap();

        drop(tx);
        drop(handle);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "mcap").unwrap_or(false))
            .collect();
        assert_eq!(entries.len(), 1);

        let bytes = std::fs::read(entries[0].path()).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        // Exactly one message was written (the DynamicProto one; no ZMQ "accel" messages sent).
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].channel.topic, "accel");
        assert_eq!(messages[0].data.as_ref(), &[0xDE_u8, 0xAD]);
        // The channel must carry the MQTT schema, not the empty shared ZMQ schema.
        let schema_data = messages[0].channel.schema.as_ref().unwrap().data.as_ref();
        assert_eq!(schema_data, mqtt_schema, "DynamicProto message was recorded under the wrong schema");
    }

    #[test]
    fn rotates_into_numbered_parts_when_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        let registry = minimal_registry();
        let (tx, rx) = record_channel();
        // Tiny 8 KiB cap forces several rollovers over a few thousand messages.
        let handle = start_recording(dir.path(), &registry, vec![], rx, Some(8 * 1024)).unwrap();

        let n = 4000i64;
        for i in 0..n {
            // 64-byte payloads so total >> cap after compression.
            tx.try_send(RecordMsg::Proto { topic: Arc::from("accel"), data: vec![(i % 256) as u8; 64], ts: i + 1 })
                .unwrap();
        }
        drop(tx);
        drop(handle);
        std::thread::sleep(std::time::Duration::from_millis(400));

        // Collect parts, sorted by name → they are recording_{secs}_000.mcap, _001, …
        let mut parts: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "mcap").unwrap_or(false))
            .collect();
        parts.sort();
        assert!(parts.len() >= 2, "expected multiple parts, got {}", parts.len());
        // First part is suffix _000.
        assert!(parts[0].file_name().unwrap().to_str().unwrap().contains("_000.mcap"));
        // Every part is finalized (summary readable, non-empty chunk index).
        for p in &parts {
            let bytes = std::fs::read(p).unwrap();
            let summary = mcap::Summary::read(&bytes).unwrap().expect("finalized summary");
            assert!(!summary.chunk_indexes.is_empty(), "part {p:?} has no chunk index");
        }

        // Stitched round-trip at the MCAP layer: every written message survives,
        // across all parts, in timestamp order.
        let mut all_ts: Vec<u64> = Vec::new();
        for p in &parts {
            let bytes = std::fs::read(p).unwrap();
            for m in mcap::MessageStream::new(&bytes).unwrap() {
                all_ts.push(m.unwrap().log_time);
            }
        }
        assert_eq!(all_ts.len(), n as usize, "all messages must be preserved across parts");
        all_ts.sort_unstable();
        assert_eq!(all_ts.first(), Some(&1));
        assert_eq!(all_ts.last(), Some(&(n as u64)));
    }
}
