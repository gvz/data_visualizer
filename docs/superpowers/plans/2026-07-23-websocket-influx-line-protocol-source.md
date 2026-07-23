# WebSocket Influx Line-Protocol Source Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a WebSocket server data input that receives InfluxDB line protocol and feeds scalar values into the app through the existing `DataSource` interface, with automatic channel discovery.

**Architecture:** A new `WsSource` implements `DataSource` (MQTT-shaped). On `spawn` it binds a TCP accept thread; each connection is upgraded to WebSocket and served on its own thread. Received text frames are split into lines, each parsed by a pure `lineproto` parser into `(topic, payload, ts)` tuples, then handed to the shared `ScalarIngest::on_message` — reusing discovery, dynamic-proto recording, topic routing, and typed store writes unchanged.

**Tech Stack:** Rust, `tungstenite` (synchronous WebSocket, no async runtime), existing `ScalarIngest`/`DataSource`/`SourceHandle` machinery.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-23-websocket-influx-line-protocol-source-design.md`.
- Dependency: `tungstenite = "0.21"` (synchronous API: `Message::Text(String)`, `read()`, `send()`; no async runtime, no TLS feature).
- Topic format: `measurement/<tagkey=tagval>/…/field`, tags sorted by key. No tags → `measurement/field`.
- Timestamps interpreted as nanoseconds; missing timestamp uses a supplied `now` fallback.
- conn_state uses existing constants `CONNECTING`/`LIVE` from `crate::ingest`; no `TIMEOUT` (matches MQTT).
- Source `name()` is `"websocket"`.
- The `SourceHandle` must carry `discovery: Some(Discovery { discovered, topic_map })` and `schema_bytes: None`.
- Commit messages: plain Conventional Commits. Do NOT add `Co-Authored-By`, `Claude-Session`, or any self-attribution/AI-identification trailer.
- Nix cache policy unchanged; do not touch flake substituter config.

---

### Task 1: Influx line-protocol parser (`lineproto.rs`)

**Files:**
- Create: `src/ingest/lineproto.rs`
- Modify: `src/ingest/mod.rs` (add `pub mod lineproto;`)
- Test: inline `#[cfg(test)] mod tests` in `src/ingest/lineproto.rs`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `pub fn parse_line(line: &str, now: i64) -> Vec<(String, String, i64)>` — returns `(topic, payload, ts)` tuples for one line; empty vec for blank/comment/malformed lines. Task 2 calls this per line.

- [ ] **Step 1: Create the parser file with a stub and the full test suite**

Create `src/ingest/lineproto.rs`:

