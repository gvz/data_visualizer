# On-the-Fly MQTT Protobuf Schema Generation for MCAP Recording

**Status:** Design approved, ready for planning
**Date:** 2026-07-22

## Goal

Let MQTT-sourced scalar data be recorded to MCAP. Today the MCAP recorder only
accepts protobuf frames plus a static `FileDescriptorSet` schema, while the MQTT
ingest path produces bare scalars (string payloads parsed to float/int/bool/text)
written straight to the live store. This feature generates a protobuf schema for
each MQTT topic at runtime, encodes each sample into a protobuf message, and feeds
it to the existing recorder.

## Locked Decisions

- **Schema-gen mechanism:** hand-built `FileDescriptorProto` via `prost-types`
  (no `protoc`, no `.proto` text, no `protox` for this path).
- **Record scope:** every topic discovered on the MQTT `#` subscription, not only
  topics bound to a channel.
- **Message shape:** one generated message type per topic.
- **Sample type:** inferred from the topic's first payload and locked for the
  lifetime of the recorder.

## Non-Goals

- No structured/JSON payload parsing — payloads are treated as single scalars, as
  today.
- No re-inference or schema migration when a topic's payload type changes
  mid-recording (mismatched samples are skipped from the recording).
- No change to how bound channels feed the live store or to the ZMQ ingest path's
  semantics.

## Architecture and Data Flow

```
MQTT broker → mqtt run_loop ─┬─ (bound topic)      → store.write_*            (unchanged)
                             └─ (recording active) → DynamicProtoRegistry
                                                      ├ infer + lock type (first sample per topic)
                                                      ├ ensure schema (build descriptor once)
                                                      └ encode DynamicMessage
                                                           → RecordMsg::DynamicProto → recorder thread → MCAP
```

Recording is **additive and independent of store binding**: when recording is
active, every discovered topic is encoded and recorded; only bound topics
continue to feed the live store. The ZMQ ingest path is unchanged and may record
into the same MCAP file alongside MQTT — MCAP holds multiple schemas and channels
in one file.

## Components

### 1. `src/record/mqtt_schema.rs` — `DynamicProtoRegistry`

Owns runtime protobuf schema generation and message encoding for MQTT topics.
Lives in the MQTT ingest thread (single-threaded owner, no locking needed).

**State**

- `pool: prost_reflect::DescriptorPool` — accumulates one generated file per topic.
- `entries: HashMap<String, TopicEntry>` keyed by MQTT topic.
- `used_names: HashSet<String>` — for collision-free message names.

```rust
struct TopicEntry {
    sample_type: SampleType,
    descriptor: prost_reflect::MessageDescriptor,
    /// A self-contained FileDescriptorSet (this topic's one file), used as the
    /// MCAP protobuf schema payload.
    schema_bytes: Arc<[u8]>,
}
```

**API**

```rust
impl DynamicProtoRegistry {
    fn new() -> Self;

    /// Infer sample type from a payload string: parse i64 → Int; else f64 →
    /// Float; else bool-keyword → Bool; else Text. (Associated fn, no self.)
    fn infer_type(payload: &str) -> SampleType;

    /// One-shot for the ingest hot path. On a topic's first sight, infer + lock
    /// its type and build the generated message + schema; thereafter reuse them.
    /// Parse `payload` to the locked type and encode a DynamicMessage with fields
    /// `t_ns` and `value`. Returns the topic's schema (for the MCAP channel) and
    /// the encoded message bytes, or None on parse mismatch. Folding ensure +
    /// encode into one call avoids a borrow of an entry that overlaps the
    /// following mutable encode.
    fn record_frame(&mut self, topic: &str, ts_ns: i64, payload: &str)
        -> Option<(Arc<[u8]>, Vec<u8>)>;
}
```

`record_frame` internally uses a private `ensure(topic, sample_type)` helper to
lazily build and cache the `TopicEntry`; `ensure` is not part of the public API.

**Generated message layout** (hand-built `FileDescriptorProto`):

- package: `mqtt`
- file name: `mqtt/<message_name>.proto` (unique per topic)
- message name: `sanitize(topic)` — see below
- field 1: `t_ns`, `TYPE_INT64`, label optional
- field 2: `value`, label optional, type by locked `SampleType`:
  - Float → `TYPE_DOUBLE`
  - Int → `TYPE_INT64`
  - Bool → `TYPE_BOOL`
  - Text → `TYPE_STRING`

**`sanitize(topic) -> String`**: produce a valid, unique protobuf message
identifier.

- Split the topic on non-alphanumeric characters; upper-case the first letter of
  each segment; concatenate (CamelCase). Example: `home/sensors/temperature` →
  `HomeSensorsTemperature`.
- If the result is empty or starts with a digit, prefix `T`.
- If the name is already in `used_names`, append `_2`, `_3`, … until unique.

**Bool token sets** (used by both inference and encoding):

- true set: `"1" | "true" | "True" | "TRUE" | "on" | "ON" | "yes" | "YES"`
- false set: `"0" | "false" | "False" | "FALSE" | "off" | "OFF" | "no" | "NO"`

**Inference order** (`infer_type`): try `i64` → Int; else `f64` → Float; else a
**textual** bool token (the true/false sets minus `"1"` and `"0"`) → Bool; else
Text. Numeric `"1"`/`"0"` therefore infer Int, not Bool — a value alone cannot
distinguish a boolean from an integer, and i64-first is the deterministic choice.

