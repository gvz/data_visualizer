# Self-Describing MCAP Replay — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make opening an MCAP recording decode all channels from the schemas embedded in the file itself, with no dependency on a live ingest schema — fixing both the "no proto schema" refusal and the inability to replay MQTT recordings.

**Architecture:** `PlaybackStore::load` reads each `mcap::Channel`'s embedded `Schema`, merges them into one `ProtoSchema` pool, routes known ZMQ topics via `channels.toml` (existing `TopicRouter`), and reconstructs unknown/MQTT topics as `value`/`t_ns` channels (`ChannelRegistry::add_dynamic`) with synthetic `ChannelBinding`s. The app drops its `ingest_schema_bytes` gate and rebuilds the channel picker for replay.

**Tech Stack:** Rust, `prost-reflect` 0.14 (`DescriptorPool`, `MessageDescriptor`, `Kind`), `prost-types` 0.13 (`FileDescriptorSet`), `mcap` 0.8, egui.

## Global Constraints

- Never add Co-Authored-By, self-attribution, or AI identification to any commit.
- Do not add or enable `cache.numtide.com`; the flake sets `extra-substituters = []`.
- Reconstructed unknown/MQTT topics use the generated-message convention: message field `t_ns` (int64) and field `value` (typed); `val_path = ["value"]`, `ts_path = ["t_ns"]`, `eu_scale = 1.0`, `eu_offset = 0.0`.
- The message descriptor for a topic is found from that channel's embedded `FileDescriptorSet` (its single message type) — NOT by assuming `mcap::Schema.name` equals the message name (the recorder sets schema name to the raw topic; the message is `mqtt.<Sanitized(topic)>`).
- Type mapping from the `value` field kind: Double/Float→Float; Int64/Int32/Uint64/Uint32/Sint64/Sint32/Fixed64/Fixed32/Sfixed64/Sfixed32→Int; Bool→Bool; String→Text; anything else → skip the topic.
- Topics in the file that are neither in `channels.toml` nor shaped like a generated message (missing `value` or `t_ns`) are skipped, not guessed.
- Reconstructed channels are not persisted beyond the session; the live channel tree is restored on replay close.
- Every existing test must keep passing. The only acceptable pre-existing warning is an unrelated binrw future-incompat note.
- Work continues on branch `mqtt-proto-schema-recording`.

---

## File Structure

- **Modify** `src/ingest/loader.rs` — add `ProtoSchema::from_descriptor_sets(&[&[u8]]) -> Self` (merge embedded schemas, skip bad ones) and `ProtoSchema::message_by_name(&str) -> Option<MessageDescriptor>`.
- **Modify** `src/record/playback.rs` — private helpers `first_message_name` / `value_sample_type`; rewrite `load` to be self-describing (new 2-arg signature); update the test MCAP writer to embed schema bytes; update existing tests; add reconstruction tests.
- **Modify** `src/channel_tree.rs` — derive `Clone` on `Node` and `ChannelTree`.
- **Modify** `src/app.rs` — drop the `ingest_schema_bytes` replay gate, call the new `load`, snapshot/rebuild the channel tree for replay, restore it on close.

---

## Task 1: `ProtoSchema::from_descriptor_sets` + `message_by_name`

**Files:**
- Modify: `src/ingest/loader.rs`