```rust
/// Parse one InfluxDB line-protocol line into `(topic, payload, ts)` tuples.
///
/// One line carries a measurement, optional sorted tags, one or more fields,
/// and an optional trailing nanosecond timestamp. Each field becomes its own
/// topic `measurement/<tagkey=tagval>/…/field`. A blank line, a comment
/// (`#…`), or a malformed line yields an empty vec (never panics). `now` is
/// used as the timestamp when the line omits one.
pub fn parse_line(_line: &str, _now: i64) -> Vec<(String, String, i64)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_field_no_tags() {
        let out = parse_line("weather temperature=82 1000", 5);
        assert_eq!(out, vec![("weather/temperature".to_string(), "82".to_string(), 1000)]);
    }

    #[test]
    fn missing_timestamp_uses_now() {
        let out = parse_line("weather temperature=82", 5);
        assert_eq!(out, vec![("weather/temperature".to_string(), "82".to_string(), 5)]);
    }

    #[test]
    fn tags_included_and_sorted() {
        let out = parse_line("weather,zone=a,location=us temperature=82 1000", 0);
        assert_eq!(
            out,
            vec![(
                "weather/location=us/zone=a/temperature".to_string(),
                "82".to_string(),
                1000
            )]
        );
    }

    #[test]
    fn multiple_fields_fan_out_shared_ts() {
        let mut out = parse_line("weather temperature=82,humidity=71 1000", 0);
        out.sort();
        assert_eq!(
            out,
            vec![
                ("weather/humidity".to_string(), "71".to_string(), 1000),
                ("weather/temperature".to_string(), "82".to_string(), 1000),
            ]
        );
    }

    #[test]
    fn int_and_uint_suffix_stripped() {
        assert_eq!(parse_line("m a=82i 1", 0)[0].1, "82");
        assert_eq!(parse_line("m a=82u 1", 0)[0].1, "82");
        assert_eq!(parse_line("m a=-5i 1", 0)[0].1, "-5");
    }

    #[test]
    fn bool_normalized() {
        assert_eq!(parse_line("m a=t 1", 0)[0].1, "true");
        assert_eq!(parse_line("m a=T 1", 0)[0].1, "true");
        assert_eq!(parse_line("m a=true 1", 0)[0].1, "true");
        assert_eq!(parse_line("m a=f 1", 0)[0].1, "false");
        assert_eq!(parse_line("m a=FALSE 1", 0)[0].1, "false");
    }

    #[test]
    fn quoted_string_value_unquoted() {
        let out = parse_line(r#"m a="too hot" 1"#, 0);
        assert_eq!(out, vec![("m/a".to_string(), "too hot".to_string(), 1)]);
    }

    #[test]
    fn quoted_string_inner_escape() {
        let out = parse_line(r#"m a="say \"hi\"" 1"#, 0);
        assert_eq!(out[0].1, r#"say "hi""#);
    }

    #[test]
    fn escaped_space_in_measurement() {
        let out = parse_line(r"a\ b temperature=1 1", 0);
        assert_eq!(out, vec![("a b/temperature".to_string(), "1".to_string(), 1)]);
    }

    #[test]
    fn escaped_equals_in_tag_value() {
        let out = parse_line(r"m,k=a\=b v=1 1", 0);
        assert_eq!(out, vec![("m/k=a=b/v".to_string(), "1".to_string(), 1)]);
    }

    #[test]
    fn blank_and_comment_are_empty() {
        assert!(parse_line("", 0).is_empty());
        assert!(parse_line("   ", 0).is_empty());
        assert!(parse_line("# a comment", 0).is_empty());
    }

    #[test]
    fn malformed_no_fields_is_empty() {
        assert!(parse_line("justmeasurement", 0).is_empty());
    }

    #[test]
    fn non_integer_timestamp_rejects_line() {
        assert!(parse_line("m a=1 not_a_number", 0).is_empty());
    }
}
```

Add to `src/ingest/mod.rs`, in the module-declaration block (after `pub mod loader;`):

```rust
pub mod lineproto;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib lineproto`
Expected: FAIL — the stub returns empty, so e.g. `single_field_no_tags` fails with a left/right assertion mismatch (`[]` vs the expected tuple).

- [ ] **Step 3: Replace the stub with the real implementation**

Replace the `parse_line` stub (keep the doc comment and the `#[cfg(test)]` module) with:

```rust
pub fn parse_line(line: &str, now: i64) -> Vec<(String, String, i64)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Vec::new();
    }

    // Section split on unescaped, unquoted spaces: [measurement+tags, fields, ts?]
    let parts = split_unescaped(line, ' ');
    if parts.len() < 2 {
        return Vec::new();
    }

    let ts = if parts.len() >= 3 {
        match parts[2].parse::<i64>() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        }
    } else {
        now
    };

    // measurement + tags
    let mut meta = split_unescaped(&parts[0], ',');
    if meta.is_empty() || meta[0].is_empty() {
        return Vec::new();
    }
    let measurement = unescape(&meta.remove(0));
    let mut tags: Vec<(String, String)> = Vec::new();
    for kv in &meta {
        let Some((k, v)) = split_key_value(kv) else {
            return Vec::new();
        };
        tags.push((unescape(&k), unescape(&v)));
    }
    tags.sort_by(|a, b| a.0.cmp(&b.0));

    let mut prefix = measurement;
    for (k, v) in &tags {
        prefix.push('/');
        prefix.push_str(k);
        prefix.push('=');
        prefix.push_str(v);
    }

    // fields
    let fields = split_unescaped(&parts[1], ',');
    let mut out = Vec::new();
    for kv in &fields {
        let Some((k, v)) = split_key_value(kv) else {
            continue;
        };
        let key = unescape(&k);
        if key.is_empty() {
            continue;
        }
        out.push((format!("{prefix}/{key}"), normalize_value(&v), ts));
    }
    out
}

/// Split `s` on unescaped, unquoted occurrences of `delim`. A backslash escapes
/// the next char (the pair is kept verbatim for later `unescape`); double quotes
/// protect the delimiter (kept verbatim for `normalize_value`).
fn split_unescaped(s: &str, delim: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    let mut in_quote = false;
    for c in s.chars() {
        if escaped {
            cur.push('\\');
            cur.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_quote = !in_quote;
            cur.push(c);
        } else if c == delim && !in_quote {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if escaped {
        cur.push('\\');
    }
    out.push(cur);
    out
}

/// Split on the first unescaped, unquoted `=` into (key, value).
fn split_key_value(s: &str) -> Option<(String, String)> {
    let mut escaped = false;
    let mut in_quote = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => in_quote = !in_quote,
            '=' if !in_quote => return Some((s[..i].to_string(), s[i + 1..].to_string())),
            _ => {}
        }
    }
    None
}

/// Remove backslash escapes (`\x` -> `x`).
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

/// Strip Influx field-value type syntax so the payload parses by channel type.
fn normalize_value(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        return v[1..v.len() - 1].replace("\\\"", "\"");
    }
    match v {
        "t" | "T" | "true" | "True" | "TRUE" => return "true".to_string(),
        "f" | "F" | "false" | "False" | "FALSE" => return "false".to_string(),
        _ => {}
    }
    if let Some(stripped) = v.strip_suffix('i').or_else(|| v.strip_suffix('u')) {
        let digits = stripped.strip_prefix('-').unwrap_or(stripped);
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return stripped.to_string();
        }
    }
    v.to_string()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib lineproto`
Expected: PASS — all parser tests green.

- [ ] **Step 5: Commit**

```bash
git add src/ingest/lineproto.rs src/ingest/mod.rs
git commit -m "feat: influx line-protocol parser for websocket source"
```

---

### Task 2: WebSocket server source (`websocket.rs`)

**Files:**
- Modify: `Cargo.toml` (add `tungstenite = "0.21"`)
- Modify: `src/ingest/source.rs` (add `topic_map_from_registry` helper)
- Modify: `src/ingest/mqtt.rs:28-37` (`MqttSource::new` uses the helper)
- Modify: `src/ingest/mod.rs` (add `pub mod websocket;` and re-export)
- Create: `src/ingest/websocket.rs`
- Test: inline `#[cfg(test)] mod tests` in `src/ingest/websocket.rs`

**Interfaces:**
- Consumes: `crate::ingest::lineproto::parse_line` (Task 1); `crate::ingest::scalar::ScalarIngest::new(discovered, topic_map, store, record_sender)`; `crate::ingest::source::{DataSource, Discovery, SourceHandle}`; `crate::ingest::{CONNECTING, LIVE}`.
- Produces:
  - `pub fn topic_map_from_registry(registry: &ChannelRegistry) -> Arc<MqttTopicMap>` (in `source.rs`).
  - `pub struct WsConfig { pub listen: String }`.
  - `pub struct WsSource` with `pub fn new(config: WsConfig, registry: &ChannelRegistry) -> Self` and `impl DataSource`. Task 3 constructs and spawns it.

- [ ] **Step 1: Add the tungstenite dependency**

In `Cargo.toml`, under `[dependencies]` (after the `egui-phosphor` line):

```toml
tungstenite = "0.21"
```

- [ ] **Step 2: Add the shared topic-map helper and route `MqttSource::new` through it**

In `src/ingest/source.rs`, update the imports at the top so they read:

```rust
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex, RwLock};

use crossbeam_channel::Sender;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::MqttTopicMap;
use crate::record::RecordMsg;
use crate::store::ChannelStore;
use crate::types::{ChannelId, SampleType};
```

Then add this free function after the `Discovery` struct definition:

```rust
/// Seed a topic map from the registry's `mqtt_topic` channels. Shared by the
/// discovery-shaped sources (MQTT, WebSocket) so a dropped/known topic routes.
pub fn topic_map_from_registry(registry: &ChannelRegistry) -> Arc<MqttTopicMap> {
    let mut initial: HashMap<String, (ChannelId, SampleType)> = HashMap::new();
    for id in registry.iter_ids() {
        if let Some(mqtt_topic) = &registry.config(id).mqtt_topic {
            initial.insert(mqtt_topic.clone(), (id, registry.meta(id).sample_type));
        }
    }
    Arc::new(RwLock::new(initial))
}
```

In `src/ingest/mqtt.rs`, replace the body of `MqttSource::new` (lines 28-37) with:

```rust
    pub fn new(config: MqttConfig, registry: &ChannelRegistry) -> Self {
        Self { config, topic_map: crate::ingest::source::topic_map_from_registry(registry) }
    }
```

`mqtt.rs` no longer uses `HashMap`, `ChannelId`, or `SampleType` directly in `new`; leave the other imports (they are still used by `run_loop`/tests). If `cargo` warns about a now-unused import in `mqtt.rs`, remove only the specific unused one.

- [ ] **Step 3: Verify the refactor kept MQTT behavior**

Run: `cargo test --lib ingest::mqtt`
Expected: PASS — `topic_map_built_from_mqtt_channels` and the other mqtt tests still pass (proves the extracted helper is equivalent).

- [ ] **Step 4: Write the WebSocket source tests (fail first)**

Create `src/ingest/websocket.rs` containing ONLY the test module for now (the impl arrives in Step 6 — this step must fail to compile, proving the tests bind to the real API):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::source::topic_map_from_registry;
    use crate::ingest::{CONNECTING, LIVE};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::Sample;
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            "[channels.\"weather/temp\"]\nmqtt_topic = \"weather/temperature\"\ntype = \"float\"\n",
        )
        .unwrap()
    }

    #[test]
    fn ws_source_spawn_has_discovery_no_schema() {
        let reg = registry();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let src = WsSource::new(WsConfig { listen: "127.0.0.1:0".into() }, &reg);
        let handle = Box::new(src).spawn(store);
        assert_eq!(handle.name, "websocket");
        assert!(handle.discovery.is_some());
        assert!(handle.schema_bytes.is_none());
    }

    #[test]
    fn serve_client_routes_to_store_discovers_and_sets_live() {
        let reg = registry();
        let id = reg.iter_ids().next().unwrap();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let topic_map = topic_map_from_registry(&reg);
        let discovered = Arc::new(Mutex::new(BTreeMap::new()));
        let record_sender = Arc::new(Mutex::new(None));
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));
        let clients = Arc::new(AtomicUsize::new(0));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let (s_map, s_disc, s_store, s_rec, s_state, s_clients) = (
            topic_map.clone(),
            discovered.clone(),
            store.clone(),
            record_sender.clone(),
            conn_state.clone(),
            clients.clone(),
        );
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, s_map, s_disc, s_store, s_rec, s_state, s_clients);
        });

        let (mut ws, _resp) =
            tungstenite::connect(format!("ws://{addr}").as_str()).unwrap();
        ws.send(tungstenite::Message::Text(
            "weather temperature=82 1000".to_string(),
        ))
        .unwrap();

        // Poll until the sample lands (server processes on its thread).
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if store.latest(id) == Some((1000, Sample::Float(82.0))) {
                break;
            }
            assert!(Instant::now() < deadline, "sample never arrived in store");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(conn_state.load(Ordering::Relaxed), LIVE);
        assert_eq!(
            discovered.lock().unwrap().get("weather/temperature").map(String::as_str),
            Some("82")
        );
        // Dropping the client closes the socket; the server read loop then ends.
        drop(ws);
    }
}
```

- [ ] **Step 5: Run to verify it fails**

Run: `cargo test --lib ingest::websocket`
Expected: FAIL — compile error, `WsSource`, `WsConfig`, and `serve_client` are not defined (the module has only tests).

- [ ] **Step 6: Implement the WebSocket source**

Prepend the implementation above the test module in `src/ingest/websocket.rs`:

```rust
use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::MqttTopicMap;
use crate::ingest::lineproto::parse_line;
use crate::ingest::scalar::ScalarIngest;
use crate::ingest::source::{topic_map_from_registry, DataSource, Discovery, SourceHandle};
use crate::ingest::{CONNECTING, LIVE};
use crate::record::RecordMsg;
use crate::store::ChannelStore;