**Encoding parse rules** (must match the locked type, else `None`):

- Float: `payload.parse::<f64>()`
- Int: `payload.parse::<i64>()`
- Bool: payload in the true set → `true`; in the false set → `false`; otherwise
  `None` (the encode-time bool sets include `"1"`/`"0"`, so a topic locked to Bool
  by a textual first sample still records later numeric `1`/`0`)
- Text: always `Some(payload.to_string())`

### 2. `RecordMsg` becomes an enum (`src/record/queue.rs`)

The recorder needs each MQTT topic's schema, so the current tuple type is
replaced:

```rust
pub enum RecordMsg {
    /// ZMQ path: encoded with the registry-derived shared schema.
    Proto { topic: Arc<str>, data: Vec<u8>, ts: i64 },
    /// MQTT path: carries its own generated schema.
    DynamicProto { topic: Arc<str>, schema: Arc<[u8]>, data: Vec<u8>, ts: i64 },
}
```

`record_channel()` and `QUEUE_CAP` are unchanged. Existing ZMQ producer code is
updated to build `RecordMsg::Proto`.

### 3. Recorder — lazy per-topic channels (`src/record/writer.rs`)

`McapRecorder` gains a `channel_ids: HashMap<String, u16>` filled lazily (it
already maps registry topics up front; this generalizes it).

- On `RecordMsg::Proto { topic, data, ts }`: look up the pre-registered channel
  for `topic` (registered at start from the registry, using the shared schema).
  Unknown topic → skip (existing behavior).
- On `RecordMsg::DynamicProto { topic, schema, data, ts }`: if `topic` has no
  channel yet, register an MCAP `Channel` whose `Schema` payload is `schema`
  (schema name = the generated message name, encoding `"protobuf"`), then write.
  Deduped by topic so each topic registers exactly once.

The recorder loop matches on the enum and dispatches to the appropriate write.

### 4. MQTT ingest wiring (`src/ingest/mqtt.rs`)

- `MqttHandles` gains
  `record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>`, mirroring the ZMQ
  `IngestHandle`.
- `run_loop` owns a thread-local `DynamicProtoRegistry` and reads the
  `record_sender` arc.
- On each `Publish`:
  1. Existing behavior: update the discovered-topics map; if the topic is bound,
     write the scalar to the store.
  2. New behavior: if `record_sender` currently holds `Some(sender)`, call
     `registry.record_frame(topic, ts, payload)`; on `Some((schema, bytes))`,
     `sender.try_send(RecordMsg::DynamicProto { topic, schema, data: bytes, ts })`.
     `ts` is `now_ns()` at receipt (also written into the `t_ns` field). A full
     queue drops the sample (existing `try_send` semantics).

### 5. App plumbing (`src/app.rs`, `src/main.rs`)

- The app holds a **list** of record-sender slots
  (`Vec<Arc<Mutex<Option<Sender<RecordMsg>>>>>`) collected from every active
  ingest source (ZMQ handle and/or MQTT handle).
- `start_recording` creates one record channel, then installs a cloned sender into
  **every** slot; `stop_recording` clears all slots. crossbeam is mpmc, so
  multiple producers feed the single recorder.
- Recording is available whenever **any** slot exists. The "Recording not
  available in demo mode" message is replaced with "Recording unavailable (no
  ingest source)" and only shown when the slot list is empty.

## Timestamps

MQTT payloads carry no timestamp. Each sample is stamped with `now_ns()` at
receipt, used both for the MCAP `log_time` and the message's `t_ns` field. This
matches the existing MQTT live-store behavior.

## Error Handling

- Payload fails to parse to the locked type → sample skipped from the recording
  (not written); the live store is unaffected.
- Descriptor build failure for a topic → log once, skip that topic for the rest of
  the session.
- Record queue full → `try_send` drops the sample (existing behavior).
- Sanitized name collision → numeric suffix guarantees uniqueness.

## Testing

**`mqtt_schema` unit tests**
- `infer_type` truth table across representative payloads (`"1"`, `"3.14"`,
  `"true"`, `"off"`, `"hello"`).
- `record_frame` builds a valid descriptor for each of the four sample types
  (field numbers, field types, message name) on first sight.
- `record_frame` round-trips: decode the returned bytes with the topic's
  descriptor, asserting `t_ns` and `value` for each type.
- `record_frame` returns `None` when a payload does not match the locked type
  (type locked from an earlier first sample).
- Name collision: two topics sanitizing to the same base name get distinct message
  names.

**`writer` tests**
- `RecordMsg::DynamicProto` for a new topic lazily registers a channel and schema;
  a second topic registers a second schema; both are readable back from the MCAP
  file with correct topics and schema payloads.
- `RecordMsg::Proto` path continues to pass existing round-trip tests (updated to
  the enum constructor).

**Queue tests**
- Updated to the enum variants; full-queue drop behavior preserved.

**App test (light)**
- Installing a sender into multiple slots and clearing them on stop.

## Trade-offs (Accepted)

- Unbounded schema/topic count and per-publish encode CPU are inherent to
  recording all topics.
- Sample type is locked on the first payload; later type changes are skipped from
  the recording (still shown live for bound channels).
