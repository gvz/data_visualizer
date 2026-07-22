# On-the-Fly MQTT Protobuf Schema Generation for MCAP Recording — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record all discovered MQTT topics into MCAP by generating a protobuf schema per topic at runtime and encoding each scalar sample into a protobuf message.

**Architecture:** A new `DynamicProtoRegistry` (owned by the MQTT ingest thread) hand-builds a `FileDescriptorProto` per topic via `prost-types`, caches the resulting `MessageDescriptor` + a self-contained `FileDescriptorSet` schema, and encodes each sample as a `prost_reflect::DynamicMessage`. `RecordMsg` becomes an enum so MQTT frames can carry their own schema; the MCAP recorder lazily registers a channel+schema per topic. Recording senders are collected from every active ingest source (ZMQ and/or MQTT).

**Tech Stack:** Rust, `prost-types` 0.13 (raw descriptors), `prost-reflect` 0.14 (`DescriptorPool`, `DynamicMessage`), `mcap` 0.8, `crossbeam-channel` (mpmc record queue), `rumqttc` (MQTT).

## Global Constraints

- Never add Co-Authored-By, self-attribution, or AI identification to any commit or PR.
- Do not add or enable `cache.numtide.com`; the flake sets `extra-substituters = []`.
- Sample types are the existing `crate::types::SampleType` enum: `Float | Int | Bool | Text`.
- Generated messages use package `mqtt`, one message type per topic, fields `t_ns` (field 1, int64) and `value` (field 2, typed).
- MQTT samples are stamped with `crate::types::now_ns()` at receipt for both the MCAP `log_time` and the `t_ns` field.
- Bool true set: `"1" "true" "True" "TRUE" "on" "ON" "yes" "YES"`; false set: `"0" "false" "False" "FALSE" "off" "OFF" "no" "NO"`. Inference uses the textual tokens only (true/false sets minus `"1"`/`"0"`); numeric `"1"`/`"0"` infer Int.
- Every existing test must keep passing (`cargo test --lib`). The only pre-existing warning is an unrelated binrw future-incompat note.

---

## File Structure

- **Create** `src/record/mqtt_schema.rs` — `DynamicProtoRegistry`: per-topic schema generation, type inference, sample encoding. Pure logic, no I/O.
- **Modify** `src/record/queue.rs` — replace the `RecordMsg` tuple with an enum (`Proto` / `DynamicProto`).
- **Modify** `src/record/writer.rs` — recorder loop matches the enum; lazy per-topic channel+schema registration for `DynamicProto`.
- **Modify** `src/record/mod.rs` — declare the new `mqtt_schema` module.
- **Modify** `src/ingest/thread.rs` — ZMQ producer builds `RecordMsg::Proto`.
- **Modify** `src/ingest/mqtt.rs` — `MqttHandles` gains `record_sender`; run loop encodes+sends `RecordMsg::DynamicProto` via a testable helper `record_publish`.
- **Modify** `src/app.rs` — `record_sender_slot: Option<...>` → `record_sender_slots: Vec<...>`; start/stop iterate all; availability message.
- **Modify** `src/main.rs` — collect record senders from ZMQ + MQTT handles into a `Vec`, pass to `DataVisApp::new`.

---

## Task 1: `RecordMsg` enum + recorder support for both variants

**Files:**
- Modify: `src/record/queue.rs`
- Modify: `src/record/writer.rs`
- Modify: `src/ingest/thread.rs:62-67`

**Interfaces:**
- Produces: `pub enum RecordMsg { Proto { topic: Arc<str>, data: Vec<u8>, ts: i64 }, DynamicProto { topic: Arc<str>, schema: Arc<[u8]>, data: Vec<u8>, ts: i64 } }`
- Produces: recorder writes `Proto` frames against the shared start-time schema (unchanged) and lazily registers a per-topic channel+schema for `DynamicProto` frames.
- Consumes: `mcap::{Writer, Channel, Schema, records::MessageHeader}` (already used in `writer.rs`).

- [ ] **Step 1: Replace the RecordMsg type with an enum**

In `src/record/queue.rs`, replace `pub type RecordMsg = (Arc<str>, Vec<u8>, i64);` with:

