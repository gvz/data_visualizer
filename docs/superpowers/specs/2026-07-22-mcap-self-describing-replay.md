# Self-Describing MCAP Replay (schemas read from the file, not live ingest)

**Status:** Design approved, ready for planning
**Date:** 2026-07-22
**Builds on:** `docs/superpowers/specs/2026-07-22-mqtt-onthefly-proto-schema-recording.md` (per-topic generated schemas, `DynamicProtoRegistry`). Work continues on branch `mqtt-proto-schema-recording`.

## Goal

Make opening an MCAP recording work standalone, decoding all recorded channels
from the schemas embedded in the file itself — with no dependency on a live
ingest schema. This fixes two problems:

1. `open_recording` refuses to load whenever `self.ingest_schema_bytes` is empty
   (demo run, MQTT-only run, or failed ingest) with *"Replay not available in
   demo mode (no proto schema)."*
2. MQTT recordings can never replay: their per-topic generated schemas live in
   the file and their topics are absent from `channels.toml`, so the existing
   router produces no bindings for them.

## Locked Decisions

- Replay reconstructs the full channel set **from the MCAP file**. Known ZMQ
  topics use `channels.toml` field paths; unknown/MQTT topics are reconstructed
  as new channels using the generated-message convention (`value` / `t_ns`).
- Reconstructed channels appear in the replay channel picker (droppable onto
  panels).
- Schemas are read from each `mcap::Channel`'s embedded `Schema`; the live
  `ingest_schema_bytes` is no longer consulted for replay.

## Non-Goals

- No re-encoding or migration of old recordings.
- No persistence of reconstructed channels beyond the replay session (they are
  added to the in-memory registry; the live channel tree is restored on close).
- No change to the recording path or to live ingest.
- Topics in the file that are neither in `channels.toml` nor shaped like a
  generated message (no `value`/`t_ns` fields) are skipped, not guessed.

## Background (current code)

- `PlaybackStore::load(path, registry, schema: &ProtoSchema)` builds a
  `TopicRouter` from `(registry, schema)` and, for each MCAP message, calls
  `decode_batch(&msg.data, router.bindings_for(&msg.channel.topic), &store)`.
  It never inspects `msg.channel.schema`.
- `ProtoSchema { pool: DescriptorPool, schema_bytes: Vec<u8> }` with
  `from_path`, `from_bytes(&[u8])`, `resolve(proto_path, ts_path) -> ChannelDesc`,
  `pool_for_test()`.
- `ChannelBinding { id: ChannelId, msg_desc: MessageDescriptor, val_path: Vec<String>, ts_path: Vec<String>, eu_scale: f64, eu_offset: f64, sample_type: SampleType }`.
- `TopicRouter::build(registry, schema)` iterates registry channels, and for each
  `(topic, proto_path, ts_path)` resolves field paths against `schema`.
- `ChannelRegistry::add_dynamic(name, mqtt_topic, sample_type) -> ChannelId`
  appends an in-memory channel (used today for live MQTT drops).
- `ChannelTree::build(registry)` builds the sidebar tree from the registry.
- The recorder writes each MQTT channel with `mcap::Schema { name: <raw topic>, encoding: "protobuf", data: <self-contained FileDescriptorSet> }`; the generated
  message inside is named `mqtt.<Sanitized(topic)>` (field 1 `t_ns` int64,
  field 2 `value` typed). ZMQ channels share one `Schema` (the ingest
  `FileDescriptorSet`).

## Components

### 1. `ProtoSchema::from_descriptor_sets` (`src/ingest/loader.rs`)

Build one `ProtoSchema` by merging several embedded `FileDescriptorSet`s (the
ZMQ shared schema plus each MQTT per-topic schema).

