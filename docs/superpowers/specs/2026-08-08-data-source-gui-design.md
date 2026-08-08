# Data Source GUI — Design

**Status:** approved design, ready for implementation planning
**Date:** 2026-08-08

## Goal

Add a GUI to view, start, and stop data sources at runtime — ZMQ, MQTT,
WebSocket, and third-party bridge subprocesses. The GUI lives in its own OS
window opened from a "Data Source" button in the top menu bar, next to File.
Sources configured through the GUI are written to `config.toml` and restored on
the next launch. CLI flags keep working but become optional: `datavis` with no
flags launches clean, and every source can be configured from the GUI instead.

## Background / current state

Sources are constructed once in `main.rs` from CLI flags (`--endpoint`,
`--schema`, `--mqtt-endpoint`, `--ws-listen`) plus the `[[sources.bridge]]`
config section, spawned, and flattened into `DerivedIngest` fields on
`DataVisApp`. There is no runtime add/remove:

- `DerivedIngest::from_handles` collapses `Vec<SourceHandle>` into loose fields.
  `conn_states` and `record_sender_slots` are `Vec` (append-friendly), but
  `mqtt_topics`, `mqtt_topic_map`, and `ingest_schema_bytes` take the **first**
  provider — a single-source assumption.
- `SourceHandle` has no stop signal except `child_guard` (bridges only). The
  ZMQ/MQTT/WS run-loops loop forever with no shutdown path.
- Only bridges are represented in config (`[[sources.bridge]]`:
  `{ name, command, args }`). ZMQ/MQTT/WS are CLI-only, not persisted.
- Config writes preserve unrelated sections via `toml_edit`: see
  `LayoutConfig::merge_into` / `save`, which rewrites only layout-owned keys and
  keeps `[channels]`/`[defaults]`/comments verbatim. The new sources writer
  follows this exact pattern.

Source construction entry points that stay the API for the manager:
`ZmqSource::build(IngestConfig, &ChannelRegistry) -> Result<Self>`,
`MqttSource::new(MqttConfig{broker_url, client_id}, &ChannelRegistry)`,
`WsSource::new(WsConfig{listen}, &ChannelRegistry)`,
`bridge::SubprocessSource::new(BridgeConfig, &ChannelRegistry)`, each followed by
`Box::new(src).spawn(store) -> SourceHandle`.

## Design

### 1. Config schema: `[sources]`

Extend config with a `[sources]` table holding one array per kind. Bridges are
unchanged; the three transport kinds are new:

```toml
[[sources.zmq]]
endpoint = "tcp://localhost:5555"
schema   = "schema.proto"

[[sources.mqtt]]
broker    = "localhost:1883"
client_id = "datavis"

[[sources.websocket]]
listen = "0.0.0.0:9001"

[[sources.bridge]]          # existing, unchanged
name    = "acme-adapter"
command = "/opt/acme/adapter"
args    = ["--fast"]
```

A new `SourcesConfig` type (in `src/config/sources.rs`) parses all four arrays:

```rust
pub struct SourcesConfig {
    pub zmq:       Vec<ZmqSourceConfig>,       // { endpoint: String, schema: String }
    pub mqtt:      Vec<MqttSourceConfig>,      // { broker: String, client_id: String }
    pub websocket: Vec<WsSourceConfig>,        // { listen: String }
    pub bridge:    Vec<bridge::BridgeConfig>,  // reuse existing type
}
```

- `SourcesConfig::from_toml_str(&str) -> Result<Self>` (absent section → all
  empty). Reuses `BridgeConfig::list_from_toml_str` semantics for the bridge arm.
- `SourcesConfig::save(&self, path)` rewrites only the `sources` table via
  `toml_edit`, preserving every other section and comments — same approach as
  `LayoutConfig::merge_into`. `client_id` defaults to `"datavis"` when omitted;
  `schema` has no default (required for ZMQ).

Field naming: config key is `schema` (matches the `--schema` flag and existing
docs), mapped to `IngestConfig.proto_path`. `broker` maps to
`MqttConfig.broker_url`.

### 2. Runtime source management: `SourceManager`

A new `SourceManager` (in `src/ingest/manager.rs`) owned by `DataVisApp`,
replacing the flattened `DerivedIngest` fields. It owns the live store handle and
the channel registry so it can spawn on demand.