```rust
/// A frame queued for the MCAP recorder.
///
/// `Proto` carries a message encoded against the shared schema registered when
/// recording starts (the ZMQ ingest path). `DynamicProto` carries its own
/// self-contained protobuf schema (the MQTT path, where schemas are generated
/// per topic at runtime).
#[derive(Debug, Clone)]
pub enum RecordMsg {
    Proto {
        topic: Arc<str>,
        data: Vec<u8>,
        ts: i64,
    },
    DynamicProto {
        topic: Arc<str>,
        schema: Arc<[u8]>,
        data: Vec<u8>,
        ts: i64,
    },
}
```

Update the two tests in `src/record/queue.rs` to the enum. Replace the body of `queue_roundtrip`:

```rust
    #[test]
    fn queue_roundtrip() {
        let (tx, rx) = record_channel();
        let topic: Arc<str> = Arc::from("accel");
        let data = vec![1u8, 2, 3];
        tx.try_send(RecordMsg::Proto { topic: topic.clone(), data: data.clone(), ts: 42_000_000 })
            .unwrap();
        match rx.try_recv().unwrap() {
            RecordMsg::Proto { topic: t, data: d, ts } => {
                assert_eq!(t.as_ref(), "accel");
                assert_eq!(d, data);
                assert_eq!(ts, 42_000_000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
```

Replace the body of `queue_full_returns_err_does_not_block`:

```rust
    #[test]
    fn queue_full_returns_err_does_not_block() {
        let (tx, _rx) = record_channel();
        for i in 0..QUEUE_CAP {
            tx.try_send(RecordMsg::Proto { topic: Arc::from("t"), data: vec![i as u8], ts: i as i64 })
                .unwrap();
        }
        assert!(tx
            .try_send(RecordMsg::Proto { topic: Arc::from("t"), data: vec![], ts: 0 })
            .is_err());
    }
```

- [ ] **Step 2: Build — expect compile errors in producers/consumers**

Run: `cargo build 2>&1 | grep -E "error" | head`
Expected: errors in `writer.rs` (tuple pattern `Ok((topic, data, ts))`) and `thread.rs` (tuple `try_send`). These are fixed in the next steps.

- [ ] **Step 3: Update the ZMQ producer to build `RecordMsg::Proto`**

In `src/ingest/thread.rs`, replace the `try_send` line (currently line 66):

```rust
                        let _ = tx.try_send(crate::record::RecordMsg::Proto {
                            topic: topic_arc,
                            data: parts[1].clone(),
                            ts: log_time_ns,
                        });
```

- [ ] **Step 4: Teach the recorder to register dynamic per-topic schemas**

In `src/record/writer.rs`, add a method on `McapRecorder` that lazily registers a channel with its own schema and writes a message. Insert after `write_msg` (after line 84):

```rust
    /// Write a message whose protobuf schema is supplied inline. Registers a new
    /// MCAP channel (with its own schema) the first time a topic is seen.
    fn write_dynamic(
        &mut self,
        topic: &str,
        schema_bytes: &[u8],
        data: &[u8],
        log_time_ns: i64,
    ) -> anyhow::Result<()> {
        if !self.channel_ids.contains_key(topic) {
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
            self.channel_ids.insert(topic.to_string(), channel_id);
        }
        self.write_msg(topic, data, log_time_ns)
    }
```

- [ ] **Step 5: Update the recorder loop to match on the enum**

In `src/record/writer.rs`, replace the `recv(record_rx)` arm and the drain loop in `recorder_loop` (lines 112-122):

```rust
            recv(record_rx) -> result => match result {
                Ok(msg) => write_record(&mut recorder, msg)?,
                Err(_) => break,
            },
            recv(stop_rx) -> _ => break,
        }
    }
    // Drain any messages that arrived before stop.
    while let Ok(msg) = record_rx.try_recv() {
        write_record(&mut recorder, msg)?;
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
```

- [ ] **Step 6: Update existing writer tests to the enum**

In `src/record/writer.rs` tests, replace the two `roundtrip_write_read` sends (lines 178-181):

```rust
        tx.try_send(RecordMsg::Proto { topic: Arc::from("accel"), data: vec![0x01, 0x02], ts: 1_000_000_000 })
            .unwrap();
        tx.try_send(RecordMsg::Proto { topic: Arc::from("accel"), data: vec![0x03, 0x04], ts: 2_000_000_000 })
            .unwrap();
```

Replace the two `unknown_topic_messages_are_skipped` sends (lines 221-222):

```rust
        tx.try_send(RecordMsg::Proto { topic: Arc::from("gyro"), data: vec![0xFF], ts: 1_000 }).unwrap();
        tx.try_send(RecordMsg::Proto { topic: Arc::from("accel"), data: vec![0x01], ts: 2_000 }).unwrap();
```

