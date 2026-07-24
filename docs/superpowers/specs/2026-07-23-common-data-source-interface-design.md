# Common Data-Source Interface — Design

**Date:** 2026-07-23
**Status:** Draft for review

## Goal

One well-structured interface for live data inputs, implemented by the existing
ZMQ and MQTT ingest paths and cheap to extend with future MQTT-shaped sources
(WebSocket, serial, another pub/sub). Unify the two existing paths *and* provide
a real extension point so a third source is a small, obvious addition.

## Motivation

Today the two ingest paths are fully separate:

| | ZMQ (`spawn_ingest`) | MQTT (`spawn_mqtt_ingest`) |
|---|---|---|
| handle | `IngestHandle { conn_state, record_sender, schema_bytes }` | `MqttHandles { discovered, topic_map, record_sender }` |
| topics | static, from config + proto schema | dynamic, discovered live via `#` |
| decode | proto schema → `decode_batch` (many channels/msg) | parse string → one scalar |
| record | `RecordMsg::Proto` (static schema) | `RecordMsg::DynamicProto` (schema built on the fly) |
| conn state | yes (`CONNECTING`/`LIVE`/`TIMEOUT`) | **none** |

`main.rs` wires each path by hand and hands `DataVisApp::new` six separate
ingest-derived parameters (`conn_state`, `record_sender_slots`,
`ingest_schema_bytes`, `mqtt_topics`, `mqtt_topic_map`, …). Adding a source means
touching all of that by copy-paste.

Confirmed direction: future sources are **MQTT-shaped** — push, discover topics
live, scalar/string payloads, record schema generated at runtime. ZMQ is the
outlier (batched binary proto, static schema). The interface is therefore
centred on the MQTT shape; ZMQ conforms to the common *lifecycle* but keeps its
own batched decode loop rather than being forced into a scalar mold.

## Architecture (Approach A)

Two layers.

### Layer 1 — common lifecycle: the `DataSource` trait

Every source is constructed with its config + the channel registry, then spawned
against the shared store. It returns one uniform handle.

```rust
// src/ingest/source.rs (new)

pub trait DataSource: Send {
    /// Human-facing name for UI and logs, e.g. "zmq", "mqtt".
    fn name(&self) -> &str;

    /// Consume config, spawn the worker thread(s), return the handle.
    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle;
}

/// Uniform handle for one running source. Optional capabilities are `None`
/// for sources that lack them.
pub struct SourceHandle {
    pub name: String,
    /// Connection status. Every source reports it (MQTT gains this).
    pub conn_state: Arc<AtomicU8>,
    /// Recorder hookup: the app installs a sender here while recording.
    pub record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    /// Live topic discovery + drag-to-bind map (MQTT-shaped sources).
    pub discovery: Option<Discovery>,
    /// Static record schema for the MCAP header (ZMQ only; MQTT embeds
    /// per-frame schemas in `RecordMsg::DynamicProto`).
    pub schema_bytes: Option<Vec<u8>>,
}

/// Capability bundle for sources that discover topics at runtime.
pub struct Discovery {
    /// All received topics with their last payload, for the sidebar picker.
    pub discovered: Arc<Mutex<BTreeMap<String, String>>>,
    /// topic → (id, type); extended when a topic is dropped onto a panel.
    pub topic_map: Arc<MqttTopicMap>,
}
```

- `conn_state` and `record_sender` are created by each source's `spawn` and
  returned in the handle (they are *outputs* the app reads/writes).
- `store` is the sole shared *input*.
- Capabilities are `Option` — a source advertises only what it has. No downcast,
  no enum matching at the call site.

`main.rs` builds a `Vec<Box<dyn DataSource>>`, spawns each into a
`Vec<SourceHandle>`, and passes that one vector to the app.

### Layer 2 — reusable scalar-ingest helper for the MQTT family

The MQTT loop body — update `discovered`, `record_publish` (dynamic schema),
`topic_map` lookup, parse scalar, `store.write_numeric` / `write_text` — is the
part every future MQTT-shaped source repeats. Factor it into a helper:

```rust
// src/ingest/scalar.rs (new) — extracted from mqtt.rs, no behavior change

/// Shared per-message handling for discover + record + route-to-store sources.
/// A transport supplies `(topic, payload, ts)`; this does discovery, dynamic
/// recording, routing and typed store writes.
pub struct ScalarIngest {
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    topic_map: Arc<MqttTopicMap>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    proto_registry: DynamicProtoRegistry,
}

impl ScalarIngest {
    pub fn on_message(&mut self, topic: &str, payload: &str, ts: i64);
}
```

