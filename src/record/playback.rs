use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;

use crate::config::ChannelRegistry;
use crate::ingest::decode::decode_batch;
use crate::ingest::loader::ProtoSchema;
use crate::ingest::router::TopicRouter;
use crate::store::ChannelStore;
use crate::types::{
    ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, SampleType, TimeWindow,
};

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

    pub fn load(
        path: &Path,
        registry: &ChannelRegistry,
        schema: &ProtoSchema,
    ) -> anyhow::Result<Arc<Self>> {
        let router = TopicRouter::build(registry, schema);
        let mut store = Self::new(registry);

        let bytes = std::fs::read(path)
            .with_context(|| format!("reading MCAP file {}", path.display()))?;
        for message in mcap::MessageStream::new(&bytes)
            .context("opening MCAP message stream")?
        {
            let msg = message.context("reading MCAP message")?;
            let bindings = router.bindings_for(&msg.channel.topic);
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
            data: Cow::Borrowed(&[]),
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
        let store = PlaybackStore::load(&path, &registry, &schema).unwrap();
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
        let store = PlaybackStore::load(&path, &registry, &schema).unwrap();
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
        let store = PlaybackStore::load(&path, &registry, &schema).unwrap();
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
        let store = PlaybackStore::load(&path, &registry, &schema).unwrap();
        assert_eq!(store.start_ns, 1_000_000_000);
        assert_eq!(store.duration_ns, 3_000_000_000);
    }
}