pub struct WsConfig {
    /// Bind address as "host:port", e.g. "0.0.0.0:9001".
    pub listen: String,
}

pub struct WsSource {
    config: WsConfig,
    pub(crate) topic_map: Arc<MqttTopicMap>,
}

impl WsSource {
    pub fn new(config: WsConfig, registry: &ChannelRegistry) -> Self {
        Self { config, topic_map: topic_map_from_registry(registry) }
    }
}

impl DataSource for WsSource {
    fn name(&self) -> &str {
        "websocket"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let discovered: Arc<Mutex<BTreeMap<String, String>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>> = Arc::new(Mutex::new(None));
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));

        let listen = self.config.listen.clone();
        let map = self.topic_map.clone();
        let disc = discovered.clone();
        let rec = record_sender.clone();
        let state = conn_state.clone();
        std::thread::spawn(move || {
            accept_loop(listen, map, disc, store, rec, state);
        });

        SourceHandle {
            name: "websocket".to_string(),
            conn_state,
            record_sender,
            discovery: Some(Discovery { discovered, topic_map: self.topic_map }),
            schema_bytes: None,
        }
    }
}

/// Bind and accept connections, one serving thread per client. A bind failure
/// logs and exits the thread (app keeps running, conn_state stays CONNECTING).
fn accept_loop(
    listen: String,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    conn_state: Arc<AtomicU8>,
) {
    let listener = match TcpListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("websocket: bind {listen} failed: {e}");
            return;
        }
    };
    let clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("websocket: accept failed: {e}");
                continue;
            }
        };
        let map = topic_map.clone();
        let disc = discovered.clone();
        let store = store.clone();
        let rec = record_sender.clone();
        let state = conn_state.clone();
        let clients = clients.clone();
        std::thread::spawn(move || {
            serve_client(stream, map, disc, store, rec, state, clients);
        });
    }
}