**Interfaces:**
- Consumes: `prost_types::FileDescriptorSet`, `prost_reflect::{DescriptorPool, MessageDescriptor}`, `prost_reflect::prost::Message` (for `decode`/`encode_to_vec`).
- Produces: `pub fn ProtoSchema::from_descriptor_sets(sets: &[&[u8]]) -> Self` and `pub fn ProtoSchema::message_by_name(&self, name: &str) -> Option<MessageDescriptor>`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/ingest/loader.rs`:

```rust
    #[test]
    fn from_descriptor_sets_merges_and_resolves() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let pa = dir.path().join("a.proto");
        let pb = dir.path().join("b.proto");
        write!(std::fs::File::create(&pa).unwrap(),
            "syntax = \"proto3\";\nmessage MsgA {{ int64 t = 1; double v = 2; }}\n").unwrap();
        write!(std::fs::File::create(&pb).unwrap(),
            "syntax = \"proto3\";\nmessage MsgB {{ int64 t = 1; bool v = 2; }}\n").unwrap();
        let sa = ProtoSchema::from_path(&pa).unwrap();
        let sb = ProtoSchema::from_path(&pb).unwrap();

        let merged = ProtoSchema::from_descriptor_sets(&[sa.schema_bytes(), sb.schema_bytes()]);
        assert!(merged.message_by_name("MsgA").is_some());
        assert!(merged.message_by_name("MsgB").is_some());
        assert!(merged.message_by_name("MsgC").is_none());
    }

    #[test]
    fn from_descriptor_sets_skips_garbage() {
        // A garbage set is skipped; a valid one still resolves.
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let pa = dir.path().join("a.proto");
        write!(std::fs::File::create(&pa).unwrap(),
            "syntax = \"proto3\";\nmessage MsgA {{ int64 t = 1; double v = 2; }}\n").unwrap();
        let sa = ProtoSchema::from_path(&pa).unwrap();
        let garbage: &[u8] = b"not a descriptor set";
        let merged = ProtoSchema::from_descriptor_sets(&[garbage, sa.schema_bytes()]);
        assert!(merged.message_by_name("MsgA").is_some());
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib ingest::loader::tests::from_descriptor_sets 2>&1 | grep -E "cannot find|error\[" | head`
Expected: errors — `from_descriptor_sets` / `message_by_name` not found.

- [ ] **Step 3: Implement the two methods**

In `src/ingest/loader.rs`, add inside `impl ProtoSchema` (after `from_bytes`):

```rust
    /// Build a schema pool by merging multiple encoded FileDescriptorSets (e.g.
    /// the per-channel schemas embedded in an MCAP file). Files with duplicate
    /// names are skipped (prost-reflect dedups). Self-contained files (no imports)
    /// may be added in any order. An individual set that fails to decode or add is
    /// logged and skipped, so one malformed channel schema does not make the
    /// recording unopenable.
    pub fn from_descriptor_sets(sets: &[&[u8]]) -> Self {
        use prost_reflect::prost::Message as _;
        let mut pool = DescriptorPool::new();
        for bytes in sets {
            match prost_types::FileDescriptorSet::decode(*bytes) {
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

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib ingest::loader::tests::from_descriptor_sets 2>&1 | tail -8`
Expected: PASS — both `from_descriptor_sets_merges_and_resolves` and `from_descriptor_sets_skips_garbage`.

- [ ] **Step 5: Full suite**

Run: `cargo test --lib 2>&1 | tail -3`
Expected: all pass (146 existing + 2 new = 148).

- [ ] **Step 6: Commit**

```bash
git add src/ingest/loader.rs
git commit -m "feat: ProtoSchema::from_descriptor_sets merges embedded MCAP schemas"
```

---

## Task 2: Self-describing `PlaybackStore::load`

**Files:**
- Modify: `src/record/playback.rs`

**Interfaces:**
- Consumes: `ProtoSchema::{from_descriptor_sets, message_by_name}` (Task 1); `crate::ingest::router::{ChannelBinding, TopicRouter}`; `crate::ingest::decode::decode_batch`; `ChannelRegistry::add_dynamic`; `crate::record::mqtt_schema::DynamicProtoRegistry` (tests only).
- Produces: `pub fn PlaybackStore::load(path: &Path, registry: &ChannelRegistry) -> anyhow::Result<Arc<Self>>` (the `&ProtoSchema` parameter is removed); private `first_message_name(&[u8]) -> Option<String>` and `value_sample_type(&MessageDescriptor) -> Option<SampleType>`.

- [ ] **Step 1: Add imports and the two private helpers**

In `src/record/playback.rs`, extend the top-of-file imports:

```rust
use std::collections::{BTreeMap, HashMap};

use crate::ingest::router::{ChannelBinding, TopicRouter};
use prost_reflect::{Kind, MessageDescriptor};
```

(Keep the existing imports; `ProtoSchema` stays imported — still used by `from_descriptor_sets`. Remove nothing yet.)

Add these free functions above `impl PlaybackStore`:

```rust
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
```

- [ ] **Step 2: Rewrite `load` with the new signature**

Replace the entire `pub fn load(...) { ... }` (currently lines 115–135) with:

```rust
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
```

- [ ] **Step 3: Update the test MCAP writer to embed the schema**

In the `tests` module, in `write_test_mcap`, change the schema data from empty to the real bytes so `load` can read it back. Replace:

```rust
        let mcap_schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Borrowed(&[]),
        });
```

with:

```rust
        let mcap_schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(schema.schema_bytes().to_vec()),
        });