A future WebSocket/serial source implements `DataSource`, owns its connect/read
loop, and calls `ScalarIngest::on_message` per message — that's the whole "third
source is small" win. ZMQ does **not** use `ScalarIngest`; it implements
`DataSource` with its own batched-proto loop and `decode_batch`.

## Components changed

- **New `src/ingest/source.rs`** — `DataSource` trait, `SourceHandle`, `Discovery`.
- **New `src/ingest/scalar.rs`** — `ScalarIngest`, extracted verbatim from the
  MQTT loop body (discovery update, `record_publish`, topic_map route, typed
  writes).
- **`src/ingest/mqtt.rs`** — `MqttSource` struct implementing `DataSource`; loop
  becomes connect + `ScalarIngest::on_message`; now also drives `conn_state`
  (set `LIVE` on publish, `CONNECTING` on connect, `TIMEOUT` after idle — mirror
  ZMQ's timeout logic). `MqttHandles`/`spawn_mqtt_ingest` removed.
- **`src/ingest/mod.rs`** — `ZmqSource` struct implementing `DataSource`,
  wrapping today's `spawn_ingest` body (returns `schema_bytes` via handle, no
  `discovery`). `IngestHandle`/`spawn_ingest` folded into it. Re-export the trait
  types.
- **`src/main.rs`** — build `Vec<Box<dyn DataSource>>` from CLI args/config,
  spawn all, collect `Vec<SourceHandle>`, pass to app.
- **`src/app.rs`** — `DataVisApp::new` takes `Vec<SourceHandle>` instead of the
  six separate ingest params, and derives its internal fields:
  - `record_sender_slots` = every handle's `record_sender`.
  - `ingest_schema_bytes` = handles' `schema_bytes` (today exactly one source
    provides one; concatenation is out of scope — take the sources that have
    one, current behavior preserved for the single-ZMQ case).
  - `conn_state` shown in the status UI = first handle that reports one (or,
    later, aggregate; single-source behavior preserved now).
  - `mqtt_topics` / `mqtt_topic_map` = the `discovery` capability of whichever
    handle has one.

## Data flow (unchanged semantics)

```
source.spawn(store) → thread:
  ZMQ:  recv_multipart → decode_batch(bindings, store) → RecordMsg::Proto
  MQTT: recv publish  → ScalarIngest::on_message
            → discovered.insert
            → record_publish → RecordMsg::DynamicProto
            → topic_map lookup → store.write_numeric/write_text
both: conn_state ← CONNECTING / LIVE / TIMEOUT
      record_sender ← installed by app while recording
```

The MCAP recorder and `RecordMsg` enum are unchanged. Store writes are unchanged.
This is a structural refactor: no new wire behavior, no new record format.

## Error handling

Unchanged from today: each source's loop logs transport errors and reconnects
with backoff (ZMQ) or retries (MQTT). A parse mismatch or full record queue
silently drops the sample, as now.

## Testing

- **Preserve** all existing ingest tests (router, decode, mqtt parse/record,
  spawn). They should pass unchanged or with only type/name updates.
- **`ScalarIngest`**: unit test that `on_message` updates `discovered`, queues a
  decodable `DynamicProto` when a sender is installed, and writes the correct
  typed value to a stub store for Float/Int/Bool/Text. (Moves/extends the
  existing `record_publish` tests.)
- **`DataSource`/handle**: a test that a spawned `MqttSource` returns a handle
  with `discovery.is_some()`, `schema_bytes.is_none()`, and a fresh
  `conn_state`; and a `ZmqSource` returns `discovery.is_none()`,
  `schema_bytes.is_some()`.
- **App wiring**: keep `record_sender_slots_install_and_clear`; adapt its setup
  to build slots from `Vec<SourceHandle>`.

## Out of scope

- Poll/request-based sources (HTTP, Modbus). The trait doesn't preclude them but
  no poll-interval machinery is added now.
- Aggregating multiple `conn_state`s or multiple static schemas into the UI —
  single-source behavior is preserved; multi-source aggregation is a later step.
- Any change to the MCAP record format or replay path.

## Global Constraints

- No Co-Authored-By / self-attribution in commits or PRs.
- Nix cache policy unchanged (`cache.numtide.com` forbidden).
- Do not commit or push unless explicitly asked.
</content>
