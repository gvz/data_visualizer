use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use prost_reflect::{Kind, MessageDescriptor};

use crate::config::ChannelRegistry;
use crate::ingest::decode::decode_batch;
use crate::ingest::loader::ProtoSchema;
use crate::ingest::router::{ChannelBinding, TopicRouter};
use crate::store::ChannelStore;
use crate::types::{
    ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, SampleType, TimeWindow,
};

/// The fully-qualified name of the single message defined by an embedded schema,
/// e.g. "mqtt.HomeSensorsTemperature". None if the set defines no message.
fn first_message_name(schema_bytes: &[u8]) -> Option<String> {
    use prost_reflect::prost::Message as _;
    let fds = prost_types::FileDescriptorSet::decode(schema_bytes).ok()?;
    for file in &fds.file {
        if let Some(msg) = file.message_type.first() {
            // prost-types 0.13 exposes these as plain Option<String> fields
            // (no name()/package() accessor methods).
            let name = msg.name.as_deref().unwrap_or_default();
            return match file.package.as_deref() {
                None | Some("") => Some(name.to_string()),
                Some(pkg) => Some(format!("{pkg}.{name}")),
            };
        }
    }
    None
}

/// Map the generated `value` field's protobuf kind to a SampleType.
fn value_sample_type(desc: &MessageDescriptor) -> Option<SampleType> {
    let field = desc.get_field_by_name("value")?;
    Some(match field.kind() {
        Kind::Double | Kind::Float => SampleType::Float,
        Kind::Int64 | Kind::Int32 | Kind::Uint64 | Kind::Uint32
        | Kind::Sint64 | Kind::Sint32 | Kind::Fixed64 | Kind::Fixed32
        | Kind::Sfixed64 | Kind::Sfixed32 => SampleType::Int,
        Kind::Bool => SampleType::Bool,
        Kind::String => SampleType::Text,
        _ => return None,
    })
}

enum PlaybackChannel {
    Float { ts: Vec<i64>, vals: Vec<f64> },
    Int { ts: Vec<i64>, vals: Vec<i64> },
    Bool { ts: Vec<i64>, vals: Vec<u8> },
    Text { lines: Vec<(i64, String)> },
}

impl PlaybackChannel {
    fn for_type(sample_type: SampleType) -> Self {
        match sample_type {
            SampleType::Float => PlaybackChannel::Float { ts: vec![], vals: vec![] },
            SampleType::Int => PlaybackChannel::Int { ts: vec![], vals: vec![] },
            SampleType::Bool => PlaybackChannel::Bool { ts: vec![], vals: vec![] },
            SampleType::Text => PlaybackChannel::Text { lines: vec![] },
        }
    }
}

pub struct PlaybackStore {
    channels: Vec<Mutex<PlaybackChannel>>,
    metas: Vec<ChannelMeta>,
    pub position_ns: Arc<AtomicI64>,
    pub duration_ns: i64,
    pub start_ns: i64,
}

impl PlaybackStore {
    fn new(registry: &ChannelRegistry) -> Self {
        let channels = registry
            .iter_ids()
            .map(|id| Mutex::new(PlaybackChannel::for_type(registry.meta(id).sample_type)))
            .collect();
        let metas = registry.iter_ids().map(|id| registry.meta(id).clone()).collect();
        Self {
            channels,
            metas,
            position_ns: Arc::new(AtomicI64::new(0)),
            duration_ns: 0,
            start_ns: 0,
        }
    }

