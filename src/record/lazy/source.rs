use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;

use anyhow::Context;
use memmap2::Mmap;
use prost_reflect::{Kind, MessageDescriptor};

use crate::config::ChannelRegistry;
use crate::ingest::decode::decode_batch;
use crate::ingest::loader::ProtoSchema;
use crate::ingest::router::{ChannelBinding, TopicRouter};
use crate::store::ChannelStore;
use crate::types::{ChannelMeta, SampleType, TimeWindow};

use super::decode_buf::{ChunkDecodeBuf, DecodedChunk};

/// The fully-qualified name of the single message defined by an embedded schema,
/// e.g. "mqtt.HomeSensorsTemperature". None if the set defines no message.
pub(crate) fn first_message_name(schema_bytes: &[u8]) -> Option<String> {
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
pub(crate) fn value_sample_type(desc: &MessageDescriptor) -> Option<SampleType> {
    let field = desc.get_field_by_name("value")?;
    Some(match field.kind() {
        Kind::Double | Kind::Float => SampleType::Float,
        Kind::Int64 | Kind::Int32 | Kind::Uint64 | Kind::Uint32 | Kind::Sint64 | Kind::Sint32
        | Kind::Fixed64 | Kind::Fixed32 | Kind::Sfixed64 | Kind::Sfixed32 => SampleType::Int,
        Kind::Bool => SampleType::Bool,
        Kind::String => SampleType::Text,
        _ => return None,
    })
}

/// Decode one MCAP message into `store` using the file's routing. ZMQ topics
/// resolve through the registry router; the rest fall back to reconstructed
/// (MQTT/generated) bindings.
pub(crate) fn decode_message(
    msg: &mcap::Message,
    router: &TopicRouter,
    reconstructed: &HashMap<String, Vec<ChannelBinding>>,
    store: &dyn ChannelStore,
) {
    let topic = msg.channel.topic.as_str();
    let zmq = router.bindings_for(topic);
    let bindings: &[ChannelBinding] = if zmq.is_empty() {
        reconstructed.get(topic).map(Vec::as_slice).unwrap_or(&[])
    } else {
        zmq
    };
    decode_batch(&msg.data, bindings, store);
}

/// Learn one embedded schema per topic and build the routing bindings for a
/// single file, registering any reconstructed (MQTT/generated) channels on the
/// shared registry. Prefers the MCAP summary section — channel/schema records
/// are indexed in the footer, so no message chunk is decompressed. Files with
/// no summary (e.g. a crash mid-recording) fall back to a scan.
pub(crate) fn plan_file(
    bytes: &[u8],
    registry: &ChannelRegistry,
) -> anyhow::Result<(TopicRouter, HashMap<String, Vec<ChannelBinding>>)> {
    let mut topic_schemas: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let summary = mcap::Summary::read(bytes).context("reading MCAP summary")?;
    match summary.filter(|s| !s.channels.is_empty()) {
        Some(summary) => {
            for channel in summary.channels.values() {
                if let Some(schema) = &channel.schema {
                    topic_schemas
                        .entry(channel.topic.clone())
                        .or_insert_with(|| schema.data.to_vec());
                }
            }
        }
        None => {
            for message in
                mcap::MessageStream::new(bytes).context("opening MCAP message stream")?
            {
                let msg = message.context("reading MCAP message")?;
                if let Some(schema) = &msg.channel.schema {
                    topic_schemas
                        .entry(msg.channel.topic.clone())
                        .or_insert_with(|| schema.data.to_vec());
                }
            }
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
        if registry.meta(id).sample_type != sample_type {
            eprintln!(
                "replay: topic {:?} already registered as {:?}, but this file records it as {:?}; skipping",
                topic,
                registry.meta(id).sample_type,
                sample_type
            );
            continue;
        }
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

    Ok((router, reconstructed))
}

/// The (min, max) message time in a file. Uses the summary statistics when
/// present (no decompression); otherwise scans message log times. `log_time`
/// equals the sample `t_ns` the recorder writes, so this matches the decoded
/// sample timeline used for stitching. Returns None for an empty file.
pub(crate) fn time_bounds(bytes: &[u8]) -> anyhow::Result<Option<(i64, i64)>> {
    if let Some(summary) = mcap::Summary::read(bytes).context("reading MCAP summary")? {
        if let Some(stats) = summary.stats {
            if stats.message_count > 0 {
                return Ok(Some((
                    stats.message_start_time as i64,
                    stats.message_end_time as i64,
                )));
            }
        }
    }
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    for message in mcap::MessageStream::new(bytes).context("opening MCAP message stream")? {
        let t = message.context("reading MCAP message")?.log_time as i64;
        min = min.min(t);
        max = max.max(t);
    }
    Ok((min <= max).then_some((min, max)))
}

/// One chunk's time span. `idx` indexes `summary.chunk_indexes`; the sentinel
/// `usize::MAX` marks the whole-file span used when a file has no chunk index.
pub struct ChunkSpan {
    pub start_ns: i64,
    pub end_ns: i64,
    pub idx: usize,
}

/// One memory-mapped MCAP recording: the compressed bytes are paged in by the
/// OS on demand, the chunk index is read from the summary, and individual
/// chunks are decoded only when a read needs them.
pub struct RecordingSource {
    mmap: Mmap,
    router: TopicRouter,
    reconstructed: HashMap<String, Vec<ChannelBinding>>,
    spans: Vec<ChunkSpan>,
    pub bounds: Option<(i64, i64)>,
}

impl RecordingSource {
    pub fn open(path: &Path, registry: &ChannelRegistry) -> anyhow::Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        // Safety: the file is opened read-only and not mutated for the mmap's
        // lifetime; MCAP playback is read-only.
        let mmap =
            unsafe { Mmap::map(&file) }.with_context(|| format!("mmapping {}", path.display()))?;
        let bytes: &[u8] = &mmap;

        let (router, reconstructed) = plan_file(bytes, registry)?;

        let mut spans = Vec::new();
        if let Some(summary) = mcap::Summary::read(bytes).context("reading MCAP summary")? {
            for (i, ci) in summary.chunk_indexes.iter().enumerate() {
                spans.push(ChunkSpan {
                    start_ns: ci.message_start_time as i64,
                    end_ns: ci.message_end_time as i64,
                    idx: i,
                });
            }
        }
        spans.sort_by_key(|s| s.start_ns);

        let bounds =
            time_bounds(bytes).with_context(|| format!("reading time bounds of {}", path.display()))?;

        // Fallback: no chunk index (unsummarised file) → one whole-file span so
        // decode_chunk(0) linear-scans the file.
        if spans.is_empty() {
            if let Some((mn, mx)) = bounds {
                spans.push(ChunkSpan { start_ns: mn, end_ns: mx, idx: usize::MAX });
            }
        }

        Ok(Self { mmap, router, reconstructed, spans, bounds })
    }

    pub fn spans(&self) -> &[ChunkSpan] {
        &self.spans
    }

    pub fn overlapping(&self, window: TimeWindow) -> Vec<usize> {
        // A span intersects [start, end) iff span.start < window.end && span.end >= window.start.
        self.spans
            .iter()
            .enumerate()
            .filter(|(_, s)| s.start_ns < window.end_ns && s.end_ns >= window.start_ns)
            .map(|(i, _)| i)
            .collect()
    }

    pub(crate) fn decode_into(&self, span: &ChunkSpan, sink: &dyn ChannelStore) -> anyhow::Result<()> {
        let bytes: &[u8] = &self.mmap;
        if span.idx == usize::MAX {
            // Whole-file linear scan (no summary).
            for message in
                mcap::MessageStream::new(bytes).context("opening MCAP message stream")?
            {
                let msg = message.context("reading MCAP message")?;
                decode_message(&msg, &self.router, &self.reconstructed, sink);
            }
            return Ok(());
        }
        let summary = mcap::Summary::read(bytes)
            .context("reading MCAP summary")?
            .context("MCAP summary missing")?;
        let index = &summary.chunk_indexes[span.idx];
        for message in summary.stream_chunk(bytes, index).context("streaming MCAP chunk")? {
            let msg = message.context("reading MCAP message")?;
            decode_message(&msg, &self.router, &self.reconstructed, sink);
        }
        Ok(())
    }

    /// Decode exactly one chunk (or the whole file, for the no-summary fallback)
    /// into a fresh `DecodedChunk`. Decode errors are logged per-message by
    /// `decode_batch` and skipped.
    pub fn decode_chunk(&self, span_idx: usize, metas: &[ChannelMeta]) -> DecodedChunk {
        let buf = ChunkDecodeBuf::from_metas(metas);
        if let Some(span) = self.spans.get(span_idx) {
            let _ = self.decode_into(span, &buf);
        }
        buf.freeze()
    }

    /// Stream every span's messages into `sink`.
    pub fn decode_all(&self, sink: &dyn ChannelStore) -> anyhow::Result<()> {
        for span in &self.spans {
            self.decode_into(span, sink)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::loader::ProtoSchema;
    use crate::types::{ChannelSnapshot, TimeWindow};
    use std::io::Write;

    fn make_proto_and_registry() -> (ProtoSchema, tempfile::TempDir, ChannelRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            r#"
syntax = "proto3";
message AccelBatch {{
  repeated Sample samples = 1;
  message Sample {{ int64 t_ns = 1; float x = 2; }}
}}
"#
        )
        .unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(
            r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#,
        )
        .unwrap();
        (schema, dir, registry)
    }

    fn write_test_mcap(path: &std::path::Path, schema: &ProtoSchema, messages: &[(i64, f32)]) {
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
            writer
                .write(&mcap::Message {
                    channel: channel.clone(),
                    sequence: 0,
                    log_time: *t_ns as u64,
                    publish_time: *t_ns as u64,
                    data: Cow::Owned(data),
                })
                .unwrap();
        }
        writer.finish().unwrap();
    }

    fn metas_of(registry: &ChannelRegistry) -> Vec<ChannelMeta> {
        registry.iter_ids().map(|id| registry.meta(id).clone()).collect()
    }

    #[test]
    fn open_indexes_chunks_and_decodes_one() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.mcap");
        write_test_mcap(
            &path,
            &schema,
            &[(1_000_000_000, 1.0), (2_000_000_000, 2.0), (3_000_000_000, 3.0)],
        );
        let src = RecordingSource::open(&path, &registry).unwrap();
        assert_eq!(src.bounds, Some((1_000_000_000, 3_000_000_000)));
        assert!(!src.spans().is_empty());
        let id = registry.id("accel.x").unwrap();
        let chunk = src.decode_chunk(0, &metas_of(&registry));
        let snap = chunk.window(id.0 as usize, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
        match snap {
            ChannelSnapshot::Float { ts, .. } => assert!(!ts.is_empty()),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn overlapping_selects_intersecting_spans() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.mcap");
        write_test_mcap(&path, &schema, &[(1_000_000_000, 1.0), (5_000_000_000, 2.0)]);
        let src = RecordingSource::open(&path, &registry).unwrap();
        // Window before all data → no spans.
        assert!(src.overlapping(TimeWindow { start_ns: 0, end_ns: 500_000_000 }).is_empty());
        // Window covering the data → at least one span.
        assert!(!src.overlapping(TimeWindow { start_ns: 0, end_ns: i64::MAX }).is_empty());
    }
}