```rust
/// Build a schema pool by merging multiple encoded FileDescriptorSets (e.g. the
/// per-channel schemas embedded in an MCAP file). Files with duplicate names are
/// skipped (prost-reflect dedups). Self-contained files (no imports) may be added
/// in any order. An individual set that fails to decode or add is logged and
/// skipped rather than aborting the whole load, so one malformed channel schema
/// does not make the recording unopenable.
pub fn from_descriptor_sets(sets: &[&[u8]]) -> Self {
    let mut pool = DescriptorPool::new();
    for bytes in sets {
        match FileDescriptorSet::decode(*bytes) {
            Ok(fds) => {
                if let Err(e) = pool.add_file_descriptor_set(fds) {
                    eprintln!("replay: skipping embedded schema: {e}");
                }
            }
            Err(e) => eprintln!("replay: skipping undecodable embedded schema: {e}"),
        }
    }
    let schema_bytes = pool.encode_to_vec();
    Self { pool, schema_bytes }
}

/// Look up a message descriptor by fully-qualified name.
pub fn message_by_name(&self, name: &str) -> Option<MessageDescriptor> {
    self.pool.get_message_by_name(name)
}
```

### 2. Message-descriptor + type helpers (`src/record/playback.rs`, private)

```rust
/// The fully-qualified name of the single message defined by an embedded schema,
/// e.g. "mqtt.HomeSensorsTemperature". Returns None if the set defines no message.
fn first_message_name(schema_bytes: &[u8]) -> Option<String> {
    let fds = FileDescriptorSet::decode(schema_bytes).ok()?;
    for file in &fds.file {
        if let Some(msg) = file.message_type.first() {
            let name = msg.name();
            return Some(match file.package() {
                "" => name.to_string(),
                pkg => format!("{pkg}.{name}"),
            });
        }
    }
    None
}

/// Map the `value` field's protobuf kind to a SampleType.
fn value_sample_type(desc: &MessageDescriptor) -> Option<SampleType> {
    use prost_reflect::Kind;
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
```

### 3. `PlaybackStore::load` — self-describing (`src/record/playback.rs`)

New signature (the `&ProtoSchema` parameter is removed):

```rust
pub fn load(path: &Path, registry: &ChannelRegistry) -> anyhow::Result<Arc<Self>>
```

Algorithm:

1. Read the file into memory. Iterate `mcap::MessageStream` once to collect a
   `BTreeMap<String, Vec<u8>>` of distinct `topic -> schema_bytes` from
   `msg.channel` (skip channels whose `schema` is `None`). Keep the raw bytes
   buffer for a second decode pass.
2. Build the merged pool: `let merged = ProtoSchema::from_descriptor_sets(&all_schema_bytes);`
   where `all_schema_bytes` is the distinct schema byte-slices. (Infallible — bad
   sets are skipped internally.)
3. Known ZMQ topics: `let router = TopicRouter::build(registry, &merged);`. This
   produces bindings for every registry channel whose `(topic, proto_path,
   ts_path)` resolves against the merged pool.
4. Reconstruct unknown topics. For each collected `topic` that has **no**
   `router.bindings_for(topic)` entries:
   - `let Some(full) = first_message_name(&schema_bytes) else { continue };`
   - `let Some(desc) = merged.message_by_name(&full) else { continue };`
   - `let Some(st) = value_sample_type(&desc) else { continue };`
   - Require the message to have a `t_ns` field: `desc.get_field_by_name("t_ns").is_some()` else skip.
   - `let id = registry.add_dynamic(topic, topic, st);`
   - Build a synthetic binding:
     `ChannelBinding { id, msg_desc: desc, val_path: vec!["value".into()], ts_path: vec!["t_ns".into()], eu_scale: 1.0, eu_offset: 0.0, sample_type: st }`.
   - Collect these into a `HashMap<String, Vec<ChannelBinding>>` keyed by topic.
5. Build the store's channel vec from the **now-extended** registry (same
   construction the current `load` uses, but after `add_dynamic` calls so the new
   channels are included).