    fn sort_and_finalize(&mut self) {
        let mut global_min = i64::MAX;
        let mut global_max = i64::MIN;

        for ch in &self.channels {
            let mut ch = ch.lock().unwrap();
            match &mut *ch {
                PlaybackChannel::Float { ts, vals } => {
                    let mut pairs: Vec<(i64, f64)> =
                        ts.iter().copied().zip(vals.iter().copied()).collect();
                    pairs.sort_unstable_by_key(|(t, _)| *t);
                    *ts = pairs.iter().map(|(t, _)| *t).collect();
                    *vals = pairs.iter().map(|(_, v)| *v).collect();
                    if let (Some(&mn), Some(&mx)) = (ts.first(), ts.last()) {
                        global_min = global_min.min(mn);
                        global_max = global_max.max(mx);
                    }
                }
                PlaybackChannel::Int { ts, vals } => {
                    let mut pairs: Vec<(i64, i64)> =
                        ts.iter().copied().zip(vals.iter().copied()).collect();
                    pairs.sort_unstable_by_key(|(t, _)| *t);
                    *ts = pairs.iter().map(|(t, _)| *t).collect();
                    *vals = pairs.iter().map(|(_, v)| *v).collect();
                    if let (Some(&mn), Some(&mx)) = (ts.first(), ts.last()) {
                        global_min = global_min.min(mn);
                        global_max = global_max.max(mx);
                    }
                }
                PlaybackChannel::Bool { ts, vals } => {
                    let mut pairs: Vec<(i64, u8)> =
                        ts.iter().copied().zip(vals.iter().copied()).collect();
                    pairs.sort_unstable_by_key(|(t, _)| *t);
                    *ts = pairs.iter().map(|(t, _)| *t).collect();
                    *vals = pairs.iter().map(|(_, v)| *v).collect();
                    if let (Some(&mn), Some(&mx)) = (ts.first(), ts.last()) {
                        global_min = global_min.min(mn);
                        global_max = global_max.max(mx);
                    }
                }
                PlaybackChannel::Text { lines } => {
                    lines.sort_unstable_by_key(|(t, _)| *t);
                    if let (Some((mn, _)), Some((mx, _))) = (lines.first(), lines.last()) {
                        global_min = global_min.min(*mn);
                        global_max = global_max.max(*mx);
                    }
                }
            }
        }

        if global_min <= global_max {
            self.start_ns = global_min;
            self.duration_ns = global_max - global_min;
            self.position_ns.store(global_min, Ordering::Relaxed);
        }
    }

    pub fn load(path: &Path, registry: &ChannelRegistry) -> anyhow::Result<Arc<Self>> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading MCAP file {}", path.display()))?;

        // Pass 1: collect one embedded schema per topic.
        let mut topic_schemas: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for message in mcap::MessageStream::new(&bytes)
            .context("opening MCAP message stream")?
        {
            let msg = message.context("reading MCAP message")?;
            if let Some(schema) = &msg.channel.schema {
                topic_schemas
                    .entry(msg.channel.topic.clone())
                    .or_insert_with(|| schema.data.to_vec());
            }
        }

        // Merge all embedded schemas into one pool; route known ZMQ topics.
        let set_refs: Vec<&[u8]> = topic_schemas.values().map(Vec::as_slice).collect();
        let merged = ProtoSchema::from_descriptor_sets(&set_refs);
        let router = TopicRouter::build(registry, &merged);

        // Reconstruct topics with no registry binding (MQTT / generated messages).
        let mut reconstructed: HashMap<String, Vec<ChannelBinding>> = HashMap::new();
        for (topic, schema_bytes) in &topic_schemas {
            if !router.bindings_for(topic).is_empty() {
                continue;
            }
            let Some(full) = first_message_name(schema_bytes) else { continue };
            let Some(desc) = merged.message_by_name(&full) else { continue };
            if desc.get_field_by_name("t_ns").is_none() {
                continue;
            }
            let Some(sample_type) = value_sample_type(&desc) else { continue };
            let id = registry.add_dynamic(topic, topic, sample_type);
            reconstructed.insert(
                topic.clone(),
                vec![ChannelBinding {
                    id,
                    msg_desc: desc,
                    val_path: vec!["value".to_string()],
                    ts_path: vec!["t_ns".to_string()],
                    eu_scale: 1.0,
                    eu_offset: 0.0,
                    sample_type,
                }],
            );
        }

        // Build the store AFTER add_dynamic so reconstructed channels are included.
        let mut store = Self::new(registry);

        // Pass 2: decode every message into the store.
        for message in mcap::MessageStream::new(&bytes)
            .context("opening MCAP message stream")?
        {
            let msg = message.context("reading MCAP message")?;
            let topic = msg.channel.topic.as_str();
            let zmq = router.bindings_for(topic);
            let bindings: &[ChannelBinding] = if zmq.is_empty() {
                reconstructed.get(topic).map(Vec::as_slice).unwrap_or(&[])
            } else {
                zmq
            };
            decode_batch(&msg.data, bindings, &store);
        }

        store.sort_and_finalize();
        Ok(Arc::new(store))
    }
}