```rust
pub struct LiveSource {
    pub id:    u64,              // monotonic, assigned by the manager
    pub kind:  SourceKind,       // Zmq | Mqtt | WebSocket | Bridge
    pub label: String,           // e.g. "zmq tcp://localhost:5555"
    handle:    SourceHandle,
}

pub enum SourceSpec {           // what the GUI/config produces
    Zmq { endpoint: String, schema: String },
    Mqtt { broker: String, client_id: String },
    WebSocket { listen: String },
    Bridge(BridgeConfig),
}

pub struct SourceManager {
    store:    Arc<dyn ChannelStore>,   // the live store
    channels: Arc<ChannelRegistry>,
    sources:  Vec<LiveSource>,
    next_id:  u64,
}
```

Methods:

- `SourceManager::new(store, channels) -> Self` — empty.
- `add(&mut self, spec: &SourceSpec) -> Result<u64>` — builds the matching source
  from `spec`, spawns it against `store.clone()`, pushes a `LiveSource`, returns
  its id. Build errors (e.g. missing `.proto` schema) return `Err` so the GUI can
  show them; nothing is added on failure.
- `remove(&mut self, id: u64)` — signals the source's stop handle, drops the
  `LiveSource`. Idempotent for unknown ids.
- Derived views computed on demand from `sources` (replacing frozen fields):
  - `conn_states(&self) -> Vec<Arc<AtomicU8>>`
  - `record_sender_slots(&self) -> Vec<Arc<Mutex<Option<Sender<RecordMsg>>>>>`
  - `schema_bytes(&self) -> &[u8]` — first source that has any (ZMQ).
  - `discovery(&self) -> Option<(&Arc<Mutex<BTreeMap<String,String>>>, &Arc<MqttTopicMap>)>`
    — first **active** discovery-capable source (MQTT/WS).
  - `iter(&self) -> impl Iterator<Item = &LiveSource>` for the GUI table.

`DataVisApp` calls these where it currently reads the frozen fields:
`aggregate_conn_state`, recording start/stop (installing senders into every
slot), the sidebar discovery snapshot, and MCAP header schema bytes. Because
these become method calls over the current vec, they stay correct as sources are
added/removed. Recording that is already active installs its sender into
newly-added sources on the next `add` (the manager takes the current record
sender, if any, as an optional argument to `add`, or the app re-installs after
`add`); adding while recording is an edge case — the app re-runs its
"install sender into all slots" step after every `add`.

### 3. Teardown: stop signal on `SourceHandle`

Add `stop: Arc<AtomicBool>` to `SourceHandle`, set by `SourceManager::remove`.
Each run-loop observes it. Fold the existing bridge `child_guard` teardown into
the same field (the manager sets `stop`; the bridge's reader thread already
watches an `AtomicBool` — reuse it).

- **ZMQ** (`ingest/thread.rs`): the socket already has `set_rcvtimeo(1_000)` and
  an `EAGAIN` branch. Thread `stop` through `run_loop`/`connect_and_recv`; return
  cleanly from both loops when `stop` is set (checked each iteration and on
  `EAGAIN`).
- **MQTT** (`ingest/mqtt.rs`): `run_loop` owns the `Client`. Store a clone of the
  client in a shared slot (`Arc<Mutex<Option<Client>>>`, mirroring
  `ChildGuard::current`); `remove` sets `stop` and calls `client.disconnect()`,
  which ends `connection.iter()`. The loop also checks `stop` each event and
  returns without reconnecting.
- **WebSocket** (`ingest/websocket.rs`): set the `TcpListener` non-blocking (or a
  short read timeout) so `accept` returns periodically; check `stop` each poll
  and return. Existing per-connection reader threads end when the listener stops
  and the socket drops.

`SourceHandle` construction in all four sources sets `stop`; the manager holds
the only setter.

### 4. The window

A dedicated OS window via `ctx.show_viewport_deferred` with a
`ViewportId`, gated on a `data_source_window_open: bool` on `DataVisApp`. A
"Data Source" button (icon + label) is added to the menu bar in `menu_bar`,
immediately after the File menu, toggling that bool.

Window contents:

- **Active sources** — a table, one row per `manager.iter()`:
  `label` · kind · status dot (LIVE/CONNECTING/TIMEOUT from `conn_state`) ·
  **Stop** button (calls `manager.remove(id)` and rewrites `[sources]` on disk).
