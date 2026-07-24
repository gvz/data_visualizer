# Common Data-Source Interface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify the ZMQ and MQTT live-input paths behind one `DataSource` trait producing a uniform `SourceHandle`, and extract the MQTT loop body into a reusable `ScalarIngest` helper so future MQTT-shaped sources are small additions.

**Architecture:** A `DataSource` trait (`spawn(store) -> SourceHandle`) implemented by `ZmqSource` and `MqttSource`. `SourceHandle` carries the common outputs (`conn_state`, `record_sender`) plus `Option` capabilities (`discovery`, `schema_bytes`). MQTT-family per-message handling (discover + dynamic-record + route-to-store) lives in `ScalarIngest`. `main.rs` builds `Vec<Box<dyn DataSource>>`, spawns them into `Vec<SourceHandle>`, and hands that one vector to `DataVisApp::new`, which derives its internal ingest fields from it.

**Tech Stack:** Rust, rumqttc (MQTT), zmq, prost-reflect, crossbeam-channel, egui/eframe.

## Global Constraints

- No Co-Authored-By / self-attribution in any commit or PR.
- Nix cache policy unchanged: `cache.numtide.com` forbidden.
- Do NOT commit or push unless the human explicitly asks.
- Pure structural refactor: no change to the wire protocols, `RecordMsg` enum, MCAP record format, replay path, or store-write semantics.
- Every existing ingest test must still pass (adapted only for renamed/moved types).
- MQTT `conn_state` scope: `CONNECTING` initially, `LIVE` on `ConnAck`, back to `CONNECTING` on connection error. The `TIMEOUT` idle-state (ZMQ-only, via `recv_timeout`) is **not** implemented for MQTT in this plan — rumqttc's blocking `Connection::iter()` has no cheap 5 s idle poll. Documented deviation from the design's "mirror ZMQ timeout logic" line.

---

## File Structure

- **Create `src/ingest/source.rs`** — `DataSource` trait, `SourceHandle`, `Discovery`.
- **Create `src/ingest/scalar.rs`** — `ScalarIngest` (extracted MQTT loop body).
- **Modify `src/ingest/mqtt.rs`** — `MqttSource: DataSource`; `run_loop` delegates to `ScalarIngest` and drives `conn_state`; `spawn_mqtt_ingest` becomes a thin remap wrapper (deleted in Task 5).
- **Modify `src/ingest/mod.rs`** — declare new modules, re-export trait types; `ZmqSource: DataSource`; `spawn_ingest` becomes a thin remap wrapper (deleted in Task 5).
- **Modify `src/app.rs`** — `DataVisApp::new` takes `Vec<SourceHandle>`; derive internal fields.
- **Modify `src/main.rs`** — build sources, spawn, pass `Vec<SourceHandle>`.

---

## Task 1: `DataSource` trait, `SourceHandle`, `Discovery`

**Files:**
- Create: `src/ingest/source.rs`
- Modify: `src/ingest/mod.rs` (add `pub mod source;` and re-exports)

**Interfaces:**
- Produces:
  - `trait DataSource: Send { fn name(&self) -> &str; fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle; }`
  - `struct SourceHandle { name: String, conn_state: Arc<AtomicU8>, record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>, discovery: Option<Discovery>, schema_bytes: Option<Vec<u8>> }`
  - `struct Discovery { discovered: Arc<Mutex<BTreeMap<String, String>>>, topic_map: Arc<MqttTopicMap> }`

- [ ] **Step 1: Write the failing test**