/// Handshake one connection, then read text frames until close/error. Each
/// frame is split into lines and routed through `ScalarIngest`. The first live
/// client sets conn_state LIVE; the last to leave restores CONNECTING.
#[allow(clippy::too_many_arguments)]
fn serve_client(
    stream: TcpStream,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    conn_state: Arc<AtomicU8>,
    clients: Arc<AtomicUsize>,
) {
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("websocket: handshake failed: {e}");
            return;
        }
    };
    if clients.fetch_add(1, Ordering::Relaxed) == 0 {
        conn_state.store(LIVE, Ordering::Relaxed);
    }

    let mut ingest = ScalarIngest::new(discovered, topic_map, store, record_sender);
    loop {
        match ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let now = crate::types::now_ns();
                for line in text.lines() {
                    for (topic, payload, ts) in parse_line(line, now) {
                        ingest.on_message(&topic, &payload, ts);
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {} // binary/ping/pong ignored; tungstenite auto-replies pings
            Err(e) => {
                eprintln!("websocket: read: {e}");
                break;
            }
        }
    }

    if clients.fetch_sub(1, Ordering::Relaxed) == 1 {
        conn_state.store(CONNECTING, Ordering::Relaxed);
    }
}
```

Add to `src/ingest/mod.rs`: `pub mod websocket;` in the module-declaration block, and extend the source re-export line so it reads:

```rust
pub use websocket::{WsConfig, WsSource};
```

- [ ] **Step 7: Run the WebSocket tests to verify they pass**

Run: `cargo test --lib ingest::websocket`
Expected: PASS — both tests green (spawn handle shape; and the end-to-end serve_client route + discovery + LIVE).

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/ingest/source.rs src/ingest/mqtt.rs src/ingest/mod.rs src/ingest/websocket.rs
git commit -m "feat: websocket server data source for influx line protocol"
```