- **Add source** — a `SourceKind` selector (ZMQ / MQTT / WebSocket / Bridge)
  driving a small form held in an `AddSourceForm` struct on the app:
  - ZMQ: `endpoint` text, `schema` text + a "Browse…" `rfd` file picker.
  - MQTT: `broker` text, `client_id` text (default `datavis`).
  - WebSocket: `listen` text.
  - Bridge: `name`, `command` (+ Browse), `args` (space-split or one-per-line).
  A **Start** button validates non-empty required fields, calls `manager.add`,
  and on success appends the entry to `[sources]` on disk and clears the form.
  Build/spawn errors are shown inline in the window (a `last_error: Option<String>`).

The window uses `show_viewport_deferred` so it renders on its own; state it needs
(form fields, the manager, config path) lives on `DataVisApp` and is accessed
through the closure. Closing the window (OS close or the toggle) only hides it;
sources keep running.

### 5. CLI / boot

`main.rs`:

- Load `SourcesConfig` from `config.toml` alongside the existing channel/layout
  load. Create the `SourceManager` and `add` every configured source (these are
  the **persisted** set the GUI manages).
- CLI flags still spawn sources at boot, but as **ephemeral** additions that are
  **not** written to config — an override/one-off. `--demo` is unchanged.
- Remove the "ZMQ starts by default with `tcp://localhost:5555`" behavior: a
  bare launch with no config sources and no flags starts with **no** live
  sources, and the user adds them in the GUI. (The default `config.toml` template
  may seed a `[[sources.zmq]]` entry so out-of-the-box behavior is unchanged for
  users who keep the default config.)

The persisted set (`[sources]`) and the ephemeral CLI set are independent;
neither is required to launch.

## Data flow

1. Boot: `SourcesConfig::load` → `SourceManager::add` per entry → threads spawn
   against the shared `LiveStore`.
2. GUI Start: form → `SourceSpec` → `manager.add` (spawn live) → `SourcesConfig`
   updated + `save` (persist).
3. GUI Stop: `manager.remove(id)` (set `stop`, unblock socket, drop handle) →
   `SourcesConfig` entry removed + `save`.
4. Store, recording, status, and sidebar discovery read the manager's derived
   views each frame, so they reflect the current source set.

## Error handling

- Build failures (missing `.proto`, bad broker/listen string) surface as `Err`
  from `manager.add`; the window shows the message and nothing is spawned or
  persisted.
- A source that spawns but never connects shows CONNECTING/TIMEOUT via its
  `conn_state` dot — same semantics as today.
- Config write failure (`save`) is reported in the window but does not tear down
  the already-spawned live source; the two operations are independent and the
  error tells the user persistence failed.
- Malformed `[sources]` on load: log and skip the bad entry (per-kind), matching
  the existing bridge behavior (`config: ignoring malformed [[sources.bridge]]`).

## Testing

- `SourcesConfig::from_toml_str`: parses all four arrays; absent section → empty;
  missing `client_id` → `"datavis"`; malformed entry skipped.
- `SourcesConfig::save`: round-trips; preserves `[channels]`/`[defaults]`/
  comments (mirror `save_preserves_channels_and_comments`); add-then-save-then-
  load equals the in-memory set.
- `SourceManager`: `add` grows `conn_states`/`record_sender_slots`;
  `remove` shrinks them and fires the stop signal (assert the `AtomicBool` is
  set, like `from_handles_retains_child_guards`); `discovery()` returns the first
  active discovery source; unknown-id `remove` is a no-op.
- Stop signals: unit-test that setting `stop` causes each run-loop helper to
  return (where testable without a live socket — at minimum ZMQ's
  `connect_and_recv` exits on `stop` during `EAGAIN`).
- `SourceSpec` ↔ config mapping: `schema`→`proto_path`, `broker`→`broker_url`.

## Out of scope / known limitations

- **Multiple simultaneous discovery sources:** RESOLVED. Every MQTT/WebSocket
  source spawned by a `SourceManager` shares one `discovered` list and one
  `topic_map`, so the sidebar shows topics from all of them and a drag-to-bind
  routes on whichever source receives that topic. (Remaining narrow limitation:
  with two ZMQ sources carrying *different* proto schemas, only the first
  schema is embedded in an MCAP recording header, since `schema_bytes()` is
  first-non-empty.)
- No editing of a running source in place — stop and re-add to change fields.
- No reordering/priority UI; sources are independent.
- CLI-spawned ephemeral sources are not shown as editable rows distinct from
  persisted ones beyond their label (they appear in the Active table but Stop
  removes them live without touching config, since they are not in `[sources]`).
