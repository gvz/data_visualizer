use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;

use anyhow::Context;
use memmap2::Mmap;
use prost_reflect::{Cardinality, Kind, MessageDescriptor};

use crate::config::ChannelRegistry;
use crate::ingest::decode::decode_batch_at;
use crate::ingest::loader::ProtoSchema;
use crate::ingest::router::{ChannelBinding, TopicRouter};
use crate::store::ChannelStore;
use crate::types::{ChannelMeta, SampleType, TimeWindow};

use super::decode_buf::{ChunkDecodeBuf, DecodedChunk};

/// Batch MCAP messages carry one `log_time` (the batch's last sample), so the
/// first batch's chunk-index start bound can sit up to one batch span AFTER the
/// chunk's true earliest sample. Pad the start check by that span so no
/// in-window chunk is skipped; per-sample clipping inside the chunk still
/// returns exact results. Kept at the max batch span (500 ms) so it does not
/// over-select on files with tightly spaced chunks.
pub(crate) const SPAN_PAD_NS: i64 = 500_000_000;

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

/// Sample type for a scalar protobuf kind, or None for non-scalar kinds
/// (enum, bytes, group, message).
fn scalar_sample_type(kind: &Kind) -> Option<SampleType> {
    Some(match kind {
        Kind::Double | Kind::Float => SampleType::Float,
        Kind::Int64 | Kind::Int32 | Kind::Uint64 | Kind::Uint32 | Kind::Sint64 | Kind::Sint32
        | Kind::Fixed64 | Kind::Fixed32 | Kind::Sfixed64 | Kind::Sfixed32 => SampleType::Int,
        Kind::Bool => SampleType::Bool,
        Kind::String => SampleType::Text,
        _ => return None, // Enum, Bytes, Message, Group
    })
}

/// Every scalar leaf field of `desc` as (field-name path, sample type). Recurses
/// into singular sub-messages; skips repeated fields, enums, bytes, and groups.
/// Depth-capped to guard against recursive schemas.
pub(crate) fn enumerate_leaves(desc: &MessageDescriptor) -> Vec<(Vec<String>, SampleType)> {
    let mut out = Vec::new();
    let mut prefix = Vec::new();
    walk_leaves(desc, &mut prefix, 0, &mut out);
    out
}