---

### Task 3: Wire `--ws-listen` into `main.rs`

**Files:**
- Modify: `src/main.rs:8` (imports) and `src/main.rs:51-57` (after the MQTT block)

**Interfaces:**
- Consumes: `datavis::ingest::{WsConfig, WsSource}` and `DataSource::spawn` (Task 2); existing `arg_value` helper and `sources` vec in `main.rs`.
- Produces: nothing consumed by later tasks (terminal wiring).

- [ ] **Step 1: Add the import**

In `src/main.rs`, extend the ingest import (line 8) to:

```rust
use datavis::ingest::{
    DataSource, IngestConfig, MqttConfig, MqttSource, WsConfig, WsSource, ZmqSource,
};
```

- [ ] **Step 2: Build the WebSocket source when `--ws-listen` is given**

In `src/main.rs`, immediately after the MQTT block (the `if let Some(broker) = mqtt_endpoint { … }` ending at line 57) and before `let registry = PanelRegistry::with_builtins();`, add:

```rust
    if let Some(listen) = arg_value(&args, "--ws-listen") {
        let src = WsSource::new(WsConfig { listen }, &channels);
        sources.push(Box::new(src).spawn(store.clone()));
    }
```

- [ ] **Step 3: Build and run the full test suite**

Run: `cargo build`
Expected: compiles clean (no errors; warnings acceptable only if pre-existing).

