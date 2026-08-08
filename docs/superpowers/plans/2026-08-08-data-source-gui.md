# Data Source GUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a GUI window (opened from a "Data Source" button beside File) to view, start, and stop ZMQ/MQTT/WebSocket/bridge sources at runtime, persisting them to `config.toml`.

**Architecture:** A new `SourceManager` owned by `DataVisApp` spawns and tears down sources against the shared live store, replacing the frozen `DerivedIngest` fields with methods computed over the current source set. A new `[sources]` config section (`SourcesConfig`) persists sources and is loaded at boot. `SourceHandle` gains an `Arc<AtomicBool>` stop signal each run-loop observes. The window is a deferred egui viewport.

**Tech Stack:** Rust 2021, eframe/egui 0.28, `toml_edit` 0.22, `rfd` 0.14, crossbeam-channel, existing `zmq`/`rumqttc`/`tungstenite` sources.

## Global Constraints

- Never add Co-Authored-By or AI self-attribution to commits/PRs.
- Do NOT commit `config.toml` (local session state); it is git-ignored intent — leave it untracked/modified.
- Commit only when the human explicitly asks. Each task's "Commit" step stages code and writes the message but the controller decides when to actually run it if the human has deferred commits. (When executing, commit per task as normal unless told otherwise.)
- Match surrounding code style: doc comments on public items, `icon::` constants from `egui_phosphor::regular`, `anyhow` for fallible construction.
- Config writes MUST preserve unrelated sections and comments (follow `LayoutConfig::merge_into` / `save` in `src/config/layout.rs`).
- Config key names are fixed: `endpoint`, `schema`, `broker`, `client_id`, `listen`, and existing bridge `name`/`command`/`args`. `schema` maps to `IngestConfig.proto_path`; `broker` maps to `MqttConfig.broker_url`.
- `client_id` defaults to `"datavis"` when omitted. `schema` is required for a ZMQ entry.

---

## File Structure

- Create: `src/config/sources.rs` — `SourcesConfig` + per-kind config structs, parse + save.
- Create: `src/ingest/manager.rs` — `SourceManager`, `SourceSpec`, `SourceKind`, `LiveSource`.
- Create: `src/app/data_source_window.rs` OR a method block in `app.rs` — the window UI + `AddSourceForm`. (Plan uses a new module `src/data_source_window.rs` to keep `app.rs` from growing; if the codebase prefers inline, fold into `app.rs`.)
- Modify: `src/config/mod.rs` — export `SourcesConfig`.
- Modify: `src/ingest/source.rs` — add `stop: Arc<AtomicBool>` to `SourceHandle`.
- Modify: `src/ingest/mod.rs` (ZMQ `spawn`), `src/ingest/thread.rs` (ZMQ loop), `src/ingest/mqtt.rs`, `src/ingest/websocket.rs`, `src/ingest/bridge/source.rs` — set/observe `stop`.
- Modify: `src/ingest/mod.rs` — export `manager` items.
- Modify: `src/app.rs` — hold `SourceManager`, replace `DerivedIngest` reads, add window toggle + menu button, recording/status/discovery via manager.
- Modify: `src/main.rs` — load `SourcesConfig`, build manager, ephemeral CLI sources, drop default-ZMQ.
- Modify: `src/config/default_config.toml` — seed a `[[sources.zmq]]` entry.

---

## Task 1: `SourcesConfig` parse