```

- [ ] **Step 4: Update existing `load` call sites in tests to the 2-arg signature**

In `src/record/playback.rs` tests, both `load_and_snapshot_returns_data_in_window` and `now_ns_returns_position_not_wall_clock` call `PlaybackStore::load(&path, &registry, &schema)`. Change both to `PlaybackStore::load(&path, &registry)` (keep `schema` — it is still used by `write_test_mcap`). Do the same for any other `PlaybackStore::load(` call in the tests module (there are additional ones around the window/text tests — update every occurrence to drop the `&schema` argument).

- [ ] **Step 5: Run existing playback tests (now proving ZMQ-without-live-schema)**

Run: `cargo test --lib record::playback 2>&1 | tail -12`
Expected: PASS — the existing tests now load using only the embedded schema (the `&schema` arg is gone), which is exactly the "ZMQ recording replays without a live schema" case.

- [ ] **Step 6: Add the MQTT reconstruction test**

Add to the `tests` module in `src/record/playback.rs`:

```rust
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

        let window = TimeWindow { start_ns: 1_000_000_000, end_ns: 2_000_000_000 };
        match store.snapshot(id, window) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                assert!((vals[0] - 21.5).abs() < 1e-4);
                assert!((vals[1] - 22.0).abs() < 1e-4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
```

- [ ] **Step 7: Add the mixed-file test**

Add to the `tests` module:

```rust
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
```

- [ ] **Step 8: Add the `value_sample_type` mapping test**

Add to the `tests` module:

```rust
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
```

- [ ] **Step 9: Run the full suite**

Run: `cargo test --lib 2>&1 | tail -4`
Expected: all pass (148 from Task 1 + 4 new here = 152). No new warnings (`cargo build 2>&1 | grep -v binrw | grep warning` prints nothing).

- [ ] **Step 10: Verify the app still compiles against the new signature**

The app's `open_recording` still calls the old 3-arg `load`. Update that call site minimally now so the crate builds (the picker rebuild comes in Task 3). In `src/app.rs`, replace the replay-schema block (currently lines 235–246) — the `is_empty` gate, the `ProtoSchema::from_bytes(...)` match, and the `PlaybackStore::load(&path, &self.channels, &schema)` call — with a direct load:

```rust
        match PlaybackStore::load(&path, &self.channels) {
```

so the surrounding `match ... { Ok(playback) => { ... } Err(e) => { ... } }` is preserved. Remove the now-unused `schema` binding and the `is_empty` early return. Leave `self.ingest_schema_bytes` in place (still used by `start_recording`).

Run: `cargo build 2>&1 | grep -E "error|warning" | grep -v binrw | head`
Expected: no output (clean build). If `use crate::ingest::loader::ProtoSchema;` in `app.rs` becomes unused, remove that import.

- [ ] **Step 11: Commit**

```bash
git add src/record/playback.rs src/app.rs
git commit -m "feat: PlaybackStore::load reads schemas from the MCAP; reconstructs MQTT topics"
```

---

## Task 3: Replay channel picker rebuild

**Files:**
- Modify: `src/channel_tree.rs`
- Modify: `src/app.rs`

**Interfaces:**
- Consumes: `ChannelTree::build` (existing); `PlaybackStore::load(path, registry)` (Task 2, already wired into `open_recording`).
- Produces: `#[derive(Clone)]` on `ChannelTree`; app field `saved_channel_tree: Option<ChannelTree>`; `open_recording` rebuilds the picker, `close_replay` restores it.

- [ ] **Step 1: Derive `Clone` on the channel tree**

In `src/channel_tree.rs`, add `#[derive(Clone)]` to both types:

```rust
#[derive(Clone)]
enum Node {
    Group { label: String, children: Vec<Node> },
    Leaf  { label: String, full_name: String, value: Option<String> },
}
```

```rust
#[derive(Clone)]
pub struct ChannelTree {
    roots: Vec<Node>,
}
```

- [ ] **Step 2: Add the saved-tree field to the app**

In `src/app.rs`, add a field to the `DataVisApp` struct next to `channel_tree`:

```rust
    /// Live channel tree saved while a replay rebuilds the picker; restored on close.
    saved_channel_tree: Option<ChannelTree>,
```

Initialize it in `DataVisApp::new` (in the struct literal, near `channel_tree`):

```rust
            saved_channel_tree: None,
```

- [ ] **Step 3: Rebuild the picker after a successful load**

In `src/app.rs` `open_recording`, in the `Ok(playback) => { ... }` arm (after `self.mode = AppMode::Replay(...)` and setting status), add:

```rust
                self.saved_channel_tree = Some(self.channel_tree.clone());
                self.channel_tree = ChannelTree::build(&self.channels);
```

- [ ] **Step 4: Restore the picker on close**

In `src/app.rs` `close_replay`, add before setting the status line:

```rust
        if let Some(tree) = self.saved_channel_tree.take() {
            self.channel_tree = tree;
        }
```

- [ ] **Step 5: Write a test for save/restore of the tree**

Add to the `tests` module in `src/app.rs`:

```rust
    #[test]
    fn channel_tree_clone_roundtrips_and_rebuild_adds_dynamic() {
        use crate::config::ChannelRegistry;
        use crate::channel_tree::ChannelTree;
        use crate::types::SampleType;

        let reg = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap();
        let saved = ChannelTree::build(&reg);
        let restored = saved.clone(); // Clone must be available

        // After a dynamic add, the channel exists and a freshly built tree is the
        // one open_recording swaps in.
        let id = reg.add_dynamic("home/temp", "home/temp", SampleType::Float);
        assert_eq!(reg.meta(id).sample_type, SampleType::Float);
        assert!(reg.id("home/temp").is_some());
        let _rebuilt = ChannelTree::build(&reg);

        // `restored` is what close_replay puts back; it must be usable (Clone worked).
        let _restored_again = restored.clone();
        let _ = saved; // built from the pre-add registry, independent of the rebuild
    }
```

(If `ChannelTree` is not re-exported for the test path, use the crate path it is declared under; `crate::channel_tree::ChannelTree` matches the module.)

- [ ] **Step 6: Run the suite**

Run: `cargo test --lib 2>&1 | tail -4`
Expected: all pass (152 + 1 = 153). Clean build: `cargo build 2>&1 | grep -E "error|warning" | grep -v binrw` prints nothing.

- [ ] **Step 7: Commit**

```bash
git add src/channel_tree.rs src/app.rs
git commit -m "feat: rebuild replay channel picker from reconstructed channels; restore on close"
```

---

## Manual Verification (after all tasks)

- [ ] With a recorded MCAP that contains MQTT topics (produced by the recording feature), run the app in demo mode: `cargo run --release -- --demo`, click **Open recording**, pick the file.
- [ ] Confirm it loads (status `Loaded <path>`, no "Replay not available in demo mode" message) and the MQTT topics appear in the sidebar and can be dropped onto a panel to show the replayed data.
- [ ] Close the replay and confirm the sidebar returns to the live channel set.