Run: `cargo test`
Expected: PASS — the full suite is green, including the Task 1 and Task 2 additions.

- [ ] **Step 4: Manual smoke (optional, if a WebSocket client is available)**

Start the app against a free port and push a line, e.g. with Python:

```bash
cargo run -- --ws-listen 127.0.0.1:9001 &
python3 - <<'PY'
import websocket  # pip install websocket-client
ws = websocket.create_connection("ws://127.0.0.1:9001")
ws.send("weather,location=lab temperature=21.5")
ws.close()
PY
```

Expected: the topic `weather/location=lab/temperature` appears in the sidebar channel picker with live value `21.5`, and the status indicator reads LIVE. (Skip if no client is installed; the Task 2 integration test already proves the path.)

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: --ws-listen flag starts the websocket influx source"
```

---

## Notes for the Implementer

- The uncommitted working-tree changes present at plan time (light-mode label colors, conn_state aggregation, numeric unit suffix) are unrelated to this feature. Do NOT stage them in these commits — use the explicit `git add <paths>` lines exactly as written.
- `tungstenite` 0.21 API specifics: `Message::Text(String)`, `WebSocket::read()`, `WebSocket::send()`, `tungstenite::accept(stream)` for the server handshake, `tungstenite::connect(url)` for the client. If the resolved version differs and the API does not match, pin `tungstenite = "=0.21.0"` rather than adapting to a newer bytes-based API.