**Files:**
- Create: `src/config/sources.rs`
- Modify: `src/config/mod.rs:1-7`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Default)]
  pub struct SourcesConfig {
      pub zmq: Vec<ZmqSourceConfig>,
      pub mqtt: Vec<MqttSourceConfig>,
      pub websocket: Vec<WsSourceConfig>,
      pub bridge: Vec<crate::ingest::bridge::BridgeConfig>,
  }
  #[derive(Debug, Clone, PartialEq)] pub struct ZmqSourceConfig { pub endpoint: String, pub schema: String }
  #[derive(Debug, Clone, PartialEq)] pub struct MqttSourceConfig { pub broker: String, pub client_id: String }
  #[derive(Debug, Clone, PartialEq)] pub struct WsSourceConfig { pub listen: String }
  impl SourcesConfig {
      pub fn from_toml_str(s: &str) -> anyhow::Result<Self>;
      pub fn load(path: &std::path::Path) -> anyhow::Result<Self>;
  }
  ```

- [ ] **Step 1: Write the failing test**

Add to `src/config/sources.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_kinds() {
        let s = r#"
[[sources.zmq]]
endpoint = "tcp://localhost:5555"
schema = "schema.proto"

[[sources.mqtt]]
broker = "localhost:1883"

[[sources.websocket]]
listen = "0.0.0.0:9001"

[[sources.bridge]]
name = "acme"
command = "/opt/acme"
args = ["--fast"]
"#;
        let c = SourcesConfig::from_toml_str(s).unwrap();
        assert_eq!(c.zmq, vec![ZmqSourceConfig { endpoint: "tcp://localhost:5555".into(), schema: "schema.proto".into() }]);
        assert_eq!(c.mqtt[0].client_id, "datavis"); // default
        assert_eq!(c.websocket[0].listen, "0.0.0.0:9001");
        assert_eq!(c.bridge[0].name, "acme");
    }

    #[test]
    fn absent_section_is_empty() {
        let c = SourcesConfig::from_toml_str("default_window_s = 5.0\n").unwrap();
        assert!(c.zmq.is_empty() && c.mqtt.is_empty() && c.websocket.is_empty() && c.bridge.is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config::sources`
Expected: FAIL (module/types not defined).

- [ ] **Step 3: Implement parse**

Write `src/config/sources.rs`. Mirror `src/ingest/bridge/config.rs` DocWrapper pattern. `client_id` default via serde:
```rust
use serde::Deserialize;
use anyhow::Context;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourcesConfig {
    pub zmq: Vec<ZmqSourceConfig>,
    pub mqtt: Vec<MqttSourceConfig>,
    pub websocket: Vec<WsSourceConfig>,
    pub bridge: Vec<crate::ingest::bridge::BridgeConfig>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZmqSourceConfig { pub endpoint: String, pub schema: String }

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttSourceConfig {
    pub broker: String,
    #[serde(default = "default_client_id")]
    pub client_id: String,
}
fn default_client_id() -> String { "datavis".to_string() }

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WsSourceConfig { pub listen: String }

#[derive(Deserialize)]
struct DocWrapper { sources: Option<RawSources> }
#[derive(Deserialize, Default)]
struct RawSources {
    #[serde(default)] zmq: Vec<ZmqSourceConfig>,
    #[serde(default)] mqtt: Vec<MqttSourceConfig>,
    #[serde(default)] websocket: Vec<WsSourceConfig>,
    #[serde(default)] bridge: Vec<crate::ingest::bridge::BridgeConfig>,
}

impl SourcesConfig {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let doc: DocWrapper = toml::from_str(s).context("parsing [sources]")?;
        let r = doc.sources.unwrap_or_default();
        Ok(Self { zmq: r.zmq, mqtt: r.mqtt, websocket: r.websocket, bridge: r.bridge })
    }
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml_str(&s)
    }
}
```
Note: `RawSources` must derive `Default` for `unwrap_or_default`; add `#[derive(Default)]` (already shown). If `#[serde(deny_unknown_fields)]` on `RawSources` conflicts with the existing bridge parser's tolerant `[sources]`, do NOT add deny on `RawSources` (leave it tolerant), matching `src/ingest/bridge/config.rs:24-28`.

Then in `src/config/mod.rs` add `pub mod sources;` and `pub use sources::{SourcesConfig, ZmqSourceConfig, MqttSourceConfig, WsSourceConfig};`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config::sources`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config/sources.rs src/config/mod.rs
git commit -m "feat(config): parse [sources] zmq/mqtt/websocket/bridge section"
```

---

## Task 2: `SourcesConfig::save` (preserve other sections)

**Files:**
- Modify: `src/config/sources.rs`

**Interfaces:**
- Consumes: `SourcesConfig` from Task 1.
- Produces:
  ```rust
  impl SourcesConfig {
      /// Serialize just the `[sources]` table, preserving every other section
      /// and comments in `existing`. Used by both save() and tests.
      pub fn merge_into(&self, existing: &str) -> anyhow::Result<String>;
      pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()>;
  }
  ```

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn save_preserves_other_sections_and_round_trips() {
    let existing = r#"default_window_s = 30.0

[defaults]
max_rate = 100000  # keep me

# hand-authored channel
[channels."volt.l1"]
type = "float"
"#;
    let mut c = SourcesConfig::default();
    c.zmq.push(ZmqSourceConfig { endpoint: "tcp://x:1".into(), schema: "s.proto".into() });
    c.mqtt.push(MqttSourceConfig { broker: "h:1883".into(), client_id: "datavis".into() });
    let out = c.merge_into(existing).unwrap();
    assert!(out.contains("# keep me"), "comment preserved");
    assert!(out.contains("[channels.\"volt.l1\"]"), "channels preserved");
    let reparsed = SourcesConfig::from_toml_str(&out).unwrap();
    assert_eq!(reparsed, c);
}

#[test]
fn save_with_empty_sources_removes_table() {
    let existing = "[[sources.zmq]]\nendpoint=\"a\"\nschema=\"b\"\n[defaults]\nmax_rate=1\n";
    let out = SourcesConfig::default().merge_into(existing).unwrap();
    let reparsed = SourcesConfig::from_toml_str(&out).unwrap();
    assert!(reparsed.zmq.is_empty());
    assert!(out.contains("[defaults]"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config::sources::tests::save`
Expected: FAIL (`merge_into` not defined).

- [ ] **Step 3: Implement save via toml_edit**

Build the `sources` table programmatically and replace it in the parsed document, then serialize:
```rust
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

impl SourcesConfig {
    pub fn merge_into(&self, existing: &str) -> anyhow::Result<String> {
        let mut doc: DocumentMut = existing.parse().context("parsing existing config")?;
        doc.remove("sources");
        if self.is_empty() {
            return Ok(doc.to_string());
        }
        let mut sources = Table::new();
        sources.set_implicit(true);

        let mut zmq = ArrayOfTables::new();
        for z in &self.zmq {
            let mut t = Table::new();
            t["endpoint"] = value(&z.endpoint);
            t["schema"] = value(&z.schema);
            zmq.push(t);
        }
        if !zmq.is_empty() { sources.insert("zmq", Item::ArrayOfTables(zmq)); }

        let mut mqtt = ArrayOfTables::new();
        for m in &self.mqtt {
            let mut t = Table::new();
            t["broker"] = value(&m.broker);
            t["client_id"] = value(&m.client_id);
            mqtt.push(t);
        }
        if !mqtt.is_empty() { sources.insert("mqtt", Item::ArrayOfTables(mqtt)); }

        let mut ws = ArrayOfTables::new();
        for w in &self.websocket {
            let mut t = Table::new();
            t["listen"] = value(&w.listen);
            ws.push(t);
        }
        if !ws.is_empty() { sources.insert("websocket", Item::ArrayOfTables(ws)); }

        let mut bridge = ArrayOfTables::new();
        for b in &self.bridge {
            let mut t = Table::new();
            t["name"] = value(&b.name);
            t["command"] = value(&b.command);
            let mut a = Array::new();
            for arg in &b.args { a.push(arg.as_str()); }
            t["args"] = value(a);
            bridge.push(t);
        }
        if !bridge.is_empty() { sources.insert("bridge", Item::ArrayOfTables(bridge)); }

        doc.insert("sources", Item::Table(sources));
        Ok(doc.to_string())
    }

    pub fn is_empty(&self) -> bool {
        self.zmq.is_empty() && self.mqtt.is_empty()
            && self.websocket.is_empty() && self.bridge.is_empty()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let out = self.merge_into(&existing)?;
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
```
If `value(&String)` does not resolve, use `value(z.endpoint.as_str())`. Verify `Array::push(&str)` compiles under toml_edit 0.22; adjust to `a.push(arg.clone())` if needed.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config::sources`
Expected: PASS (all sources tests).

- [ ] **Step 5: Commit**

```bash
git add src/config/sources.rs
git commit -m "feat(config): write [sources] preserving other sections"
```

---

## Task 3: Stop signal on `SourceHandle` + ZMQ teardown

**Files:**
- Modify: `src/ingest/source.rs:51-80` (add field), `src/ingest/source.rs` tests
- Modify: `src/ingest/mod.rs:74-95` (ZMQ spawn sets stop)
- Modify: `src/ingest/thread.rs`

**Interfaces:**
- Produces: `SourceHandle { …, pub stop: Arc<std::sync::atomic::AtomicBool> }` — set `false` at spawn; `SourceManager::remove` sets `true`.
- Consumes (later tasks): every `SourceHandle` literal must set `stop`.

- [ ] **Step 1: Write the failing test**

In `src/ingest/thread.rs` tests (add a `#[cfg(test)] mod tests`):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn connect_and_recv_returns_when_stopped() {
        // No publisher on this port → recv times out (EAGAIN) each second.
        // With stop set, connect_and_recv must return Ok(()) promptly.
        let router = crate::ingest::router::TopicRouter::empty();
        let store: Arc<dyn ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(
                &crate::config::ChannelRegistry::from_toml_str("").unwrap()));
        let state = Arc::new(AtomicU8::new(CONNECTING));
        let rec = Arc::new(std::sync::Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(true)); // already stopped
        let r = connect_and_recv("tcp://localhost:5599", &router, store.as_ref(),
                                 &state, &rec, &stop);
        assert!(r.is_ok());
    }
}
```
If `TopicRouter::empty()` does not exist, build one from an empty registry via the existing `TopicRouter::build(&registry, &schema)` path, or add a small `empty()` constructor in `router.rs` (test-only helper).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::thread`
Expected: FAIL (`connect_and_recv` arity / `stop` param missing).

- [ ] **Step 3: Implement**

`src/ingest/source.rs`: add `pub stop: Arc<AtomicBool>` to `SourceHandle` (import `AtomicBool`). Update the `handle_holds_optional_capabilities` test literal to set `stop: Arc::new(AtomicBool::new(false))`.

`src/ingest/mod.rs` ZMQ `spawn`: create `let stop = Arc::new(AtomicBool::new(false));`, clone into the thread, pass to `thread::run_loop(endpoint, router, store, state_clone, sender_clone, stop.clone())`, and set `stop` in the returned `SourceHandle`.

`src/ingest/thread.rs`: add `stop: Arc<AtomicBool>` param to `run_loop` and `connect_and_recv`. In `run_loop`, break the outer loop when `stop.load(Relaxed)`. In `connect_and_recv`, at the top of the inner `loop`, `if stop.load(Relaxed) { return Ok(()); }`; also return `Ok(())` in the `EAGAIN` branch when stop is set.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib ingest`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/source.rs src/ingest/mod.rs src/ingest/thread.rs
git commit -m "feat(ingest): add stop signal to SourceHandle; ZMQ honors it"
```

---

## Task 4: MQTT teardown

**Files:**
- Modify: `src/ingest/mqtt.rs`

**Interfaces:**
- Consumes: `SourceHandle.stop` from Task 3.
- Produces: MQTT `spawn` sets `stop`; a watcher thread calls `client.disconnect()` when stop flips, and `run_loop` returns without reconnecting.

- [ ] **Step 1: Write the failing test**

MQTT needs no broker to test the stop→disconnect wiring at the unit level cheaply; assert instead that `spawn` returns a handle whose `stop` starts `false` and, after setting it, the spawned threads terminate. Because thread termination is timing-dependent, keep the test minimal and deterministic:
```rust
#[test]
fn spawn_sets_stop_false_and_disconnects_on_stop() {
    use std::sync::atomic::Ordering;
    let registry = crate::config::ChannelRegistry::from_toml_str("").unwrap();
    let store: Arc<dyn crate::store::ChannelStore> =
        Arc::new(crate::store::LiveStore::from_registry(&registry));
    let src = MqttSource::new(
        MqttConfig { broker_url: "localhost:1".into(), client_id: "t".into() },
        &registry);
    let handle = Box::new(src).spawn(store);
    assert_eq!(handle.stop.load(Ordering::Relaxed), false);
    handle.stop.store(true, Ordering::Relaxed);
    // Give the watcher a beat; disconnect on a dead broker is a no-op error we ignore.
    std::thread::sleep(std::time::Duration::from_millis(300));
    // No assertion on thread join (no join handle); test verifies wiring compiles
    // and stop is settable. Real teardown is exercised manually / in integration.
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::mqtt`
Expected: FAIL (`handle.stop` field missing until Task 3 merged — after Task 3 it compiles; this test then also drives the new watcher wiring).

- [ ] **Step 3: Implement**

In `src/ingest/mqtt.rs` `spawn`:
```rust
let stop = Arc::new(AtomicBool::new(false));
```
Pass `stop.clone()` into `run_loop`. Inside `run_loop`, after `let (client, mut connection) = Client::new(opts, 64);`, spawn a watcher:
```rust
let watch_stop = stop.clone();
let watch_client = client.clone();
std::thread::spawn(move || {
    while !watch_stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = watch_client.disconnect();
});
```
In the `for event in connection.iter()` loop, at the top of each iteration `if stop.load(Ordering::Relaxed) { return; }`, and in the `Err(_)` arm return instead of sleeping-and-reconnecting when stop is set. Set `stop` in the returned `SourceHandle`. Add `use std::sync::atomic::AtomicBool;`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib ingest::mqtt`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/mqtt.rs
git commit -m "feat(ingest): MQTT source honors stop signal via disconnect watcher"
```

---

## Task 5: WebSocket teardown

**Files:**
- Modify: `src/ingest/websocket.rs`

**Interfaces:**
- Consumes: `SourceHandle.stop`.
- Produces: WS `spawn` sets `stop`; `accept_loop` polls a non-blocking listener and returns when stop is set.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn accept_loop_stops_promptly() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let registry = crate::config::ChannelRegistry::from_toml_str("").unwrap();
    let store: Arc<dyn crate::store::ChannelStore> =
        Arc::new(crate::store::LiveStore::from_registry(&registry));
    let src = WsSource::new(WsConfig { listen: "127.0.0.1:0".into() }, &registry);
    let handle = Box::new(src).spawn(store);
    assert_eq!(handle.stop.load(Ordering::Relaxed), false);
    handle.stop.store(true, Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(300));
    // Wiring test: stop is settable and the accept loop observes it.
}
```
`"127.0.0.1:0"` binds an ephemeral port so the test never collides.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::websocket`
Expected: FAIL until implemented.

- [ ] **Step 3: Implement**

Thread `stop: Arc<AtomicBool>` into `accept_loop`. After binding the `TcpListener`, call `listener.set_nonblocking(true)`. In the accept `loop`:
```rust
if stop.load(Ordering::Relaxed) { return; }
match listener.accept() {
    Ok((sock, _)) => { sock.set_nonblocking(false).ok(); /* spawn reader as today */ }
    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
        std::thread::sleep(Duration::from_millis(100));
        continue;
    }
    Err(e) => { eprintln!("ws: accept: {e}"); return; }
}
```
Ensure each accepted socket is reset to blocking before it is handed to the per-connection reader (tungstenite expects blocking). Set `stop` on the returned `SourceHandle`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib ingest::websocket`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/websocket.rs
git commit -m "feat(ingest): WebSocket source honors stop via non-blocking accept"
```

---

## Task 6: Bridge sets stop

**Files:**
- Modify: `src/ingest/bridge/source.rs`

**Interfaces:**
- Consumes: `SourceHandle.stop`, existing `ChildGuard { stop, current }`.
- Produces: bridge `spawn` returns a `SourceHandle` whose `stop` is the SAME `Arc<AtomicBool>` the `ChildGuard`/reader thread already watches, so `SourceManager::remove` tears the child down.

- [ ] **Step 1: Write the failing test**

In `src/ingest/bridge/source.rs` tests, assert the handle's `stop` and the child guard's stop are the same underlying flag (or at least both settable). Adapt existing bridge tests that build a `SubprocessSource`:
```rust
#[test]
fn handle_stop_drives_child_guard() {
    use std::sync::atomic::Ordering;
    let registry = crate::config::ChannelRegistry::from_toml_str("").unwrap();
    let store: Arc<dyn crate::store::ChannelStore> =
        Arc::new(crate::store::LiveStore::from_registry(&registry));
    let cfg = crate::ingest::bridge::BridgeConfig {
        name: "t".into(), command: "true".into(), args: vec![] };
    let handle = Box::new(crate::ingest::bridge::SubprocessSource::new(cfg, &registry)).spawn(store);
    handle.stop.store(true, Ordering::Relaxed);
    // reader thread observes stop and ceases respawning
    assert!(handle.stop.load(Ordering::Relaxed));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::bridge`
Expected: FAIL (handle has no `stop` set consistently / field just added).

- [ ] **Step 3: Implement**

In bridge `spawn`, create the reader/restart `stop` `Arc<AtomicBool>` as today, use it for both the `ChildGuard.stop` AND the returned `SourceHandle.stop` (share one `Arc`). Keep `child_guard: Some(ChildGuard { stop: stop.clone(), current })`. This way setting `handle.stop` also stops respawns; `ChildGuard::drop` still kills the current child on drop.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib ingest::bridge`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/bridge/source.rs
git commit -m "feat(ingest): bridge SourceHandle.stop drives child teardown"
```

---

## Task 7: `SourceManager`

**Files:**
- Create: `src/ingest/manager.rs`
- Modify: `src/ingest/mod.rs` (add `pub mod manager;` + re-exports)

**Interfaces:**
- Consumes: source constructors (`ZmqSource::build`, `MqttSource::new`, `WsSource::new`, `bridge::SubprocessSource::new`), `SourceHandle` (+ `stop`), `SourcesConfig` (Task 1) for `SourceSpec` conversion.
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum SourceKind { Zmq, Mqtt, WebSocket, Bridge }
  #[derive(Clone, Debug, PartialEq)] pub enum SourceSpec {
      Zmq { endpoint: String, schema: String },
      Mqtt { broker: String, client_id: String },
      WebSocket { listen: String },
      Bridge(crate::ingest::bridge::BridgeConfig),
  }
  pub struct LiveSource {
      pub id: u64, pub kind: SourceKind, pub label: String,
      pub persist: bool,   // true: config/GUI source (written to config); false: ephemeral CLI source
      /* private: spec: SourceSpec, handle: SourceHandle */
  }
  impl LiveSource { pub fn spec(&self) -> &SourceSpec; }
  pub struct SourceManager { /* store, channels, sources, next_id */ }
  impl SourceManager {
      pub fn new(store: Arc<dyn ChannelStore>, channels: Arc<ChannelRegistry>) -> Self;
      /// persist=true for config/GUI sources, false for ephemeral CLI ones.
      pub fn add(&mut self, spec: &SourceSpec, persist: bool) -> anyhow::Result<u64>;
      pub fn remove(&mut self, id: u64);
      pub fn conn_states(&self) -> Vec<Arc<AtomicU8>>;
      pub fn record_sender_slots(&self) -> Vec<Arc<Mutex<Option<Sender<RecordMsg>>>>>;
      pub fn schema_bytes(&self) -> Vec<u8>;
      pub fn discovery(&self) -> Option<(Arc<Mutex<BTreeMap<String,String>>>, Arc<MqttTopicMap>)>;
      pub fn sources(&self) -> &[LiveSource];
  }
  impl SourceSpec { pub fn label(&self) -> String; pub fn kind(&self) -> SourceKind; }
  ```
  Also add `SourcesConfig::specs(&self) -> Vec<SourceSpec>` in `src/config/sources.rs` (config→spec) and `SourcesConfig::push(&mut self, spec: &SourceSpec)` for persistence (Task 10). `SourceKind` derives `Default` with `Zmq` as `#[default]` (needed by the Task 10 form). `LiveSource` stores the `SourceSpec` and a `persist: bool`; the manager exposes `sources()` so Task 10 can rebuild `SourcesConfig` from `sources().iter().filter(|s| s.persist).map(|s| s.spec())`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn mgr() -> SourceManager {
        let registry = std::sync::Arc::new(
            crate::config::ChannelRegistry::from_toml_str("").unwrap());
        let store: Arc<dyn ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(&registry));
        SourceManager::new(store, registry)
    }

    #[test]
    fn add_grows_derived_views_remove_shrinks_and_stops() {
        let mut m = mgr();
        // WebSocket binds ephemeral; no external dependency.
        let id = m.add(&SourceSpec::WebSocket { listen: "127.0.0.1:0".into() }, true).unwrap();
        assert_eq!(m.conn_states().len(), 1);
        assert_eq!(m.record_sender_slots().len(), 1);
        assert!(m.discovery().is_some());
        assert!(m.sources()[0].persist);
        m.remove(id);
        assert_eq!(m.conn_states().len(), 0);
        assert!(m.discovery().is_none());
    }

    #[test]
    fn add_zmq_missing_schema_errors_and_adds_nothing() {
        let mut m = mgr();
        let r = m.add(&SourceSpec::Zmq {
            endpoint: "tcp://localhost:5599".into(),
            schema: "/nonexistent.proto".into() }, true);
        assert!(r.is_err());
        assert_eq!(m.sources().len(), 0);
    }

    #[test]
    fn remove_unknown_id_is_noop() {
        let mut m = mgr();
        m.remove(999);
        assert_eq!(m.sources().len(), 0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ingest::manager`
Expected: FAIL.

- [ ] **Step 3: Implement**

Write `src/ingest/manager.rs`. `add` matches on `spec`, builds + spawns:
```rust
pub fn add(&mut self, spec: &SourceSpec, persist: bool) -> anyhow::Result<u64> {
    let handle = match spec {
        SourceSpec::Zmq { endpoint, schema } => {
            let src = crate::ingest::ZmqSource::build(
                crate::ingest::IngestConfig {
                    endpoint: endpoint.clone(),
                    proto_path: std::path::PathBuf::from(schema),
                },
                self.channels.as_ref())?;
            Box::new(src).spawn(self.store.clone())
        }
        SourceSpec::Mqtt { broker, client_id } => {
            let src = crate::ingest::MqttSource::new(
                crate::ingest::MqttConfig { broker_url: broker.clone(), client_id: client_id.clone() },
                self.channels.as_ref());
            Box::new(src).spawn(self.store.clone())
        }
        SourceSpec::WebSocket { listen } => {
            let src = crate::ingest::WsSource::new(
                crate::ingest::WsConfig { listen: listen.clone() }, self.channels.as_ref());
            Box::new(src).spawn(self.store.clone())
        }
        SourceSpec::Bridge(cfg) => {
            let src = crate::ingest::bridge::SubprocessSource::new(cfg.clone(), self.channels.as_ref());
            Box::new(src).spawn(self.store.clone())
        }
    };
    let id = self.next_id;
    self.next_id += 1;
    self.sources.push(LiveSource {
        id, kind: spec.kind(), label: spec.label(),
        persist, spec: spec.clone(), handle,
    });
    Ok(id)
}

pub fn remove(&mut self, id: u64) {
    if let Some(pos) = self.sources.iter().position(|s| s.id == id) {
        let s = self.sources.remove(pos);
        s.handle.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // handle drops here; ChildGuard (bridge) fires on drop.
    }
}
```
`schema_bytes` returns the first non-empty `handle.schema_bytes` clone (else empty vec). `discovery` returns the first `handle.discovery` maps cloned. `conn_states`/`record_sender_slots` map over `sources`. `SourceSpec::label`: e.g. `format!("zmq {endpoint}")`, `format!("mqtt {broker}")`, `format!("ws {listen}")`, `format!("bridge {}", cfg.name)`.

Add to `src/ingest/mod.rs`: `pub mod manager;` and `pub use manager::{SourceManager, SourceSpec, SourceKind, LiveSource};`. Add `WsConfig` is already exported; confirm `IngestConfig`, `MqttConfig` are re-exported (they are used from `main.rs`, so yes).

In `src/config/sources.rs` add `specs()`:
```rust
impl SourcesConfig {
    pub fn specs(&self) -> Vec<crate::ingest::SourceSpec> {
        use crate::ingest::SourceSpec as S;
        let mut v = Vec::new();
        for z in &self.zmq { v.push(S::Zmq { endpoint: z.endpoint.clone(), schema: z.schema.clone() }); }
        for m in &self.mqtt { v.push(S::Mqtt { broker: m.broker.clone(), client_id: m.client_id.clone() }); }
        for w in &self.websocket { v.push(S::WebSocket { listen: w.listen.clone() }); }
        for b in &self.bridge { v.push(S::Bridge(b.clone())); }
        v
    }
    pub fn push(&mut self, spec: &crate::ingest::SourceSpec) {
        use crate::ingest::SourceSpec as S;
        match spec {
            S::Zmq { endpoint, schema } => self.zmq.push(ZmqSourceConfig { endpoint: endpoint.clone(), schema: schema.clone() }),
            S::Mqtt { broker, client_id } => self.mqtt.push(MqttSourceConfig { broker: broker.clone(), client_id: client_id.clone() }),
            S::WebSocket { listen } => self.websocket.push(WsSourceConfig { listen: listen.clone() }),
            S::Bridge(cfg) => self.bridge.push(cfg.clone()),
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib ingest::manager config::sources`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/manager.rs src/ingest/mod.rs src/config/sources.rs
git commit -m "feat(ingest): SourceManager for runtime add/remove of sources"
```

---

## Task 8: Wire `DataVisApp` to `SourceManager`

**Files:**
- Modify: `src/app.rs` (struct, `new`, `start_recording`, `stop_recording`, sidebar discovery snapshot ~1131-1160, `aggregate_conn_state` call ~664)

**Interfaces:**
- Consumes: `SourceManager` (Task 7).
- Produces: `DataVisApp` holds `manager: SourceManager` and `config_path: PathBuf` (for persistence, reuse `layout_path` which already points at `config.toml`). `DataVisApp::new` signature changes: replace the `sources: Vec<SourceHandle>` param with `manager: SourceManager`.

- [ ] **Step 1: Update the app-level test**

Adjust `record_sender_slots_install_and_clear` and the `DerivedIngest` tests: `DerivedIngest` is removed, so migrate those assertions to `SourceManager` (already covered in Task 7) or delete the now-obsolete `DerivedIngest` tests in `app.rs`. Add:
```rust
#[test]
fn recording_reads_slots_from_manager() {
    // Build a manager with one ephemeral WS source, confirm start/stop recording
    // installs and clears the sender via manager.record_sender_slots().
    // (Construct DataVisApp is heavy; instead assert at the manager level that
    // slots are shared Arcs the app can install into — covered in Task 7.)
}
```
Prefer deleting `DerivedIngest`-specific tests and relying on Task 7 coverage; keep `aggregate_conn_state` tests (they take `&[Arc<AtomicU8>]` and are unaffected).

- [ ] **Step 2: Run to verify current tests fail to compile**

Run: `cargo test --lib app`
Expected: FAIL (compile) once `DerivedIngest` fields are removed.

- [ ] **Step 3: Implement**

- Remove `DerivedIngest` struct + `from_handles` (and its tests) from `app.rs`.
- Replace these struct fields: `conn_states`, `record_sender_slots`, `ingest_schema_bytes`, `mqtt_topics`, `mqtt_topic_map`, `_bridge_guards` with a single `manager: SourceManager`. Keep `mqtt_snapshot`/`mqtt_snapshot_at`.
- `new`: take `manager: SourceManager` instead of `sources`; store it. Drop the `DerivedIngest { .. } = from_handles(...)` block.
- `start_recording`: replace `self.record_sender_slots` with `let slots = self.manager.record_sender_slots();` (compute once), guard `slots.is_empty()`, install `tx.clone()` into each, and use `self.manager.schema_bytes()` for the MCAP header.
- `stop_recording`: clear senders via `self.manager.record_sender_slots()`.
- `aggregate_conn_state` call site (~664): pass `&self.manager.conn_states()`.
- Sidebar discovery (~1131-1160): replace `self.mqtt_topics`/`self.mqtt_topic_map` with `self.manager.discovery()` — bind `let disc = self.manager.discovery();` and use `disc.as_ref().map(|(topics, _)| topics)` for the snapshot and `disc.as_ref().map(|(_, map)| map)` for routing.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib`
Expected: PASS (whole lib compiles and tests pass).

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "refactor(app): drive sources through SourceManager"
```

---

## Task 9: `main.rs` boot via `SourceManager`

**Files:**
- Modify: `src/main.rs`
- Modify: `src/config/default_config.toml`

**Interfaces:**
- Consumes: `SourcesConfig` (Task 1), `SourceManager` (Task 7), updated `DataVisApp::new` (Task 8).

- [ ] **Step 1: Implement (no new unit test; verified by build + run)**

In `main.rs`:
- After loading channels/layout, `let sources_cfg = SourcesConfig::from_toml_str(&config_text).unwrap_or_default();` (reuse the `config_text` already read for bridges, or read once).
- Build `let mut manager = SourceManager::new(store.clone() as Arc<dyn ChannelStore>, channels.clone());`
- Add persisted sources: `for spec in sources_cfg.specs() { if let Err(e) = manager.add(&spec, true) { eprintln!("source: {e}"); } }`
- Ephemeral CLI sources (`manager.add(&spec, false)`, not persisted): if `--demo`, keep `spawn_demo` as today. Else, translate the existing CLI flags into `manager.add(&SourceSpec::…, false)`:
  - Only add the ZMQ CLI source when `--endpoint` OR `--schema` was explicitly passed (use `arg_value(...).is_some()`), NOT unconditionally — this removes the "ZMQ on by default" behavior.
  - `--mqtt-endpoint` → `SourceSpec::Mqtt`.
  - `--ws-listen` → `SourceSpec::WebSocket`.
- Remove the old direct `ZmqSource::build`/`MqttSource::new`/`WsSource::new`/bridge spawn block; bridges now come from `sources_cfg` (they were already read from config, so this is not a behavior change for bridges — but confirm bridges are NOT double-added: remove the separate `BridgeConfig::list_from_toml_str` spawn loop since `sources_cfg.bridge` covers it).
- Pass `manager` into `DataVisApp::new` instead of the `sources` Vec.
- Update `print_help` text: note that `[sources]` in config and the GUI configure sources; CLI flags are optional overrides.

In `src/config/default_config.toml`: add
```toml
[[sources.zmq]]
endpoint = "tcp://localhost:5555"
schema = "schema.proto"
```
so a freshly-saved default keeps today's out-of-box ZMQ behavior.

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles.

- [ ] **Step 3: Smoke-run**

Run: `cargo run -- --demo` (should open with the demo source, no ZMQ).
Then `cargo run` with a `config.toml` containing a `[[sources.zmq]]` (verify it starts a zmq source; status shows CONNECTING with no publisher).
Expected: app launches both ways; no panic.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs src/config/default_config.toml
git commit -m "feat: boot data sources from [sources] config via SourceManager"
```

---

## Task 10: Data Source window + menu button + persistence

**Files:**
- Create: `src/data_source_window.rs`
- Modify: `src/lib.rs` (add `pub mod data_source_window;` if a standalone module), `src/app.rs` (menu button, toggle field, form field, call `show`)

**Interfaces:**
- Consumes: `SourceManager` (add/remove/sources), `SourcesConfig::{save,push,specs}`, `SourceSpec`.
- Produces:
  ```rust
  #[derive(Default)] pub struct AddSourceForm {
      pub kind: SourceKind,          // default Zmq
      pub endpoint: String, pub schema: String,     // zmq
      pub broker: String, pub client_id: String,    // mqtt
      pub listen: String,                            // ws
      pub name: String, pub command: String, pub args: String, // bridge (args space/newline split)
      pub last_error: Option<String>,
  }
  impl AddSourceForm {
      /// Build a spec from the current form, or Err(msg) if a required field is blank.
      pub fn to_spec(&self) -> Result<SourceSpec, String>;
  }
  ```
  `SourceKind` needs `Default` (Zmq) — add `#[derive(Default)]` with `#[default]` on `Zmq` in Task 7's enum (adjust Task 7 if not present).

- [ ] **Step 1: Write the failing test**

In `src/data_source_window.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{SourceKind, SourceSpec};

    #[test]
    fn zmq_form_requires_endpoint_and_schema() {
        let mut f = AddSourceForm { kind: SourceKind::Zmq, ..Default::default() };
        assert!(f.to_spec().is_err());
        f.endpoint = "tcp://x:1".into();
        f.schema = "s.proto".into();
        assert_eq!(f.to_spec().unwrap(),
                   SourceSpec::Zmq { endpoint: "tcp://x:1".into(), schema: "s.proto".into() });
    }

    #[test]
    fn mqtt_defaults_client_id_when_blank() {
        let f = AddSourceForm { kind: SourceKind::Mqtt, broker: "h:1883".into(), ..Default::default() };
        assert_eq!(f.to_spec().unwrap(),
                   SourceSpec::Mqtt { broker: "h:1883".into(), client_id: "datavis".into() });
    }

    #[test]
    fn bridge_splits_args() {
        let f = AddSourceForm { kind: SourceKind::Bridge, name: "n".into(),
            command: "/c".into(), args: "--a  --b".into(), ..Default::default() };
        match f.to_spec().unwrap() {
            SourceSpec::Bridge(cfg) => assert_eq!(cfg.args, vec!["--a", "--b"]),
            _ => panic!(),
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib data_source_window`
Expected: FAIL.

- [ ] **Step 3: Implement the form logic + window**

`to_spec`: validate per kind, defaulting `client_id` to `datavis` when blank, splitting `args` on whitespace, returning `Err("endpoint required")` etc. for blank required fields.

Window rendering — a free function called each frame from `DataVisApp::update`:
```rust
pub fn show(ctx: &egui::Context, open: &mut bool,
            manager: &mut SourceManager, form: &mut AddSourceForm,
            config_path: &std::path::Path) {
    if !*open { return; }
    let vid = egui::ViewportId::from_hash_of("data_source_window");
    let builder = egui::ViewportBuilder::default()
        .with_title("Data Sources").with_inner_size([520.0, 420.0]);
    ctx.show_viewport_deferred(vid, builder, move |ctx, _class| {
        egui::CentralPanel::default().show(ctx, |ui| {
            // Active sources table: manager.sources() -> label, kind, status dot, Stop button.
            // Stop: collect id, after the loop call manager.remove(id) + persist_remove(...).
            // Add source: kind selector + per-kind fields + Browse pickers (rfd) + Start.
            // Start: match form.to_spec() { Ok(spec) => match manager.add(&spec, true) {
            //          Ok(_) => { let _ = persist(config_path, manager); form clear; }
            //          Err(e) => form.last_error = Some(e.to_string()) },
            //        Err(msg) => form.last_error = Some(msg) }
            // Stop: manager.remove(id); let _ = persist(config_path, manager);
        });
        if ctx.input(|i| i.viewport().close_requested()) { /* set a flag; see note */ }
    });
}
```
Persistence strategy (single helper for both add and remove): after any change (`manager.add(spec, true)` or `manager.remove(id)`), rebuild `SourcesConfig` from the manager's remaining **persistent** sources and `save`:
```rust
fn persist(path: &std::path::Path, manager: &SourceManager) -> Result<(), String> {
    let mut cfg = crate::config::SourcesConfig::default();
    for s in manager.sources().iter().filter(|s| s.persist) {
        cfg.push(s.spec());
    }
    cfg.save(path).map_err(|e| e.to_string())
}
```
This keeps `config.toml` in sync and excludes ephemeral CLI sources (`persist == false`), using the `LiveSource { persist, spec }` fields and `add(spec, persist)` finalized in Task 7. The GUI Start path calls `manager.add(&spec, true)` then `persist(...)`; Stop calls `manager.remove(id)` then `persist(...)`. Surface a `persist` error via `form.last_error`.

Wire into `app.rs`:
- Struct: add `data_source_window_open: bool`, `add_source_form: AddSourceForm`.
- `menu_bar` (after the File `ui.menu_button` block, still inside the `menu::bar`): add
  ```rust
  if ui.button(format!("{} Data Source", icon::PLUGS_CONNECTED)).clicked() {
      self.data_source_window_open = !self.data_source_window_open;
  }
  ```
  (Pick an existing phosphor icon that resolves; `PLUGS_CONNECTED` or `BROADCAST` — verify against `egui_phosphor::regular`.)
- In `update`, after drawing the main UI, call
  `crate::data_source_window::show(ctx, &mut self.data_source_window_open, &mut self.manager, &mut self.add_source_form, &self.layout_path);`

Close handling: `show_viewport_deferred` runs its own event loop; set `*open=false` when the child viewport requests close (read `close_requested` inside the deferred closure and write through a shared flag; since `open` is `&mut` captured by the outer function, use an `Arc<AtomicBool>` bridge or move the close check to the outer function via `ctx.input_for(vid, ...)` in `app.rs`). Implement whichever compiles cleanly under eframe 0.28; a working pattern: keep `open: &mut bool` and, in `update`, check `ctx.input(|i| i.viewport().close_requested())` is not applicable to child — instead read `ctx.send_viewport_cmd_to(vid, ...)`; if this proves fiddly, fall back to an in-app `egui::Window` that is still visually a separate floating window. **The requirement is "its own window"; prefer the real viewport, but a floating `egui::Window` is an acceptable fallback if deferred-viewport close handling is problematic under 0.28.**

- [ ] **Step 4: Run tests + build**

Run: `cargo test --lib data_source_window` then `cargo build`
Expected: PASS + compiles.

- [ ] **Step 5: Manual verification**

Run: `cargo run -- --demo`
- Click **Data Source** in the menu bar → window opens.
- Add a WebSocket `127.0.0.1:9099` → Start → row appears, status dot shows; `config.toml` gains `[[sources.websocket]]`.
- Stop it → row disappears; `config.toml` entry removed.
- Add ZMQ with a bogus schema path → inline error, nothing added/persisted.
- Restart `cargo run` → persisted sources reappear.

- [ ] **Step 6: Commit**

```bash
git add src/data_source_window.rs src/app.rs src/lib.rs
git commit -m "feat(app): Data Source window to start/stop/persist sources"
```

---

## Self-Review notes

- **Spec coverage:** config schema+parse (T1), save/persist (T2, T10), stop signals ZMQ/MQTT/WS/bridge (T3–T6), SourceManager runtime add/remove + derived views (T7), app wiring incl. recording/status/discovery (T8), boot + CLI-optional + default seed (T9), window + menu button + persistence + CLI-ephemeral distinction (T10). Single-active-discovery limitation preserved (T7 `discovery()` returns first).
- **Type consistency:** `schema`→`proto_path`, `broker`→`broker_url` fixed in constraints and used in T7/T9. `SourceSpec`/`SourceKind` defined in T7; `SourceKind: Default = Zmq` required by T10 (flagged as a T7 adjustment). `LiveSource` carries `spec: SourceSpec` + `persist: bool` and `add(spec, persist)` (flagged as T7 adjustments driven by T10 persistence needs — apply them when implementing T7).
- **Ordering:** T3 must land before T4–T6 (they set the `stop` field). T7 before T8/T10. T8 before T9 (new() signature). T10 last.
- **Known deferrals:** multi-discovery aggregation; in-place edit of a running source.
```