Add to `src/ingest/source.rs` (create the file with just this test + `use super::*;` scaffolding will fail to compile until Step 3 adds the types):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, RwLock};

    #[test]
    fn handle_holds_optional_capabilities() {
        let discovery = Discovery {
            discovered: Arc::new(Mutex::new(BTreeMap::new())),
            topic_map: Arc::new(RwLock::new(std::collections::HashMap::new())),
        };
        let h = SourceHandle {
            name: "mqtt".to_string(),
            conn_state: Arc::new(AtomicU8::new(0)),
            record_sender: Arc::new(Mutex::new(None)),
            discovery: Some(discovery),
            schema_bytes: None,
        };
        assert_eq!(h.name, "mqtt");
        assert!(h.discovery.is_some());
        assert!(h.schema_bytes.is_none());
        assert_eq!(h.conn_state.load(Ordering::Relaxed), 0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::source`
Expected: FAIL — compile error, `Discovery` / `SourceHandle` not found.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/ingest/source.rs`:

```rust
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use crate::dynamic_channel::MqttTopicMap;
use crate::record::RecordMsg;
use crate::store::ChannelStore;

/// A live data input. Constructed with its own config + the channel registry,
/// then spawned against the shared store; returns one uniform handle.
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
    /// Connection status (`CONNECTING`/`LIVE`/`TIMEOUT` from `ingest`).
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

In `src/ingest/mod.rs`, add alongside the other `pub mod` lines:

```rust
pub mod source;
```

and alongside the existing `pub use`:

```rust
pub use source::{DataSource, Discovery, SourceHandle};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ingest::source`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/source.rs src/ingest/mod.rs
git commit -m "feat: add DataSource trait and uniform SourceHandle"
```

---

## Task 2: `ScalarIngest` helper (extract MQTT loop body)

**Files:**
- Create: `src/ingest/scalar.rs`
- Modify: `src/ingest/mqtt.rs` (`run_loop` delegates to `ScalarIngest`; move `record_publish` and its tests out)
- Modify: `src/ingest/mod.rs` (add `pub mod scalar;`)

**Interfaces:**
- Consumes: `MqttTopicMap`, `DynamicProtoRegistry`, `ChannelStore`, `RecordMsg` (unchanged).
- Produces:
  - `struct ScalarIngest { /* private */ }`
  - `impl ScalarIngest { pub fn new(discovered: Arc<Mutex<BTreeMap<String,String>>>, topic_map: Arc<MqttTopicMap>, store: Arc<dyn ChannelStore>, record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>) -> Self; pub fn on_message(&mut self, topic: &str, payload: &str, ts: i64); }`

- [ ] **Step 1: Write the failing test**

Create `src/ingest/scalar.rs` with the impl-less test module (compile-fails until Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::record::{record_channel, RecordMsg};
    use crate::store::LiveStore;
    use crate::types::{ChannelId, SampleType};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex, RwLock};

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."sensor/temp"]
mqtt_topic = "home/temp"
type = "float"
"#,
        )
        .unwrap()
    }

    #[test]
    fn on_message_discovers_records_and_routes() {
        let reg = registry();
        let store: Arc<dyn crate::store::ChannelStore> =
            Arc::new(LiveStore::from_registry(&reg));

        // Build the topic_map the way spawn does.
        let mut initial: HashMap<String, (ChannelId, SampleType)> = HashMap::new();
        for id in reg.iter_ids() {
            if let Some(t) = &reg.config(id).mqtt_topic {
                initial.insert(t.clone(), (id, reg.meta(id).sample_type));
            }
        }
        let topic_map = Arc::new(RwLock::new(initial));
        let discovered = Arc::new(Mutex::new(BTreeMap::new()));
        let (tx, rx) = record_channel();
        let record_sender = Arc::new(Mutex::new(Some(tx)));

        let mut ingest = ScalarIngest::new(
            discovered.clone(),
            topic_map,
            store,
            record_sender,
        );
        ingest.on_message("home/temp", "21.5", 1_000);

        // discovered updated
        assert_eq!(discovered.lock().unwrap().get("home/temp").map(String::as_str), Some("21.5"));
        // a dynamic-proto frame was queued
        match rx.try_recv().unwrap() {
            RecordMsg::DynamicProto { topic, ts, .. } => {
                assert_eq!(topic.as_ref(), "home/temp");
                assert_eq!(ts, 1_000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn on_message_without_sender_still_discovers() {
        let reg = registry();
        let store: Arc<dyn crate::store::ChannelStore> =
            Arc::new(LiveStore::from_registry(&reg));
        let topic_map = Arc::new(RwLock::new(HashMap::new()));
        let discovered = Arc::new(Mutex::new(BTreeMap::new()));
        let record_sender = Arc::new(Mutex::new(None));

        let mut ingest = ScalarIngest::new(discovered.clone(), topic_map, store, record_sender);
        ingest.on_message("a/b", "hello", 0);
        assert_eq!(discovered.lock().unwrap().get("a/b").map(String::as_str), Some("hello"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::scalar`
Expected: FAIL — `ScalarIngest` not found.

- [ ] **Step 3: Write minimal implementation**

Prepend to `src/ingest/scalar.rs` (this is the MQTT loop body moved verbatim, plus the `record_publish` helper moved from `mqtt.rs`):

```rust
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use crate::dynamic_channel::MqttTopicMap;
use crate::record::mqtt_schema::DynamicProtoRegistry;
use crate::record::RecordMsg;
use crate::store::ChannelStore;
use crate::types::{NumericVal, SampleType};

/// Per-message handling shared by discover + record + route-to-store sources
/// (the MQTT family). A transport supplies `(topic, payload, ts)`; this does
/// discovery, dynamic-schema recording, topic routing and typed store writes.
pub struct ScalarIngest {
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    topic_map: Arc<MqttTopicMap>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    proto_registry: DynamicProtoRegistry,
}

impl ScalarIngest {
    pub fn new(
        discovered: Arc<Mutex<BTreeMap<String, String>>>,
        topic_map: Arc<MqttTopicMap>,
        store: Arc<dyn ChannelStore>,
        record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    ) -> Self {
        Self {
            discovered,
            topic_map,
            store,
            record_sender,
            proto_registry: DynamicProtoRegistry::new(),
        }
    }

    /// Handle one received message: update discovery, queue a record frame if
    /// recording, then route to the store if a channel is bound to `topic`.
    pub fn on_message(&mut self, topic: &str, payload: &str, ts: i64) {
        self.discovered.lock().unwrap().insert(topic.to_string(), payload.to_string());

        if let Ok(guard) = self.record_sender.try_lock() {
            record_publish(&mut self.proto_registry, &guard, topic, payload, ts);
        }

        let Some((id, sample_type)) = self.topic_map.read().unwrap().get(topic).copied() else {
            return;
        };
        match sample_type {
            SampleType::Float => {
                if let Ok(v) = payload.parse::<f64>() {
                    self.store.write_numeric(id, ts, NumericVal::Float(v));
                }
            }
            SampleType::Int => {
                if let Ok(v) = payload.parse::<i64>() {
                    self.store.write_numeric(id, ts, NumericVal::Int(v));
                }
            }
            SampleType::Bool => {
                let v = matches!(
                    payload,
                    "1" | "true" | "True" | "TRUE" | "on" | "ON" | "yes" | "YES"
                );
                self.store.write_numeric(id, ts, NumericVal::Bool(v));
            }
            SampleType::Text => {
                self.store.write_text(id, ts, payload.to_string());
            }
        }
    }
}

/// Encode one publish and queue it for the recorder, if recording is active.
/// Generates the topic's schema on first sight. A parse mismatch or a full
/// queue silently drops the sample.
fn record_publish(
    reg: &mut DynamicProtoRegistry,
    sender: &Option<Sender<RecordMsg>>,
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

Add to `src/ingest/mod.rs` alongside the other `pub mod` lines:

```rust
pub mod scalar;
```

Now rewrite `mqtt.rs::run_loop` to delegate. Replace its body's per-publish handling with a `ScalarIngest`. Replace the existing `run_loop` (lines ~87–151) with:

```rust
fn run_loop(
    opts: MqttOptions,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
) {
    let (client, mut connection) = Client::new(opts, 64);
    let mut ingest =
        crate::ingest::scalar::ScalarIngest::new(discovered, topic_map, store, record_sender);

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                if let Err(e) = client.subscribe("#", QoS::AtMostOnce) {
                    eprintln!("mqtt: subscribe # failed: {e}");
                }
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let payload_str = std::str::from_utf8(&p.payload)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| format!("({} bytes)", p.payload.len()));
                let ts = crate::types::now_ns();
                ingest.on_message(&p.topic, payload_str.as_str(), ts);
            }
            Err(e) => {
                eprintln!("mqtt: {e}");
                std::thread::sleep(Duration::from_secs(2));
            }
            _ => {}
        }
    }
}
```

Delete the now-unused `record_publish` fn from `mqtt.rs` and its two tests
(`record_publish_sends_decodable_dynamic_proto`, `record_publish_noop_when_no_sender`)
— they are superseded by the `ScalarIngest` tests. Remove the now-unused imports
from `mqtt.rs` (`DynamicProtoRegistry`, `NumericVal`; keep `HashMap`, `SampleType`,
`ChannelId` — still used by `spawn_mqtt_ingest`'s `topic_map` build).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib ingest`
Expected: PASS — `ingest::scalar` tests green; the remaining `mqtt.rs` tests
(`parse_*`, `spawn_returns_discovered_set`, `topic_map_built_from_mqtt_channels`) still green.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/scalar.rs src/ingest/mqtt.rs src/ingest/mod.rs
git commit -m "refactor: extract MQTT loop body into reusable ScalarIngest"
```

---

## Task 3: `MqttSource` implements `DataSource` (+ conn_state)

**Files:**
- Modify: `src/ingest/mqtt.rs` (add `MqttSource`, drive `conn_state`, make `spawn_mqtt_ingest` a remap wrapper)

**Interfaces:**
- Consumes: `DataSource`, `SourceHandle`, `Discovery` (Task 1); `ScalarIngest` (Task 2); `CONNECTING`, `LIVE` (from `crate::ingest`).
- Produces:
  - `struct MqttSource { config: MqttConfig, topic_map: Arc<MqttTopicMap> }`
  - `impl MqttSource { pub fn new(config: MqttConfig, registry: &ChannelRegistry) -> Self }`
  - `impl DataSource for MqttSource`

- [ ] **Step 1: Write the failing test**

Add to the `mqtt.rs` tests module:

```rust
#[test]
fn mqtt_source_handle_has_discovery_no_schema() {
    use crate::ingest::{DataSource, CONNECTING};
    use std::sync::atomic::Ordering;

    let registry = crate::config::ChannelRegistry::from_toml_str(
        r#"
[channels."x"]
mqtt_topic = "home/x"
type = "float"
"#,
    )
    .unwrap();
    let store: Arc<dyn ChannelStore> =
        Arc::new(crate::store::LiveStore::from_registry(&registry));
    let src = MqttSource::new(
        MqttConfig { broker_url: "localhost:19997".into(), client_id: "test".into() },
        &registry,
    );
    let handle = Box::new(src).spawn(store);
    assert_eq!(handle.name, "mqtt");
    assert!(handle.discovery.is_some());
    assert!(handle.schema_bytes.is_none());
    // No broker at that port → stays CONNECTING.
    assert_eq!(handle.conn_state.load(Ordering::Relaxed), CONNECTING);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::mqtt::tests::mqtt_source_handle_has_discovery_no_schema`
Expected: FAIL — `MqttSource` not found.

- [ ] **Step 3: Write minimal implementation**

In `mqtt.rs`, add imports:

```rust
use std::sync::atomic::AtomicU8;
use crate::ingest::source::{DataSource, Discovery, SourceHandle};
use crate::ingest::{CONNECTING, LIVE};
```

Add the struct + impl (near `spawn_mqtt_ingest`):

```rust
pub struct MqttSource {
    config: MqttConfig,
    topic_map: Arc<MqttTopicMap>,
}

impl MqttSource {
    pub fn new(config: MqttConfig, registry: &ChannelRegistry) -> Self {
        let mut initial: HashMap<String, (ChannelId, SampleType)> = HashMap::new();
        for id in registry.iter_ids() {
            if let Some(mqtt_topic) = &registry.config(id).mqtt_topic {
                initial.insert(mqtt_topic.clone(), (id, registry.meta(id).sample_type));
            }
        }
        Self { config, topic_map: Arc::new(RwLock::new(initial)) }
    }
}

impl DataSource for MqttSource {
    fn name(&self) -> &str {
        "mqtt"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let discovered: Arc<Mutex<BTreeMap<String, String>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let (host, port) = parse_broker_url(&self.config.broker_url);
        let mut opts = MqttOptions::new(self.config.client_id.clone(), host, port);
        opts.set_keep_alive(Duration::from_secs(30));

        let record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>> =
            Arc::new(Mutex::new(None));
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));

        let disc = discovered.clone();
        let map = self.topic_map.clone();
        let rec = record_sender.clone();
        let state = conn_state.clone();
        std::thread::spawn(move || {
            run_loop(opts, map, disc, store, rec, state);
        });

        SourceHandle {
            name: "mqtt".to_string(),
            conn_state,
            record_sender,
            discovery: Some(Discovery { discovered, topic_map: self.topic_map }),
            schema_bytes: None,
        }
    }
}
```

Extend `run_loop` to take and drive `conn_state`:

```rust
fn run_loop(
    opts: MqttOptions,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
    conn_state: Arc<AtomicU8>,
) {
    use std::sync::atomic::Ordering;
    let (client, mut connection) = Client::new(opts, 64);
    let mut ingest =
        crate::ingest::scalar::ScalarIngest::new(discovered, topic_map, store, record_sender);

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                conn_state.store(LIVE, Ordering::Relaxed);
                if let Err(e) = client.subscribe("#", QoS::AtMostOnce) {
                    eprintln!("mqtt: subscribe # failed: {e}");
                }
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let payload_str = std::str::from_utf8(&p.payload)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| format!("({} bytes)", p.payload.len()));
                let ts = crate::types::now_ns();
                ingest.on_message(&p.topic, payload_str.as_str(), ts);
            }
            Err(e) => {
                conn_state.store(CONNECTING, Ordering::Relaxed);
                eprintln!("mqtt: {e}");
                std::thread::sleep(Duration::from_secs(2));
            }
            _ => {}
        }
    }
}
```

Rewrite `spawn_mqtt_ingest` as a thin remap wrapper over `MqttSource` (keeps the
existing `MqttHandles` API and its tests working; removed in Task 5):

```rust
pub fn spawn_mqtt_ingest(
    config: MqttConfig,
    registry: &ChannelRegistry,
    store: Arc<dyn ChannelStore>,
) -> MqttHandles {
    let src = MqttSource::new(config, registry);
    let handle = Box::new(src).spawn(store);
    let discovery = handle.discovery.expect("MqttSource always provides discovery");
    MqttHandles {
        discovered: discovery.discovered,
        topic_map: discovery.topic_map,
        record_sender: handle.record_sender,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib ingest::mqtt`
Expected: PASS — new `mqtt_source_handle_*` test plus all existing mqtt tests.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/mqtt.rs
git commit -m "feat: MqttSource implements DataSource and reports conn_state"
```

---

## Task 4: `ZmqSource` implements `DataSource`

**Files:**
- Modify: `src/ingest/mod.rs` (add `ZmqSource`, make `spawn_ingest` a remap wrapper)

**Interfaces:**
- Consumes: `DataSource`, `SourceHandle` (Task 1); existing `loader::ProtoSchema`, `router::TopicRouter`, `thread::run_loop`.
- Produces:
  - `struct ZmqSource { config: IngestConfig, router: TopicRouter, schema_bytes: Vec<u8> }`
  - `impl ZmqSource { pub fn build(config: IngestConfig, registry: &ChannelRegistry) -> anyhow::Result<Self> }`
  - `impl DataSource for ZmqSource`

- [ ] **Step 1: Write the failing test**

Add to the `mod.rs` tests module:

```rust
#[test]
fn zmq_source_handle_has_schema_no_discovery() {
    use std::io::Write;
    use crate::ingest::source::DataSource;

    let dir = tempfile::tempdir().unwrap();
    let proto_path = dir.path().join("test.proto");
    let mut f = std::fs::File::create(&proto_path).unwrap();
    write!(f, "syntax = \"proto3\";\nmessage M {{ int64 t = 1; float v = 2; }}\n").unwrap();
    let registry = crate::config::ChannelRegistry::from_toml_str(
        r#"
[channels."x"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#,
    )
    .unwrap();
    let store: Arc<dyn crate::store::ChannelStore> =
        Arc::new(crate::store::LiveStore::from_registry(&registry));
    let src = ZmqSource::build(
        IngestConfig { endpoint: "tcp://localhost:59998".into(), proto_path },
        &registry,
    )
    .unwrap();
    let handle = Box::new(src).spawn(store);
    assert_eq!(handle.name, "zmq");
    assert!(handle.discovery.is_none());
    assert!(handle.schema_bytes.as_ref().is_some_and(|b| !b.is_empty()));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::tests::zmq_source_handle_has_schema_no_discovery`
Expected: FAIL — `ZmqSource` not found.

- [ ] **Step 3: Write minimal implementation**

In `src/ingest/mod.rs`, add imports at the top:

```rust
use std::sync::atomic::AtomicU8;
use crate::store::ChannelStore;
```

(`ChannelStore` may already be imported — keep one.) Add the struct + impl:

```rust
pub struct ZmqSource {
    endpoint: String,
    router: router::TopicRouter,
    schema_bytes: Vec<u8>,
}

impl ZmqSource {
    pub fn build(config: IngestConfig, registry: &ChannelRegistry) -> anyhow::Result<Self> {
        let schema = loader::ProtoSchema::from_path(&config.proto_path)?;
        let schema_bytes = schema.schema_bytes().to_vec();
        let router = router::TopicRouter::build(registry, &schema);
        Ok(Self { endpoint: config.endpoint, router, schema_bytes })
    }
}

impl source::DataSource for ZmqSource {
    fn name(&self) -> &str {
        "zmq"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> source::SourceHandle {
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));
        let record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>> =
            Arc::new(Mutex::new(None));

        let state_clone = conn_state.clone();
        let sender_clone = record_sender.clone();
        let endpoint = self.endpoint.clone();
        let router = self.router;
        std::thread::spawn(move || {
            thread::run_loop(endpoint, router, store, state_clone, sender_clone);
        });

        source::SourceHandle {
            name: "zmq".to_string(),
            conn_state,
            record_sender,
            discovery: None,
            schema_bytes: Some(self.schema_bytes),
        }
    }
}
```

Rewrite `spawn_ingest` as a thin remap wrapper over `ZmqSource` (keeps the
existing `IngestHandle` API and its tests working; removed in Task 5):

```rust
pub fn spawn_ingest(
    config: IngestConfig,
    registry: &ChannelRegistry,
    store: Arc<dyn ChannelStore>,
) -> anyhow::Result<IngestHandle> {
    let src = ZmqSource::build(config, registry)?;
    let schema_bytes = src.schema_bytes.clone();
    let handle = Box::new(src).spawn(store);
    Ok(IngestHandle {
        conn_state: handle.conn_state,
        record_sender: handle.record_sender,
        schema_bytes,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib ingest`
Expected: PASS — new `zmq_source_*` test plus all existing ingest tests
(`spawn_ingest_missing_schema_returns_err`, `schema_bytes_via_spawn_ingest_are_non_empty`, etc.).

- [ ] **Step 5: Commit**

```bash
git add src/ingest/mod.rs
git commit -m "feat: ZmqSource implements DataSource"
```

---

## Task 5: Wire `DataVisApp` and `main.rs` to `Vec<SourceHandle>`

**Files:**
- Modify: `src/app.rs` (constructor signature + field derivation; adapt `record_sender_slots_install_and_clear` test)
- Modify: `src/main.rs` (build sources, spawn, pass one vector)
- Modify: `src/ingest/mqtt.rs` — delete `spawn_mqtt_ingest`, `MqttHandles`, and their tests (`spawn_returns_discovered_set`, `topic_map_built_from_mqtt_channels` if they reference removed items — keep the topic-map assertion test by pointing it at `MqttSource::new` output instead).
- Modify: `src/ingest/mod.rs` — delete `spawn_ingest`, `IngestHandle`; update its tests (`spawn_ingest_missing_schema_returns_err` → `ZmqSource::build`, `schema_bytes_via_spawn_ingest_are_non_empty` → `ZmqSource::build(...).unwrap().schema_bytes`).

**Interfaces:**
- Consumes: `SourceHandle`, `DataSource`, `MqttSource`, `ZmqSource`.
- Produces: `DataVisApp::new(store, channels, registry, workspace, layout_path, sources: Vec<SourceHandle>, live_view_ns, live_history_s, default_window_s)`.

- [ ] **Step 1: Write the failing test**

Replace the existing `record_sender_slots_install_and_clear` test body in `app.rs` so it builds slots from `Vec<SourceHandle>` (this drives the new derivation helper). Add near it:

```rust
#[test]
fn derives_ingest_fields_from_handles() {
    use crate::ingest::{Discovery, SourceHandle};
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicU8;
    use std::sync::{Arc, Mutex, RwLock};

    let mqtt = SourceHandle {
        name: "mqtt".into(),
        conn_state: Arc::new(AtomicU8::new(0)),
        record_sender: Arc::new(Mutex::new(None)),
        discovery: Some(Discovery {
            discovered: Arc::new(Mutex::new(BTreeMap::new())),
            topic_map: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }),
        schema_bytes: None,
    };
    let zmq = SourceHandle {
        name: "zmq".into(),
        conn_state: Arc::new(AtomicU8::new(1)),
        record_sender: Arc::new(Mutex::new(None)),
        discovery: None,
        schema_bytes: Some(vec![1, 2, 3]),
    };
    let d = DerivedIngest::from_handles(vec![mqtt, zmq]);
    assert_eq!(d.record_sender_slots.len(), 2);
    assert_eq!(d.ingest_schema_bytes, vec![1, 2, 3]);
    assert!(d.mqtt_topics.is_some());
    assert!(d.mqtt_topic_map.is_some());
    assert!(d.conn_state.is_some());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib app::tests::derives_ingest_fields_from_handles`
Expected: FAIL — `DerivedIngest` not found.

- [ ] **Step 3: Write minimal implementation**

Add a derivation helper to `app.rs` (module-level, above `impl DataVisApp`):

```rust
/// Ingest-derived fields collapsed from the running sources. `conn_state`,
/// `mqtt_topics`, `mqtt_topic_map` and `ingest_schema_bytes` take the first
/// source that provides each — today at most one source provides any given one.
pub(crate) struct DerivedIngest {
    pub conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
    pub record_sender_slots:
        Vec<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
    pub ingest_schema_bytes: Vec<u8>,
    pub mqtt_topics: Option<Arc<Mutex<BTreeMap<String, String>>>>,
    pub mqtt_topic_map: Option<Arc<crate::dynamic_channel::MqttTopicMap>>,
}

impl DerivedIngest {
    pub(crate) fn from_handles(handles: Vec<crate::ingest::SourceHandle>) -> Self {
        let mut conn_state = None;
        let mut record_sender_slots = Vec::new();
        let mut ingest_schema_bytes = Vec::new();
        let mut mqtt_topics = None;
        let mut mqtt_topic_map = None;
        for h in handles {
            record_sender_slots.push(h.record_sender);
            if conn_state.is_none() {
                conn_state = Some(h.conn_state);
            }
            if let Some(bytes) = h.schema_bytes {
                if ingest_schema_bytes.is_empty() {
                    ingest_schema_bytes = bytes;
                }
            }
            if let Some(d) = h.discovery {
                if mqtt_topics.is_none() {
                    mqtt_topics = Some(d.discovered);
                    mqtt_topic_map = Some(d.topic_map);
                }
            }
        }
        Self { conn_state, record_sender_slots, ingest_schema_bytes, mqtt_topics, mqtt_topic_map }
    }
}
```

Change `DataVisApp::new` to accept `sources: Vec<crate::ingest::SourceHandle>` in
place of the six ingest params (`conn_state`, `record_sender_slots`,
`ingest_schema_bytes`, `mqtt_topics`, `mqtt_topic_map` — keep `live_view_ns`,
`live_history_s`, `default_window_s`). At the top of the body:

```rust
let DerivedIngest {
    conn_state,
    record_sender_slots,
    ingest_schema_bytes,
    mqtt_topics,
    mqtt_topic_map,
} = DerivedIngest::from_handles(sources);
```

The struct-literal field assignments (`conn_state`, `record_sender_slots`,
`ingest_schema_bytes`, `mqtt_topics`, `mqtt_topic_map`) stay unchanged — they now
read the destructured locals.

Update `main.rs` to build sources and spawn them:

```rust
let mut sources: Vec<datavis::ingest::SourceHandle> = Vec::new();
let mut ingest_schema_bytes = vec![]; // retained only for demo-mode branch below

if let Some(broker) = mqtt_endpoint {
    let src = datavis::ingest::MqttSource::new(
        MqttConfig { broker_url: broker, client_id: "datavis".to_string() },
        &channels,
    );
    sources.push(Box::new(src).spawn(store.clone()));
}

if demo {
    datavis::demo::spawn_demo(store.clone(), &channels);
} else {
    let config = IngestConfig { endpoint, proto_path: PathBuf::from(&schema_path) };
    match datavis::ingest::ZmqSource::build(config, &channels) {
        Ok(src) => sources.push(Box::new(src).spawn(store.clone())),
        Err(e) => eprintln!("ingest: failed to start ({e}); running without live data"),
    }
}
let _ = &ingest_schema_bytes;
```

Then simplify the `DataVisApp::new` call to pass `sources` in place of the six
former args. Update imports in `main.rs`: add `use datavis::ingest::{DataSource, MqttSource, SourceHandle, ZmqSource};`, drop `MqttConfig`/`IngestConfig` only if unused (they are still used — keep them).

Delete the dead wrappers and types:
- `mqtt.rs`: remove `spawn_mqtt_ingest`, `MqttHandles`, and update the two tests
  that used them — repoint `topic_map_built_from_mqtt_channels` to assert on
  `MqttSource::new(...).topic_map` contents (make the field `pub(crate)` or add a
  test accessor), and delete `spawn_returns_discovered_set` (covered by
  `mqtt_source_handle_has_discovery_no_schema`).
- `mod.rs`: remove `spawn_ingest`, `IngestHandle`; repoint
  `spawn_ingest_missing_schema_returns_err` and
  `schema_bytes_via_spawn_ingest_are_non_empty` to `ZmqSource::build`.
- `mod.rs` re-export line: drop `spawn_mqtt_ingest, MqttHandles`; add
  `pub use mqtt::{MqttConfig, MqttSource};` and `pub use self::ZmqSource;` (adjust to
  actual paths; `ZmqSource` is defined in `mod.rs` so it is already public).

- [ ] **Step 4: Run the full suite to verify it passes**

Run: `cargo test`
Expected: PASS — all tests, including the adapted app + ingest tests. Then:

Run: `cargo build`
Expected: clean build, no unused-import/dead-code warnings from the ingest module.

- [ ] **Step 5: Run the app to confirm it launches**

Run: `cargo run -- --demo` (Ctrl-C after the window appears)
Expected: window opens, demo data plots — confirms the wiring path is intact.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs src/main.rs src/ingest/mqtt.rs src/ingest/mod.rs
git commit -m "refactor: wire app and main to unified Vec<SourceHandle>"
```

---

## Self-Review Notes

- **Spec coverage:** trait (Task 1), ScalarIngest (Task 2), MqttSource+conn_state
  (Task 3), ZmqSource (Task 4), app+main collapse to `Vec<SourceHandle>` (Task 5).
  All design "Components changed" items are covered.
- **Deliberate deviation:** MQTT `TIMEOUT` state not implemented (see Global
  Constraints). Flagged for the human; the design's "mirror timeout logic" line
  should be softened to "CONNECTING/LIVE" when the spec is next touched.
- **Type consistency:** `SourceHandle`/`Discovery`/`DataSource` field and method
  names are identical across Tasks 1–5. `ScalarIngest::new` arg order (discovered,
  topic_map, store, record_sender) matches every call site.
- **Green at each task:** Tasks 3–4 keep the old `spawn_*` wrappers so `main.rs`
  compiles; Task 5 removes them together with the constructor switch.
</content>