Ensure `use crate::record::queue::RecordMsg;` is present in the writer test module (add `use super::*;` already re-exports it via `use crate::record::queue::RecordMsg;` at top of file — it is imported at line 13, so `RecordMsg` is in scope in tests through `use super::*;`).

- [ ] **Step 7: Add a test — DynamicProto lazily registers per-topic schemas**

Add to the `tests` module in `src/record/writer.rs`:

```rust
    #[test]
    fn dynamic_proto_registers_per_topic_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let registry = minimal_registry();
        let (tx, rx) = record_channel();
        // Shared schema empty: MQTT topics carry their own.
        let handle = start_recording(dir.path(), &registry, vec![], rx).unwrap();

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
```

- [ ] **Step 8: Run the record tests**

Run: `cargo test --lib record:: 2>&1 | tail -20`
Expected: PASS — `queue::tests`, `writer::tests::roundtrip_write_read`, `unknown_topic_messages_are_skipped`, `dynamic_proto_registers_per_topic_schemas`.

- [ ] **Step 9: Full build + test**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: all tests pass (135 existing + 1 new = 136).

- [ ] **Step 10: Commit**

```bash
git add src/record/queue.rs src/record/writer.rs src/ingest/thread.rs
git commit -m "feat: RecordMsg enum with per-topic dynamic schemas in the MCAP recorder"
```

---

## Task 2: `DynamicProtoRegistry` — runtime schema generation + encoding

**Files:**
- Create: `src/record/mqtt_schema.rs`
- Modify: `src/record/mod.rs` (add `pub mod mqtt_schema;`)

**Interfaces:**
- Consumes: `crate::types::SampleType`, `prost_types::{FileDescriptorProto, DescriptorProto, FieldDescriptorProto, field_descriptor_proto::{Type, Label}, FileDescriptorSet}`, `prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, Value}`, `prost_reflect::prost::Message`.
- Produces: `pub struct DynamicProtoRegistry` with `pub fn new() -> Self`, `pub fn infer_type(payload: &str) -> SampleType`, `pub fn record_frame(&mut self, topic: &str, ts_ns: i64, payload: &str) -> Option<(Arc<[u8]>, Vec<u8>)>`. Returns `(schema_bytes, encoded_message_bytes)`; `None` on parse mismatch or schema build failure.

- [ ] **Step 1: Declare the module**

In `src/record/mod.rs`, add under the other `pub mod` lines (after line 6):

```rust
pub mod mqtt_schema;
```

- [ ] **Step 2: Write failing tests**