fn walk_leaves(
    desc: &MessageDescriptor,
    prefix: &mut Vec<String>,
    depth: usize,
    out: &mut Vec<(Vec<String>, SampleType)>,
) {
    if depth > 8 {
        return;
    }
    for f in desc.fields() {
        if f.cardinality() == Cardinality::Repeated {
            continue;
        }
        match f.kind() {
            Kind::Message(sub) => {
                prefix.push(f.name().to_string());
                walk_leaves(&sub, prefix, depth + 1, out);
                prefix.pop();
            }
            k => {
                if let Some(st) = scalar_sample_type(&k) {
                    let mut path = prefix.clone();
                    path.push(f.name().to_string());
                    out.push((path, st));
                }
            }
        }
    }
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
    decode_batch_at(&msg.data, bindings, store, msg.log_time as i64);
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

        // Determine shape: flat (top-level t_ns+value) or batch (repeated Sample).
        let (leaf_desc, val_path, ts_path) = if desc.get_field_by_name("t_ns").is_some()
            && desc.get_field_by_name("value").is_some()
        {
            (desc.clone(), vec!["value".to_string()], vec!["t_ns".to_string()])
        } else if let Some((rep_name, sub)) = desc.fields().find_map(|f| {
            if f.cardinality() == Cardinality::Repeated {
                if let Kind::Message(m) = f.kind() {
                    if m.get_field_by_name("t_ns").is_some() && m.get_field_by_name("value").is_some() {
                        return Some((f.name().to_string(), m));
                    }
                }
            }
            None
        }) {
            (sub, vec![rep_name.clone(), "value".to_string()], vec![rep_name, "t_ns".to_string()])
        } else {
            // Foreign structured message: not datavis's {t_ns, value} shape.
            // Flatten every scalar leaf into its own channel, timestamped by the
            // MCAP record log_time (these messages carry no embedded ts).
            let mut bindings = Vec::new();
            for (path, sample_type) in enumerate_leaves(&desc) {
                let name = format!("{topic}/{}", path.join("/"));
                let id = registry.add_dynamic(&name, topic, sample_type);
                if registry.meta(id).sample_type != sample_type {
                    eprintln!(
                        "replay: channel {name:?} already registered as {:?}, but this file records it as {:?}; skipping",
                        registry.meta(id).sample_type, sample_type
                    );
                    continue;
                }
                let fast = crate::ingest::decode::FastPlan::build(
                    &desc, &path, &[], sample_type, 1.0, 0.0, id,
                );
                bindings.push(ChannelBinding {
                    id,
                    fast,
                    msg_desc: desc.clone(),
                    val_path: path,
                    ts_path: Vec::new(),
                    eu_scale: 1.0,
                    eu_offset: 0.0,
                    sample_type,
                });
            }
            if !bindings.is_empty() {
                reconstructed.insert(topic.clone(), bindings);
            }
            continue;
        };

        let Some(sample_type) = value_sample_type(&leaf_desc) else { continue };
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
                fast: crate::ingest::decode::FastPlan::build(
                    &desc, &val_path, &ts_path, sample_type, 1.0, 0.0, id,
                ),
                msg_desc: desc,
                val_path,
                ts_path,
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
    /// Parsed once at open and reused. `Summary::read` seeks to the footer and
    /// parses every chunk index, channel, and schema; re-reading it per chunk
    /// decode — once per scroll frame on a miss, once per chunk per worker at
    /// load — is a heavy repeated cost on large files. `None` only for an
    /// unsummarised file, where decoding uses the whole-file span instead.
    ///
    /// `Summary` borrows the mapped bytes; its lifetime is erased to `'static`
    /// (see `open`). This is sound because it borrows only from `mmap`, whose
    /// pages are stable for the struct's life, and this field is declared
    /// before `mmap` so it is dropped first. The borrow is never exposed
    /// beyond the struct.
    summary: Option<mcap::Summary<'static>>,
    mmap: Mmap,
    router: TopicRouter,
    reconstructed: HashMap<String, Vec<ChannelBinding>>,
    spans: Vec<ChunkSpan>,
    pub bounds: Option<(i64, i64)>,
    /// channel id → topic key (index into `topics_by_key`). Lets the detail path
    /// decode only the one topic that owns a requested channel instead of every
    /// topic in the chunk — decisive for foreign recordings that flatten into
    /// thousands of channels across hundreds of topics.
    chan_topic_key: HashMap<u32, u32>,
    topics_by_key: Vec<String>,
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

        let summary = mcap::Summary::read(bytes).context("reading MCAP summary")?;
        // SAFETY: `summary` borrows only from `mmap`'s mapped pages, which are
        // stable for this `RecordingSource`'s lifetime and (by field order)
        // dropped after `summary`. Erasing the borrow to 'static lets the two
        // live in one struct; no reference outliving the struct is ever handed
        // out. Moving `mmap` into `Self` below moves only the handle, not the
        // pages, so the borrow stays valid.
        let summary: Option<mcap::Summary<'static>> = unsafe { std::mem::transmute(summary) };
        let mut spans = Vec::new();
        if let Some(summary) = &summary {
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

        // Reverse index: channel id → topic, so the detail path can decode a
        // single topic. Covers both ZMQ (router) and reconstructed topics; every
        // bound channel belongs to exactly one topic.
        let mut topics_by_key: Vec<String> = Vec::new();
        let mut chan_topic_key: HashMap<u32, u32> = HashMap::new();
        let mut index_topic = |topic: &str, bindings: &[ChannelBinding], topics: &mut Vec<String>| {
            if bindings.is_empty() {
                return;
            }
            let key = topics.len() as u32;
            topics.push(topic.to_string());
            for b in bindings {
                chan_topic_key.insert(b.id.0, key);
            }
        };
        // Collect ZMQ topics first (borrow of router ends before we move it).
        let zmq_topics: Vec<String> = router.topics().map(str::to_string).collect();
        for topic in &zmq_topics {
            index_topic(topic, router.bindings_for(topic), &mut topics_by_key);
        }
        for (topic, bindings) in &reconstructed {
            index_topic(topic, bindings, &mut topics_by_key);
        }

        Ok(Self {
            mmap,
            summary,
            router,
            reconstructed,
            spans,
            bounds,
            chan_topic_key,
            topics_by_key,
        })
    }

    pub fn spans(&self) -> &[ChunkSpan] {
        &self.spans
    }

    /// The topic key owning channel `ch` in this source, or `None` if this
    /// source records nothing for that channel (e.g. it lives in another
    /// stitched file). The key indexes this source's cache entries.
    pub(crate) fn topic_key_of(&self, ch: usize) -> Option<u32> {
        self.chan_topic_key.get(&(ch as u32)).copied()
    }

    /// Decode only the messages of one topic in a span, into a fresh chunk. The
    /// detail path uses this so serving one channel doesn't reflect-decode every
    /// other topic in the chunk. `topic_key` comes from [`topic_key_of`].
    pub(crate) fn decode_chunk_topic(
        &self,
        span_idx: usize,
        topic_key: u32,
        metas: &[ChannelMeta],
    ) -> DecodedChunk {
        let buf = ChunkDecodeBuf::from_metas(metas);
        if let (Some(span), Some(topic)) =
            (self.spans.get(span_idx), self.topics_by_key.get(topic_key as usize))
        {
            let bindings = self
                .reconstructed
                .get(topic)
                .map(Vec::as_slice)
                .unwrap_or_else(|| self.router.bindings_for(topic));
            let _ = self.decode_topic_into(span, topic, bindings, &buf);
        }
        buf.freeze()
    }

    /// Stream a span (or the whole file, for the no-summary fallback), decoding
    /// only messages on `topic` through `bindings`.
    fn decode_topic_into(
        &self,
        span: &ChunkSpan,
        topic: &str,
        bindings: &[ChannelBinding],
        sink: &dyn ChannelStore,
    ) -> anyhow::Result<()> {
        let bytes: &[u8] = &self.mmap;
        let decode = |msg: &mcap::Message| {
            if msg.channel.topic == topic {
                decode_batch_at(&msg.data, bindings, sink, msg.log_time as i64);
            }
        };
        if span.idx == usize::MAX {
            for message in
                mcap::MessageStream::new(bytes).context("opening MCAP message stream")?
            {
                decode(&message.context("reading MCAP message")?);
            }
            return Ok(());
        }
        let summary = self.summary.as_ref().context("MCAP summary missing")?;
        let index = &summary.chunk_indexes[span.idx];
        for message in summary.stream_chunk(bytes, index).context("streaming MCAP chunk")? {
            decode(&message.context("reading MCAP message")?);
        }
        Ok(())
    }

    pub fn overlapping(&self, window: TimeWindow) -> Vec<usize> {
        // The index start can trail the true earliest sample by ≤ SPAN_PAD_NS, so a
        // span may contain in-window samples iff (start - pad) < window.end && end >= window.start.
        self.spans
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.start_ns.saturating_sub(SPAN_PAD_NS) < window.end_ns && s.end_ns >= window.start_ns
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Count of spans that *truly* intersect the window, using the exact index
    /// bounds with no start pad. This drives the overview-vs-detail budget
    /// decision: the [`SPAN_PAD_NS`] slop in [`overlapping`] must only widen the
    /// detail decode set for batched files, never inflate the overlap count and
    /// push a finely-chunked unbatched file onto the decimated overview path.
    pub fn overlapping_true_count(&self, window: TimeWindow) -> usize {
        self.spans
            .iter()
            .filter(|s| s.start_ns < window.end_ns && s.end_ns >= window.start_ns)
            .count()
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
        let summary = self.summary.as_ref().context("MCAP summary missing")?;
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

    #[test]
    fn enumerate_leaves_collects_scalars_recurses_singular_skips_repeated() {
        use crate::ingest::loader::ProtoSchema;
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message Health {{
  uint64 scheduler_got_sync = 1;
  bool ok = 2;
  Inner cmd = 3;
  repeated uint64 samples = 4;
  message Inner {{
    string label = 1;
    int64 code = 2;
  }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let desc = schema.message_by_name("Health").unwrap();

        let mut leaves = super::enumerate_leaves(&desc);
        leaves.sort_by_key(|x| x.0.clone());
        let expected = vec![
            (vec!["cmd".to_string(), "code".to_string()], SampleType::Int),
            (vec!["cmd".to_string(), "label".to_string()], SampleType::Text),
            (vec!["ok".to_string()], SampleType::Bool),
            (vec!["scheduler_got_sync".to_string()], SampleType::Int),
        ];
        let mut expected_sorted = expected.clone();
        expected_sorted.sort_by_key(|x| x.0.clone());
        assert_eq!(leaves, expected_sorted, "repeated `samples` must be skipped; singular `cmd` recursed");
    }

    #[test]
    fn enumerate_leaves_message_without_scalars_is_empty() {
        use crate::ingest::loader::ProtoSchema;
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message Wrap {{
  repeated Wrap children = 1;
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let desc = schema.message_by_name("Wrap").unwrap();
        assert!(super::enumerate_leaves(&desc).is_empty());
    }

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

    /// Write a batched MCAP: many Samples per topic in one repeated-field message.
    fn write_batched_mqtt_mcap(path: &std::path::Path, topic: &str, samples: &[(i64, &str)]) {
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let mut reg = DynamicProtoRegistry::new();
        let mut schema_bytes: Option<Arc<[u8]>> = None;
        let mut batch: Vec<u8> = Vec::new();
        let mut last_ts = 0i64;
        for (ts, payload) in samples {
            let (schema, sample_bytes) = reg.record_frame(topic, *ts, payload).unwrap();
            schema_bytes.get_or_insert(schema);
            // repeated Sample samples = 1  →  tag 0x0A, len, bytes
            batch.push(0x0A);
            let mut len = sample_bytes.len();
            loop {
                let mut b = (len & 0x7f) as u8;
                len >>= 7;
                if len != 0 { b |= 0x80; }
                batch.push(b);
                if len == 0 { break; }
            }
            batch.extend_from_slice(&sample_bytes);
            last_ts = *ts;
        }
        let channel = Arc::new(mcap::Channel {
            topic: topic.to_string(),
            schema: Some(Arc::new(mcap::Schema {
                name: topic.to_string(),
                encoding: "protobuf".to_string(),
                data: Cow::Owned(schema_bytes.unwrap().to_vec()),
            })),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        writer.write(&mcap::Message {
            channel, sequence: 0, log_time: last_ts as u64, publish_time: last_ts as u64,
            data: Cow::Owned(batch),
        }).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn batched_topic_reconstructs_and_decodes_all_samples() {
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batched.mcap");
        write_batched_mqtt_mcap(&path, "home/temp", &[
            (1_000_000_000, "21.5"),
            (2_000_000_000, "22.0"),
            (3_000_000_000, "23.5"),
        ]);
        let src = RecordingSource::open(&path, &registry).unwrap();
        let id = registry.id("home/temp").expect("batched topic reconstructed");
        let chunk = src.decode_chunk(0, &metas_of(&registry));
        match chunk.window(id.0 as usize, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX }) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000, 3_000_000_000]);
                assert!((vals[0] - 21.5).abs() < 1e-4 && (vals[2] - 23.5).abs() < 1e-4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn narrow_window_at_batch_leading_edge_returns_samples() {
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge.mcap");
        // One batch; log_time = last sample (300). Chunk index start_ns becomes 300,
        // but samples exist at 100/200/300.
        write_batched_mqtt_mcap(&path, "home/temp", &[
            (100, "1.0"), (200, "2.0"), (300, "3.0"),
        ]);
        let store = crate::record::playback::PlaybackStore::load(&path, &registry).unwrap();
        let id = registry.id("home/temp").unwrap();
        // Window [100,150) covers only the first sample, which sits before the
        // coarsened chunk start — without the pad this returns empty.
        let w = TimeWindow { start_ns: 100, end_ns: 150 };
        match crate::store::ChannelStore::snapshot(store.as_ref(), id, w) {
            ChannelSnapshot::Float { ts, .. } => assert_eq!(ts, vec![100]),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn write_foreign_mcap(path: &std::path::Path, msgs: &[(i64, i64, i64)]) {
        use crate::ingest::loader::ProtoSchema;
        use prost_reflect::prost::Message as _;
        use prost_reflect::{DynamicMessage, Value};
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::io::Write;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let ppath = dir.path().join("foreign.proto");
        let mut f = std::fs::File::create(&ppath).unwrap();
        write!(f, r#"
syntax = "proto3";
message Health {{
  int64 code = 1;
  Inner cmd = 2;
  repeated uint64 samples = 3;
  message Inner {{
    int64 depth_val = 1;
  }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&ppath).unwrap();
        let pool = schema.pool_for_test();
        let health = pool.get_message_by_name("Health").unwrap();
        let inner = pool.get_message_by_name("Health.Inner").unwrap();

        let mcap_schema = Arc::new(mcap::Schema {
            name: "Health".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(schema.schema_bytes().to_vec()),
        });
        let channel = Arc::new(mcap::Channel {
            topic: "S1/HEALTH".to_string(),
            schema: Some(mcap_schema),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        for (log_time, code, depth_val) in msgs {
            let mut im = DynamicMessage::new(inner.clone());
            im.set_field(&inner.get_field_by_name("depth_val").unwrap(), Value::I64(*depth_val));
            let mut hm = DynamicMessage::new(health.clone());
            hm.set_field(&health.get_field_by_name("code").unwrap(), Value::I64(*code));
            hm.set_field(&health.get_field_by_name("cmd").unwrap(), Value::Message(im));
            let data = hm.encode_to_vec();
            writer.write(&mcap::Message {
                channel: channel.clone(),
                sequence: 0,
                log_time: *log_time as u64,
                publish_time: *log_time as u64,
                data: Cow::Owned(data),
            }).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn foreign_topic_flattens_into_leaf_channels_stamped_by_log_time() {
        let registry = ChannelRegistry::from_toml_str("").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign.mcap");
        write_foreign_mcap(&path, &[(1_000_000_000, 7, 11), (2_000_000_000, 8, 22)]);

        let src = RecordingSource::open(&path, &registry).unwrap();

        // Channels registered: two scalar leaves; the repeated `samples` skipped.
        let code_id = registry.id("S1/HEALTH/code").expect("code leaf channel");
        let depth_id = registry.id("S1/HEALTH/cmd/depth_val").expect("nested leaf channel");
        assert!(registry.id("S1/HEALTH/samples").is_none(), "repeated field must not become a channel");

        // Decode and check samples stamped by log_time.
        let metas = metas_of(&registry);
        let chunk = src.decode_chunk(0, &metas);
        let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
        match chunk.window(code_id.0 as usize, all) {
            ChannelSnapshot::Int { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                assert_eq!(vals, vec![7, 8]);
            }
            other => panic!("wrong variant for code: {other:?}"),
        }
        match chunk.window(depth_id.0 as usize, all) {
            ChannelSnapshot::Int { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                assert_eq!(vals, vec![11, 22]);
            }
            other => panic!("wrong variant for depth_val: {other:?}"),
        }
    }

    #[test]
    fn decode_chunk_topic_decodes_only_the_requested_topic() {
        let registry = ChannelRegistry::from_toml_str("").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign.mcap");
        write_foreign_mcap(&path, &[(1_000_000_000, 7, 11), (2_000_000_000, 8, 22)]);

        let src = RecordingSource::open(&path, &registry).unwrap();
        let code_id = registry.id("S1/HEALTH/code").unwrap();
        let depth_id = registry.id("S1/HEALTH/cmd/depth_val").unwrap();

        // Both leaves belong to the same topic → same topic key.
        let key = src.topic_key_of(code_id.0 as usize).expect("code has a topic");
        assert_eq!(Some(key), src.topic_key_of(depth_id.0 as usize));

        // Topic-scoped decode fills the topic's leaves with the same data the
        // whole-chunk path produces.
        let metas = metas_of(&registry);
        let chunk = src.decode_chunk_topic(0, key, &metas);
        let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
        match chunk.window(code_id.0 as usize, all) {
            ChannelSnapshot::Int { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                assert_eq!(vals, vec![7, 8]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        match chunk.window(depth_id.0 as usize, all) {
            ChannelSnapshot::Int { vals, .. } => assert_eq!(vals, vec![11, 22]),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn foreign_topic_detail_snapshot_matches_via_playback() {
        // Exercise the real per-topic detail path through PlaybackStore::snapshot.
        let registry = ChannelRegistry::from_toml_str("").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign.mcap");
        write_foreign_mcap(&path, &[(1_000_000_000, 7, 11), (2_000_000_000, 8, 22)]);
        let store = crate::record::playback::PlaybackStore::load(&path, &registry).unwrap();
        let code_id = registry.id("S1/HEALTH/code").unwrap();
        let w = TimeWindow { start_ns: 1_000_000_000, end_ns: 2_500_000_000 };
        match crate::store::ChannelStore::snapshot(store.as_ref(), code_id, w) {
            ChannelSnapshot::Int { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                assert_eq!(vals, vec![7, 8]);
            }
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