6. Decode pass: iterate `mcap::MessageStream` again; for each message, look up
   bindings — first `router.bindings_for(topic)` (ZMQ), else the reconstructed
   map — and call `decode_batch(&msg.data, bindings, &store)`. A topic with no
   bindings in either source is skipped.
7. Return the `Arc<PlaybackStore>`.

Timestamps: the MQTT recorder always writes `t_ns` (= record time), and ZMQ
messages carry their own timestamp, so `decode`'s existing behavior is correct;
no `log_time` fallback is introduced.

### 4. App wiring (`src/app.rs`)

`open_recording`:
- Delete the `if self.ingest_schema_bytes.is_empty() { ... return; }` gate and the
  `ProtoSchema::from_bytes(&self.ingest_schema_bytes)` reconstruction.
- Call `PlaybackStore::load(&path, &self.channels)`.
- On success, snapshot the current channel tree into a new field
  `saved_channel_tree: Option<ChannelTree>` (set to `Some(std::mem::take/clone of
  self.channel_tree)`), then rebuild `self.channel_tree =
  ChannelTree::build(&self.channels)` so reconstructed topics are droppable.
- Status: `format!("Loaded {}", path.display())` (unchanged on success).

`close_replay`:
- If `saved_channel_tree` is `Some`, restore it into `self.channel_tree` and clear
  the field; this hides the reconstructed channels again in live mode.

`ChannelTree` must be `Clone` (derive it) to snapshot/restore. It is a plain tree
of `Node`s; deriving `Clone` is sufficient.

## Error Handling

- A channel with `schema: None` → skipped (its topic won't reconstruct).
- Embedded schema fails to decode / add to the pool → `from_descriptor_sets`
  logs and skips that individual set (infallible); the affected topic simply
  won't reconstruct. A single malformed per-topic schema never aborts the load.
- Unknown topic whose message lacks `value` or `t_ns`, or whose `value` kind is
  unmapped → skipped (not guessed).
- File with zero decodable channels → the store loads empty; the app shows
  `"Loaded <path>"` and panels show "no data". (A recording with no usable
  channels is not an error.)

## Testing

`src/record/playback.rs` tests (extend the existing MCAP-writing test helper):

- **MQTT reconstruction:** write an MCAP whose only channel is an MQTT-style
  message (topic `"home/temp"` NOT in the registry; embedded self-contained
  `FileDescriptorSet` for `mqtt.HomeTemp { t_ns=1 int64; value=2 double }`; one
  message with `t_ns` and `value` set). `PlaybackStore::load(path, registry)`
  must: register a channel named `"home/temp"` of type `Float`, and a snapshot of
  it returns the sample value at its timestamp.
- **ZMQ without live schema:** write an MCAP for a registry topic (e.g. the
  existing `accel` fixture) with the batch schema embedded as the channel schema,
  and load with only `(path, registry)` — no `ProtoSchema` argument. Assert the
  samples decode (proving the ingest-schema dependency is gone).
- **Mixed file:** one MCAP containing both a registry ZMQ batch topic and an
  unknown MQTT topic; assert both decode into their channels.
- **`from_descriptor_sets` merge:** two self-contained sets merge into one pool
  that resolves a message from each.
- **Type mapping:** `value_sample_type` for double/int64/bool/string messages
  returns Float/Int/Bool/Text respectively.

The existing playback tests are updated to the new `load(path, registry)`
signature (drop the `&schema` argument; the schema now comes from the file the
test writes).

## Notes for Planning

- `message_by_name` (Component 1) is the production pool accessor `playback.rs`
  uses; `pool_for_test()` stays test-only and untouched.
- The synthetic `ChannelBinding` reuses the exact struct the router produces, so
  `decode_batch`/`decode_single_channel` need no changes.
- `add_dynamic` persists channels in the shared registry for the process
  lifetime; the channel-tree snapshot/restore keeps them out of the live picker
  after replay closes. This matches how live MQTT drops already behave.