Create `src/record/mqtt_schema.rs` with only the tests first (implementation stubs added next step):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SampleType;
    use prost_reflect::{DynamicMessage, Value};
    use prost_reflect::prost::Message as _;

    #[test]
    fn infer_type_table() {
        assert_eq!(DynamicProtoRegistry::infer_type("42"), SampleType::Int);
        assert_eq!(DynamicProtoRegistry::infer_type("-7"), SampleType::Int);
        assert_eq!(DynamicProtoRegistry::infer_type("3.14"), SampleType::Float);
        assert_eq!(DynamicProtoRegistry::infer_type("true"), SampleType::Bool);
        assert_eq!(DynamicProtoRegistry::infer_type("off"), SampleType::Bool);
        assert_eq!(DynamicProtoRegistry::infer_type("hello"), SampleType::Text);
        // Numeric 1/0 infer Int, not Bool (i64-first ordering).
        assert_eq!(DynamicProtoRegistry::infer_type("1"), SampleType::Int);
        assert_eq!(DynamicProtoRegistry::infer_type("0"), SampleType::Int);
    }

    /// Encode a frame, then decode it back with the topic's generated descriptor
    /// (rebuilt from the returned self-contained FileDescriptorSet) and check the
    /// t_ns + value fields.
    fn decode_back(schema: &[u8], data: &[u8], msg_name: &str) -> DynamicMessage {
        let pool = prost_reflect::DescriptorPool::decode(schema).unwrap();
        let desc = pool.get_message_by_name(msg_name).unwrap();
        DynamicMessage::decode(desc, data).unwrap()
    }

    #[test]
    fn record_frame_float_roundtrip() {
        let mut reg = DynamicProtoRegistry::new();
        let (schema, data) = reg.record_frame("sensors/x", 111, "2.5").unwrap();
        let msg = decode_back(&schema, &data, "mqtt.SensorsX");
        assert_eq!(msg.get_field_by_name("t_ns").unwrap().as_i64(), Some(111));
        assert_eq!(msg.get_field_by_name("value").unwrap().as_f64(), Some(2.5));
    }

    #[test]
    fn record_frame_int_bool_text() {
        let mut reg = DynamicProtoRegistry::new();
        let (s1, d1) = reg.record_frame("a/count", 1, "7").unwrap();
        assert_eq!(decode_back(&s1, &d1, "mqtt.ACount").get_field_by_name("value").unwrap().as_i64(), Some(7));

        let (s2, d2) = reg.record_frame("a/flag", 2, "true").unwrap();
        assert_eq!(decode_back(&s2, &d2, "mqtt.AFlag").get_field_by_name("value").unwrap().as_bool(), Some(true));

        let (s3, d3) = reg.record_frame("a/msg", 3, "hi").unwrap();
        assert_eq!(
            decode_back(&s3, &d3, "mqtt.AMsg").get_field_by_name("value").unwrap().as_str(),
            Some("hi")
        );
    }

    #[test]
    fn locked_type_rejects_mismatch() {
        let mut reg = DynamicProtoRegistry::new();
        // First sample locks Int.
        assert!(reg.record_frame("t/n", 1, "5").is_some());
        // A non-numeric payload no longer matches the locked Int type.
        assert!(reg.record_frame("t/n", 2, "notanint").is_none());
    }

    #[test]
    fn bool_topic_records_numeric_after_textual_lock() {
        let mut reg = DynamicProtoRegistry::new();
        // Textual first sample locks Bool.
        assert!(reg.record_frame("t/b", 1, "on").is_some());
        // Later numeric 1/0 still encodes as bool.
        let (s, d) = reg.record_frame("t/b", 2, "0").unwrap();
        assert_eq!(decode_back(&s, &d, "mqtt.TB").get_field_by_name("value").unwrap().as_bool(), Some(false));
    }

    #[test]
    fn name_collision_disambiguated() {
        let mut reg = DynamicProtoRegistry::new();
        // Both sanitize to "AB".
        let (s1, _) = reg.record_frame("a/b", 1, "1").unwrap();
        let (s2, _) = reg.record_frame("a.b", 2, "1").unwrap();
        // Second topic's schema must define a differently-named message.
        let pool2 = prost_reflect::DescriptorPool::decode(s2.as_ref()).unwrap();
        assert!(pool2.get_message_by_name("mqtt.AB").is_none());
        assert!(pool2.get_message_by_name("mqtt.AB_2").is_some());
        // First topic keeps the base name.
        let pool1 = prost_reflect::DescriptorPool::decode(s1.as_ref()).unwrap();
        assert!(pool1.get_message_by_name("mqtt.AB").is_some());
    }
}
```

- [ ] **Step 3: Run tests to confirm they fail to compile**

Run: `cargo test --lib mqtt_schema 2>&1 | grep -E "error\[|cannot find" | head`
Expected: errors — `DynamicProtoRegistry` not found.

- [ ] **Step 4: Implement `DynamicProtoRegistry`**

Prepend the implementation above the `tests` module in `src/record/mqtt_schema.rs`:

```rust
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use prost_reflect::prost::Message as _;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, Value};
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet};

use crate::types::SampleType;

/// Bool payload tokens shared by inference and encoding.
const BOOL_TRUE: &[&str] = &["1", "true", "True", "TRUE", "on", "ON", "yes", "YES"];
const BOOL_FALSE: &[&str] = &["0", "false", "False", "FALSE", "off", "OFF", "no", "NO"];

struct TopicEntry {
    sample_type: SampleType,
    descriptor: MessageDescriptor,
    /// Self-contained FileDescriptorSet (this topic's one file) used as the MCAP
    /// protobuf schema payload.
    schema_bytes: Arc<[u8]>,
}

/// Generates a protobuf schema per MQTT topic at runtime and encodes scalar
/// samples into `DynamicMessage`s. Single-threaded owner (the MQTT ingest
/// thread); no internal locking.
pub struct DynamicProtoRegistry {
    pool: DescriptorPool,
    entries: HashMap<String, TopicEntry>,
    used_names: HashSet<String>,
}