impl ChannelStore for PlaybackStore {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal) {
        let mut ch = self.channels[channel.0 as usize].lock().unwrap();
        match (&mut *ch, val) {
            (PlaybackChannel::Float { ts: tvec, vals }, NumericVal::Float(v)) => {
                tvec.push(ts);
                vals.push(v);
            }
            (PlaybackChannel::Int { ts: tvec, vals }, NumericVal::Int(v)) => {
                tvec.push(ts);
                vals.push(v);
            }
            (PlaybackChannel::Bool { ts: tvec, vals }, NumericVal::Bool(v)) => {
                tvec.push(ts);
                vals.push(v as u8);
            }
            _ => {}
        }
    }

    fn write_text(&self, channel: ChannelId, ts: i64, line: String) {
        let mut ch = self.channels[channel.0 as usize].lock().unwrap();
        if let PlaybackChannel::Text { lines } = &mut *ch {
            lines.push((ts, line));
        }
    }

    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot {
        let ch = self.channels[channel.0 as usize].lock().unwrap();
        match &*ch {
            PlaybackChannel::Float { ts, vals } => {
                let start = ts.partition_point(|&t| t < window.start_ns);
                let end = ts.partition_point(|&t| t < window.end_ns);
                ChannelSnapshot::Float {
                    ts: ts[start..end].to_vec(),
                    vals: vals[start..end].to_vec(),
                }
            }
            PlaybackChannel::Int { ts, vals } => {
                let start = ts.partition_point(|&t| t < window.start_ns);
                let end = ts.partition_point(|&t| t < window.end_ns);
                ChannelSnapshot::Int {
                    ts: ts[start..end].to_vec(),
                    vals: vals[start..end].to_vec(),
                }
            }
            PlaybackChannel::Bool { ts, vals } => {
                let start = ts.partition_point(|&t| t < window.start_ns);
                let end = ts.partition_point(|&t| t < window.end_ns);
                ChannelSnapshot::Bool {
                    ts: ts[start..end].to_vec(),
                    vals: vals[start..end].to_vec(),
                }
            }
            PlaybackChannel::Text { lines } => {
                let start = lines.partition_point(|(t, _)| *t < window.start_ns);
                let end = lines.partition_point(|(t, _)| *t < window.end_ns);
                ChannelSnapshot::Text { lines: lines[start..end].to_vec() }
            }
        }
    }

    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)> {
        let pos = self.position_ns.load(Ordering::Relaxed);
        let ch = self.channels[channel.0 as usize].lock().unwrap();
        match &*ch {
            PlaybackChannel::Float { ts, vals } => {
                // Last index where ts <= pos
                let idx = ts.partition_point(|&t| t <= pos);
                if idx == 0 { return None; }
                Some((ts[idx - 1], Sample::Float(vals[idx - 1])))
            }
            PlaybackChannel::Int { ts, vals } => {
                let idx = ts.partition_point(|&t| t <= pos);
                if idx == 0 { return None; }
                Some((ts[idx - 1], Sample::Int(vals[idx - 1])))
            }
            PlaybackChannel::Bool { ts, vals } => {
                let idx = ts.partition_point(|&t| t <= pos);
                if idx == 0 { return None; }
                Some((ts[idx - 1], Sample::Bool(vals[idx - 1] != 0)))
            }
            PlaybackChannel::Text { lines } => {
                let idx = lines.partition_point(|(t, _)| *t <= pos);
                if idx == 0 { return None; }
                Some((lines[idx - 1].0, Sample::Text(lines[idx - 1].1.clone())))
            }
        }
    }

    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta {
        &self.metas[channel.0 as usize]
    }

    fn now_ns(&self) -> i64 {
        self.position_ns.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::loader::ProtoSchema;
    use crate::types::{ChannelSnapshot, TimeWindow};
    use std::io::Write;
    use std::sync::atomic::Ordering;

    fn make_proto_and_registry() -> (ProtoSchema, tempfile::TempDir, ChannelRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message AccelBatch {{
  repeated Sample samples = 1;
  message Sample {{ int64 t_ns = 1; float x = 2; }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
        (schema, dir, registry)
    }

    fn write_test_mcap(
        path: &std::path::Path,
        schema: &ProtoSchema,
        messages: &[(i64, f32)],  // (t_ns, x)
    ) {
        use prost_reflect::prost::Message as _;
        use prost_reflect::{DynamicMessage, Value};
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("AccelBatch").unwrap();
        let sample_desc = pool.get_message_by_name("AccelBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let x_field = sample_desc.get_field_by_name("x").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();

        let mcap_schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(schema.schema_bytes().to_vec()),
        });
        let channel = Arc::new(mcap::Channel {
            topic: "accel".to_string(),
            schema: Some(mcap_schema),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        for (t_ns, x) in messages {
            let list = vec![{
                let mut s = DynamicMessage::new(sample_desc.clone());
                s.set_field(&t_field, Value::I64(*t_ns));
                s.set_field(&x_field, Value::F32(*x));
                Value::Message(s)
            }];
            let mut batch = DynamicMessage::new(batch_desc.clone());
            batch.set_field(&samples_field, Value::List(list));
            let data = batch.encode_to_vec();
            writer.write(&mcap::Message {
                channel: channel.clone(),
                sequence: 0,
                log_time: *t_ns as u64,
                publish_time: *t_ns as u64,
                data: Cow::Owned(data),
            }).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn load_and_snapshot_returns_data_in_window() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (2_000_000_000, 2.0),
            (3_000_000_000, 3.0),
        ]);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("accel.x").unwrap();
        let window = TimeWindow { start_ns: 1_000_000_000, end_ns: 3_000_000_000 };
        let snap = store.snapshot(id, window);
        match snap {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts.len(), 2);
                assert_eq!(ts[0], 1_000_000_000);
                assert_eq!(ts[1], 2_000_000_000);
                // EU scale 1.0, offset 0.0 (defaults) → values unchanged
                assert!((vals[0] - 1.0_f64).abs() < 1e-4);
                assert!((vals[1] - 2.0_f64).abs() < 1e-4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn now_ns_returns_position_not_wall_clock() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[(1_000_000_000, 1.0)]);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let expected_pos = 42_999_999_999i64;
        store.position_ns.store(expected_pos, Ordering::Relaxed);
        assert_eq!(store.now_ns(), expected_pos);
    }

    #[test]
    fn latest_returns_sample_at_or_before_position() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (3_000_000_000, 3.0),
        ]);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("accel.x").unwrap();

        // Position before any sample → None
        store.position_ns.store(0, Ordering::Relaxed);
        assert!(store.latest(id).is_none());

        // Position at first sample → returns first
        store.position_ns.store(1_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest(id).unwrap();
        assert_eq!(ts, 1_000_000_000);

        // Position between samples → returns first
        store.position_ns.store(2_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest(id).unwrap();
        assert_eq!(ts, 1_000_000_000);

        // Position at second sample → returns second
        store.position_ns.store(3_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest(id).unwrap();
        assert_eq!(ts, 3_000_000_000);
    }

    #[test]
    fn duration_and_start_ns_computed_from_data() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (4_000_000_000, 4.0),
        ]);
        let store = PlaybackStore::load(&path, &registry).unwrap();
        assert_eq!(store.start_ns, 1_000_000_000);
        assert_eq!(store.duration_ns, 3_000_000_000);
    }

    /// Write a single-message MCAP for one MQTT topic using the recorder's own
    /// schema generator, so the embedded schema matches production exactly.
    fn write_mqtt_mcap(path: &std::path::Path, topic: &str, payloads: &[(i64, &str)]) {
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let mut reg = DynamicProtoRegistry::new();
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        let mut channel: Option<Arc<mcap::Channel>> = None;
        for (ts, payload) in payloads {
            let (schema_bytes, data) = reg.record_frame(topic, *ts, payload).unwrap();
            let ch = channel.get_or_insert_with(|| {
                Arc::new(mcap::Channel {
                    topic: topic.to_string(),
                    schema: Some(Arc::new(mcap::Schema {
                        name: topic.to_string(),
                        encoding: "protobuf".to_string(),
                        data: Cow::Owned(schema_bytes.to_vec()),
                    })),
                    message_encoding: "protobuf".to_string(),
                    metadata: BTreeMap::new(),
                })
            });
            writer.write(&mcap::Message {
                channel: ch.clone(),
                sequence: 0,
                log_time: *ts as u64,
                publish_time: *ts as u64,
                data: Cow::Owned(data),
            }).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn mqtt_topic_reconstructed_from_embedded_schema() {
        // Registry does NOT contain the MQTT topic.
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mqtt.mcap");
        write_mqtt_mcap(&path, "home/temp", &[(1_000_000_000, "21.5"), (2_000_000_000, "22.0")]);

        let store = PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("home/temp").expect("reconstructed channel registered");
        assert_eq!(registry.meta(id).sample_type, SampleType::Float);

        let window = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
        match store.snapshot(id, window) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                assert!((vals[0] - 21.5).abs() < 1e-4);
                assert!((vals[1] - 22.0).abs() < 1e-4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn mixed_zmq_and_mqtt_topics_both_decode() {
        use prost_reflect::prost::Message as _;
        use prost_reflect::{DynamicMessage, Value};
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let (schema, _dir, registry) = make_proto_and_registry(); // registers "accel.x" on topic "accel"
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("mixed.mcap");

        // ZMQ channel (embedded AccelBatch schema).
        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("AccelBatch").unwrap();
        let sample_desc = pool.get_message_by_name("AccelBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let x_field = sample_desc.get_field_by_name("x").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();
        let accel_channel = Arc::new(mcap::Channel {
            topic: "accel".to_string(),
            schema: Some(Arc::new(mcap::Schema {
                name: "protobuf".to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(schema.schema_bytes().to_vec()),
            })),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });

        // MQTT channel (generated schema).
        let mut reg = DynamicProtoRegistry::new();
        let (mqtt_schema_bytes, mqtt_data) = reg.record_frame("home/temp", 1_500_000_000, "30.0").unwrap();
        let mqtt_channel = Arc::new(mcap::Channel {
            topic: "home/temp".to_string(),
            schema: Some(Arc::new(mcap::Schema {
                name: "home/temp".to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(mqtt_schema_bytes.to_vec()),
            })),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });

        let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        // one accel batch sample
        let list = vec![{
            let mut s = DynamicMessage::new(sample_desc.clone());
            s.set_field(&t_field, Value::I64(1_000_000_000));
            s.set_field(&x_field, Value::F32(1.0));
            Value::Message(s)
        }];
        let mut batch = DynamicMessage::new(batch_desc.clone());
        batch.set_field(&samples_field, Value::List(list));
        writer.write(&mcap::Message {
            channel: accel_channel, sequence: 0,
            log_time: 1_000_000_000, publish_time: 1_000_000_000,
            data: Cow::Owned(batch.encode_to_vec()),
        }).unwrap();
        writer.write(&mcap::Message {
            channel: mqtt_channel, sequence: 0,
            log_time: 1_500_000_000, publish_time: 1_500_000_000,
            data: Cow::Owned(mqtt_data),
        }).unwrap();
        writer.finish().unwrap();

        let store = PlaybackStore::load(&path, &registry).unwrap();
        let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

        let accel_id = registry.id("accel.x").unwrap();
        match store.snapshot(accel_id, all) {
            ChannelSnapshot::Float { ts, .. } => assert_eq!(ts, vec![1_000_000_000]),
            other => panic!("accel wrong variant: {other:?}"),
        }
        let mqtt_id = registry.id("home/temp").unwrap();
        match store.snapshot(mqtt_id, all) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_500_000_000]);
                assert!((vals[0] - 30.0).abs() < 1e-4);
            }
            other => panic!("mqtt wrong variant: {other:?}"),
        }
    }

    #[test]
    fn value_sample_type_maps_kinds() {
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        let mut reg = DynamicProtoRegistry::new();
        // Each first payload locks a distinct type; grab the generated descriptor
        // back out via the merged pool and check value_sample_type.
        for (topic, payload, expected) in [
            ("t/f", "3.14", SampleType::Float),
            ("t/i", "7", SampleType::Int),
            ("t/b", "true", SampleType::Bool),
            ("t/s", "hello", SampleType::Text),
        ] {
            let (schema_bytes, _data) = reg.record_frame(topic, 1, payload).unwrap();
            let full = first_message_name(&schema_bytes).unwrap();
            let merged = ProtoSchema::from_descriptor_sets(&[&schema_bytes]);
            let desc = merged.message_by_name(&full).unwrap();
            assert_eq!(value_sample_type(&desc), Some(expected), "topic {topic}");
        }
    }
}