impl DynamicProtoRegistry {
    pub fn new() -> Self {
        Self {
            pool: DescriptorPool::new(),
            entries: HashMap::new(),
            used_names: HashSet::new(),
        }
    }

    /// Infer a sample type from a payload: i64 → Int; else f64 → Float; else a
    /// textual bool token → Bool; else Text. Numeric `"1"`/`"0"` infer Int.
    pub fn infer_type(payload: &str) -> SampleType {
        if payload.parse::<i64>().is_ok() {
            SampleType::Int
        } else if payload.parse::<f64>().is_ok() {
            SampleType::Float
        } else if is_textual_bool(payload) {
            SampleType::Bool
        } else {
            SampleType::Text
        }
    }

    /// One-shot ingest hot path: lazily build the topic's schema on first sight
    /// (type inferred + locked), then encode the sample. Returns the topic's
    /// schema bytes and the encoded message, or `None` on parse mismatch or a
    /// schema build failure.
    pub fn record_frame(
        &mut self,
        topic: &str,
        ts_ns: i64,
        payload: &str,
    ) -> Option<(Arc<[u8]>, Vec<u8>)> {
        let sample_type = match self.entries.get(topic) {
            Some(e) => e.sample_type,
            None => {
                let ty = Self::infer_type(payload);
                self.build_entry(topic, ty)?;
                ty
            }
        };
        let value = parse_value(payload, sample_type)?;
        let entry = self.entries.get(topic)?;
        let mut msg = DynamicMessage::new(entry.descriptor.clone());
        msg.set_field_by_name("t_ns", Value::I64(ts_ns));
        msg.set_field_by_name("value", value);
        Some((entry.schema_bytes.clone(), msg.encode_to_vec()))
    }

    /// Build and cache the generated message + schema for a new topic.
    fn build_entry(&mut self, topic: &str, sample_type: SampleType) -> Option<()> {
        let msg_name = self.unique_name(topic);
        let file = build_file_descriptor(&msg_name, sample_type);
        if self.pool.add_file_descriptor_proto(file.clone()).is_err() {
            return None;
        }
        let fq = format!("mqtt.{msg_name}");
        let descriptor = self.pool.get_message_by_name(&fq)?;
        let schema_bytes: Arc<[u8]> = FileDescriptorSet { file: vec![file] }
            .encode_to_vec()
            .into();
        self.entries.insert(
            topic.to_string(),
            TopicEntry { sample_type, descriptor, schema_bytes },
        );
        Some(())
    }

    /// A unique, valid protobuf message identifier derived from the topic.
    fn unique_name(&mut self, topic: &str) -> String {
        let base = sanitize(topic);
        let mut name = base.clone();
        let mut n = 2;
        while self.used_names.contains(&name) {
            name = format!("{base}_{n}");
            n += 1;
        }
        self.used_names.insert(name.clone());
        name
    }
}

fn is_textual_bool(payload: &str) -> bool {
    // Textual tokens only — numeric "1"/"0" are excluded so they infer Int.
    (BOOL_TRUE.contains(&payload) || BOOL_FALSE.contains(&payload))
        && payload != "1"
        && payload != "0"
}

fn parse_value(payload: &str, sample_type: SampleType) -> Option<Value> {
    match sample_type {
        SampleType::Float => payload.parse::<f64>().ok().map(Value::F64),
        SampleType::Int => payload.parse::<i64>().ok().map(Value::I64),
        SampleType::Bool => {
            if BOOL_TRUE.contains(&payload) {
                Some(Value::Bool(true))
            } else if BOOL_FALSE.contains(&payload) {
                Some(Value::Bool(false))
            } else {
                None
            }
        }
        SampleType::Text => Some(Value::String(payload.to_string())),
    }
}

/// CamelCase valid proto identifier from a topic; `T`-prefixed if empty or
/// digit-leading.
fn sanitize(topic: &str) -> String {
    let mut out = String::new();
    for segment in topic.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() || out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, 'T');
    }
    out
}

/// Hand-build a proto3 file with one message `<msg_name>` holding `t_ns` (int64)
/// and a typed `value` field.
fn build_file_descriptor(msg_name: &str, sample_type: SampleType) -> FileDescriptorProto {
    let value_type = match sample_type {
        SampleType::Float => Type::Double,
        SampleType::Int => Type::Int64,
        SampleType::Bool => Type::Bool,
        SampleType::Text => Type::String,
    };
    let field = |name: &str, number: i32, ty: Type| FieldDescriptorProto {
        name: Some(name.to_string()),
        number: Some(number),
        label: Some(Label::Optional as i32),
        r#type: Some(ty as i32),
        ..Default::default()
    };
    FileDescriptorProto {
        name: Some(format!("mqtt/{msg_name}.proto")),
        package: Some("mqtt".to_string()),
        syntax: Some("proto3".to_string()),
        message_type: vec![DescriptorProto {
            name: Some(msg_name.to_string()),
            field: vec![field("t_ns", 1, Type::Int64), field("value", 2, value_type)],
            ..Default::default()
        }],
        ..Default::default()
    }
}

impl Default for DynamicProtoRegistry {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 5: Run the module tests**

Run: `cargo test --lib mqtt_schema 2>&1 | tail -15`
Expected: PASS — `infer_type_table`, `record_frame_float_roundtrip`, `record_frame_int_bool_text`, `locked_type_rejects_mismatch`, `bool_topic_records_numeric_after_textual_lock`, `name_collision_disambiguated`.

- [ ] **Step 6: Commit**

```bash
git add src/record/mqtt_schema.rs src/record/mod.rs
git commit -m "feat: DynamicProtoRegistry generates per-topic protobuf schemas at runtime"
```

---

## Task 3: MQTT ingest records every discovered topic

**Files:**
- Modify: `src/ingest/mqtt.rs`

**Interfaces:**
- Consumes: `DynamicProtoRegistry::{new, record_frame}` (Task 2), `RecordMsg::DynamicProto` (Task 1).
- Produces: `MqttHandles.record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>`; a testable free fn `record_publish(reg: &mut DynamicProtoRegistry, sender: &Option<Sender<RecordMsg>>, topic: &str, payload: &str, ts: i64)`.

- [ ] **Step 1: Write a failing test for `record_publish`**

Add to the `tests` module in `src/ingest/mqtt.rs`:

```rust
    #[test]
    fn record_publish_sends_decodable_dynamic_proto() {
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        use crate::record::{record_channel, RecordMsg};

        let mut reg = DynamicProtoRegistry::new();
        let (tx, rx) = record_channel();
        let sender = Some(tx);

        record_publish(&mut reg, &sender, "home/temp", "21.5", 1_234);

        match rx.try_recv().unwrap() {
            RecordMsg::DynamicProto { topic, schema, data, ts } => {
                assert_eq!(topic.as_ref(), "home/temp");
                assert_eq!(ts, 1_234);
                let pool = prost_reflect::DescriptorPool::decode(schema.as_ref()).unwrap();
                let desc = pool.get_message_by_name("mqtt.HomeTemp").unwrap();
                let msg = prost_reflect::DynamicMessage::decode(desc, data.as_ref()).unwrap();
                assert_eq!(msg.get_field_by_name("value").unwrap().as_f64(), Some(21.5));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn record_publish_noop_when_no_sender() {
        use crate::record::mqtt_schema::DynamicProtoRegistry;
        let mut reg = DynamicProtoRegistry::new();
        // No sender → returns before touching the registry, so no type is locked.
        record_publish(&mut reg, &None, "x/y", "hello", 0);
        // A later Int sample therefore still infers Int (would be Text if the
        // earlier "hello" had locked the topic).
        let (schema, data) = reg.record_frame("x/y", 1, "5").unwrap();
        let pool = prost_reflect::DescriptorPool::decode(schema.as_ref()).unwrap();
        let desc = pool.get_message_by_name("mqtt.XY").unwrap();
        let msg = prost_reflect::DynamicMessage::decode(desc, data.as_ref()).unwrap();
        assert_eq!(msg.get_field_by_name("value").unwrap().as_i64(), Some(5));
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib ingest::mqtt 2>&1 | grep -E "cannot find|error\[" | head`
Expected: errors — `record_publish` not found.

- [ ] **Step 3: Add the `record_sender` field and `record_publish` helper**

In `src/ingest/mqtt.rs`:

Add to imports at the top:

```rust
use crate::record::mqtt_schema::DynamicProtoRegistry;
use crate::record::RecordMsg;
```

Extend `MqttHandles`:

```rust
pub struct MqttHandles {
    /// All received topics with their last payload, for the sidebar picker.
    pub discovered: Arc<Mutex<BTreeMap<String, String>>>,
    /// topic → (id, type); extended when a topic is dropped onto a panel.
    pub topic_map: Arc<MqttTopicMap>,
    /// Installed by the app while recording so the ingest thread queues frames.
    pub record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
}
```

Add the helper (place it near `run_loop`):

```rust
/// Encode one MQTT publish and queue it for the recorder, if recording is
/// active. Generates the topic's schema on first sight. A parse mismatch or a
/// full queue silently drops the sample.
fn record_publish(
    reg: &mut DynamicProtoRegistry,
    sender: &Option<crossbeam_channel::Sender<RecordMsg>>,
    topic: &str,
    payload: &str,
    ts: i64,
) {
    let Some(tx) = sender else { return };
    if let Some((schema, data)) = reg.record_frame(topic, ts, payload) {
        let _ = tx.try_send(RecordMsg::DynamicProto {
            topic: Arc::from(topic),
            schema,
            data,
            ts,
        });
    }
}
```

- [ ] **Step 4: Run the two new unit tests**

Run: `cargo test --lib ingest::mqtt::tests::record_publish 2>&1 | tail -10`
Expected: PASS — both `record_publish_*` tests.

- [ ] **Step 5: Wire `record_sender` through spawn + run loop**

In `spawn_mqtt_ingest`, create the shared sender slot, clone it into the thread, and return it. Replace the thread spawn + return (the `let disc = ...; let map = ...; std::thread::spawn(...); MqttHandles { discovered, topic_map }` block):

```rust
    let record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>> =
        Arc::new(Mutex::new(None));

    let disc = discovered.clone();
    let map = topic_map.clone();
    let rec = record_sender.clone();
    std::thread::spawn(move || {
        run_loop(opts, map, disc, store, rec);
    });

    MqttHandles { discovered, topic_map, record_sender }
```

Change `run_loop`'s signature to accept the sender and own a registry:

```rust
fn run_loop(
    opts: MqttOptions,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
) {
    let (client, mut connection) = Client::new(opts, 64);
    let mut proto_registry = DynamicProtoRegistry::new();
```

In the `Packet::Publish(p)` arm, after `discovered.lock().unwrap().insert(...)` and before the `topic_map` binding lookup, add the recording call (records ALL topics, independent of binding):

```rust
                let ts = crate::types::now_ns();
                if let Ok(guard) = record_sender.try_lock() {
                    record_publish(&mut proto_registry, &guard, &p.topic, payload_str.as_str(), ts);
                }
```

Then remove the later `let ts = crate::types::now_ns();` inside the `Some((id, sample_type))` block (it is now computed above; reuse `ts`). The store-write branch keeps using `ts`.

- [ ] **Step 6: Fix the existing `spawn_returns_discovered_set` test if needed**

The existing test only reads `handles.discovered`; the new field is additive, so it still compiles. Confirm with:

Run: `cargo test --lib ingest::mqtt 2>&1 | tail -15`
Expected: PASS — all `ingest::mqtt::tests`, including the two new ones.

- [ ] **Step 7: Full build + test**

Run: `cargo test --lib 2>&1 | tail -5`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add src/ingest/mqtt.rs
git commit -m "feat: MQTT ingest records all discovered topics via generated schemas"
```

---

## Task 4: Collect record senders from all ingest sources

**Files:**
- Modify: `src/app.rs:85,116,143,184-221`
- Modify: `src/main.rs:33-82`

**Interfaces:**
- Consumes: `IngestHandle.record_sender` (ZMQ), `MqttHandles.record_sender` (Task 3), `RecordMsg` (Task 1).
- Produces: `DataVisApp` records to every active ingest source; `DataVisApp::new` takes `record_sender_slots: Vec<Arc<Mutex<Option<Sender<RecordMsg>>>>>` instead of a single `Option<...>`.

- [ ] **Step 1: Change the app field to a Vec of slots**

In `src/app.rs`, replace the field (line 85):

```rust
    record_sender_slots: Vec<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
```

Replace the constructor parameter (line 116):

```rust
        record_sender_slots: Vec<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
```

Replace the struct-init line (line 143):

```rust
            record_sender_slots,
```

- [ ] **Step 2: Update `start_recording` and `stop_recording`**

Replace `start_recording` (lines 184-212):

```rust
    fn start_recording(&mut self) {
        if self.record_sender_slots.is_empty() {
            self.status = "Recording unavailable (no ingest source)".to_string();
            return;
        }
        let (tx, rx) = crate::record::record_channel();
        // Install the sender into every active ingest source (mpmc queue).
        for slot in &self.record_sender_slots {
            *slot.lock().unwrap() = Some(tx.clone());
        }
        match start_recording(
            Path::new("."),
            &self.channels,
            self.ingest_schema_bytes.clone(),
            rx,
        ) {
            Ok(handle) => {
                self.record_handle = Some(handle);
                self.status = "Recording started".to_string();
            }
            Err(e) => {
                // Remove senders since the recorder won't consume them.
                for slot in &self.record_sender_slots {
                    *slot.lock().unwrap() = None;
                }
                self.status = format!("Record failed: {e}");
            }
        }
    }
```

Replace `stop_recording` (lines 214-221):

```rust
    fn stop_recording(&mut self) {
        // Remove senders first so ingest stops queuing, then drop handle to signal recorder.
        for slot in &self.record_sender_slots {
            *slot.lock().unwrap() = None;
        }
        self.record_handle = None;
        self.status = "Recording stopped".to_string();
    }
```

- [ ] **Step 3: Update `main.rs` to collect senders from ZMQ + MQTT**

In `src/main.rs`, after the `mqtt_handles` / `(mqtt_topics, mqtt_topic_map)` block (lines 40-43), capture the MQTT record sender. Replace lines 40-43:

```rust
    let (mqtt_topics, mqtt_topic_map, mqtt_record_sender) = match mqtt_handles {
        Some(h) => (Some(h.discovered), Some(h.topic_map), Some(h.record_sender)),
        None => (None, None, None),
    };
```

Replace the ZMQ/demo block (lines 45-63) so it yields an optional ZMQ sender instead of the single slot:

```rust
    let (conn_state, zmq_record_sender, ingest_schema_bytes) = if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
        (None, None, vec![])
    } else {
        let config = IngestConfig {
            endpoint,
            proto_path: PathBuf::from(&schema_path),
        };
        match datavis::ingest::spawn_ingest(config, &channels, store.clone()) {
            Ok(handle) => {
                let schema_bytes = handle.schema_bytes.clone();
                (Some(handle.conn_state), Some(handle.record_sender), schema_bytes)
            }
            Err(e) => {
                eprintln!("ingest: failed to start ({e}); running without live data");
                (None, None, vec![])
            }
        }
    };

    // Recording targets every active ingest source (ZMQ and/or MQTT).
    let record_sender_slots: Vec<_> =
        [zmq_record_sender, mqtt_record_sender].into_iter().flatten().collect();
```

Replace the `record_sender_slot,` argument in the `DataVisApp::new(...)` call (line 75):

```rust
        record_sender_slots,
```

- [ ] **Step 4: Build**

Run: `cargo build 2>&1 | grep -E "error" | head`
Expected: no output (clean build).

- [ ] **Step 5: Add a test for multi-slot install/clear**

Add to the `tests` module in `src/app.rs` (which already has `#[test]` fns per line 688):

```rust
    #[test]
    fn record_sender_slots_install_and_clear() {
        use std::sync::{Arc, Mutex};
        let (tx, _rx) = crate::record::record_channel();
        let slots: Vec<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>> =
            vec![Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None))];
        // Install into all.
        for slot in &slots {
            *slot.lock().unwrap() = Some(tx.clone());
        }
        assert!(slots.iter().all(|s| s.lock().unwrap().is_some()));
        // Clear all.
        for slot in &slots {
            *slot.lock().unwrap() = None;
        }
        assert!(slots.iter().all(|s| s.lock().unwrap().is_none()));
    }
```

- [ ] **Step 6: Run app tests + full suite**

Run: `cargo test --lib 2>&1 | tail -6`
Expected: all pass, including `app::tests::record_sender_slots_install_and_clear`.

- [ ] **Step 7: Commit**

```bash
git add src/app.rs src/main.rs
git commit -m "feat: record to every active ingest source; MQTT-only runs can record"
```

---

## Manual Verification (after all tasks)

- [ ] Run against an MQTT broker: `cargo run --release -- --demo --mqtt-endpoint localhost:1883` (demo provides live panels; `--mqtt-endpoint` starts MQTT ingest so recording has a source). Publish a few topics, click **Rec**, then **Stop Rec**.
- [ ] Confirm a `recording_<secs>.mcap` file is written in the working dir and status shows "Recording started" (not "Recording unavailable").
- [ ] Inspect the file: `mcap info recording_*.mcap` — expect one channel per published topic, each with a protobuf schema; message counts match.
