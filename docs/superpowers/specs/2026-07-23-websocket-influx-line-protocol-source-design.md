# WebSocket Influx Line-Protocol Source — Design

**Date:** 2026-07-23
**Status:** Approved for planning

## Goal

Add a new live data input: a WebSocket server that accepts InfluxDB line
protocol from connecting clients and feeds the scalar values into the app
through the existing `DataSource` interface. Channels auto-discover from the
incoming stream, exactly as MQTT does.

## Background

The ingest layer was unified behind a `DataSource` trait (`src/ingest/source.rs`):
a source is constructed with its config + the channel registry, then `spawn`ed
against the shared store, returning a uniform `SourceHandle`. MQTT-shaped
sources reuse `ScalarIngest` (`src/ingest/scalar.rs`), which per received
message does discovery, dynamic-proto recording, topic routing, and typed store
writes. The WebSocket source is MQTT-shaped and reuses this machinery in full.

## Non-Goals

- No InfluxDB HTTP `/write` endpoint, no `?precision=` query negotiation.
  Timestamps are interpreted as nanoseconds (the Influx default).
- No outbound WebSocket client mode; this is a server that clients push to.
- No TLS / `wss://`; plain `ws://` only.
- No authentication.
- No InfluxQL, Flux, or query support — write path only.

## Architecture

A new `WsSource` implements `DataSource`. It is MQTT-shaped:

- It owns a `topic_map: Arc<MqttTopicMap>` seeded from channels whose
  `mqtt_topic` matches an incoming topic (reusing the existing discovery and
  drag-to-bind plumbing — no new registry concept).
- On `spawn` it starts a TCP accept thread bound to the configured address.
  Each accepted TCP connection is upgraded to WebSocket and served on its own
  thread, so multiple producers may connect concurrently.
- Per received text frame: split into lines, parse each line, and for every
  `(topic, payload, ts)` produced call the shared
  `ScalarIngest::on_message(topic, payload, ts)` — the same recorder,
  discovery, store-routing path MQTT uses.

Because the returned `SourceHandle` carries
`discovery: Some(Discovery { discovered, topic_map })`, the sidebar picker,
live-value display, drag-to-bind, and dynamic-proto recording all work with no
additional code. The status indicator aggregates conn_state across all sources
(LIVE if any source is live), so the WebSocket source participates unchanged.

## Topic Mapping

One Influx line carries a measurement, tags, one or more fields, and an
optional timestamp. Each field becomes its own channel topic:

```
measurement/<tagkey=tagval>/…/field
```

- Tags are sorted by key (Influx already emits them sorted; we sort defensively)
  so the topic is a deterministic series identity.
- A line with no tags maps to `measurement/field`.
- A line with multiple fields fans out to one topic per field, each carrying
  the same timestamp.

Example:

```
weather,location=us-midwest,zone=a temperature=82,humidity=71 1465839830100400200
```

produces:

```
weather/location=us-midwest/zone=a/temperature = 82   @ 1465839830100400200
weather/location=us-midwest/zone=a/humidity    = 71   @ 1465839830100400200
```

## Field Value Normalization

`ScalarIngest` parses the payload string according to the bound channel's
declared `type` (Float/Int/Bool/Text). The parser strips Influx type syntax so
payloads parse cleanly regardless of the channel type:

| Influx literal | Payload passed on |
|----------------|-------------------|
| `82`           | `82`              |
| `82.0`         | `82.0`            |
| `82i`          | `82` (trailing `i` stripped) |
| `82u`          | `82` (trailing `u` stripped) |
| `"text"`       | `text` (surrounding quotes stripped, inner `\"` unescaped) |
| `t`/`T`/`true`/`True`/`TRUE` | `true` |
| `f`/`F`/`false`/`False`/`FALSE` | `false` |

Normalization is unconditional in the parser; the channel's declared type still
governs the final parse in `ScalarIngest` (an unbound topic is only discovered,
not parsed).

## Timestamp Handling

- If the line ends with an integer timestamp, it is used as nanoseconds.
- If absent, the parser is given a fallback `now` (from `crate::types::now_ns()`
  at read time) and uses it.
- The parser takes `now` as a parameter so it is pure and deterministic under
  test.

## Connection Lifecycle & conn_state

- The accept thread loops on `TcpListener::accept`. Each client is handled on
  its own thread (handshake + read loop).
- A shared client counter (`Arc<AtomicUsize>`) tracks live connections:
  first connection transitions `conn_state` to `LIVE`; when the count returns
  to 0 it transitions back to `CONNECTING`.
- A handshake or read error closes only that one connection: decrement the
  counter, log via `eprintln!`, keep listening.
- Bind failure at spawn logs via `eprintln!` and the accept thread exits
  (mirrors the ZMQ "running without live data" behavior). The `SourceHandle` is
  still returned so the app runs. `conn_state` stays `CONNECTING`.
- No `TIMEOUT` state (consistent with MQTT, whose blocking loop also never sets
  it).

## Components

### `src/ingest/lineproto.rs` (new)

Pure parser, no I/O.

- `parse_line(line: &str, now: i64) -> Vec<(String, String, i64)>`
  Returns `(topic, payload, ts)` tuples for one line; empty vec for a
  blank line, a comment (`#…`), or a malformed line.
- Handles Influx escaping: in the measurement, `\ ` and `\,`; in tag keys,
  tag values, and field keys, `\ `, `\,`, and `\=`; in quoted string field
  values, `\"`. Unescape when building topic segments and payloads.
- Tag sorting, field fan-out, field-literal normalization, optional trailing
  timestamp.
- A malformed line does not panic and does not abort sibling lines in the same
  frame; it yields an empty vec and is logged once by the caller.

### `src/ingest/websocket.rs` (new)

- `WsConfig { listen: String }` — bind address as `host:port`.
- `WsSource { config: WsConfig, topic_map: Arc<MqttTopicMap> }` with
  `WsSource::new(config, registry)` seeding `topic_map` from `mqtt_topic`
  channels (same construction as `MqttSource::new`).
- `impl DataSource`: `name()` → `"websocket"`; `spawn()` starts the accept
  thread, wires `ScalarIngest`, returns a `SourceHandle` with
  `discovery: Some(...)`, `schema_bytes: None`.
- Accept loop, per-connection read loop, client counter / conn_state
  transitions as above.

### `src/ingest/mod.rs` (modified)

- `pub mod lineproto;` and `pub mod websocket;`.
- Re-export `WsConfig`, `WsSource`.

### `src/main.rs` (modified)

- Parse `--ws-listen <addr>` (off by default, like `--mqtt-endpoint`).
- When present, build `WsSource::new(WsConfig { listen }, &channels)` and push
  its `spawn(store.clone())` handle onto `sources`.

## Dependency

Add `tungstenite` (synchronous WebSocket) to `Cargo.toml`. It matches the
existing `std::thread` blocking-loop model and pulls in no async runtime.
`tungstenite::accept(tcp_stream)` performs the server handshake; `read()`
returns `Message` frames; text frames carry line protocol.

## Error Handling Summary

| Condition | Behavior |
|-----------|----------|
| Bind fails | `eprintln!`, accept thread exits, app still runs, conn_state stays CONNECTING |
| Handshake fails on a connection | log, close that connection only |
| Read error / client disconnect | decrement counter, maybe → CONNECTING, keep listening |
| Malformed line | skipped, logged, sibling lines unaffected |
| Non-text frame (binary/ping/pong/close) | ping/pong/close handled by protocol; binary ignored |
| Unbound topic | discovered only (shown in picker), not written to store |

## Testing Strategy

### `lineproto.rs` unit tests

- Single field → one tuple with correct topic/payload/ts.
- Multiple fields → fan-out, shared timestamp.
- Tags sorted by key regardless of input order.
- Escaped comma / space / equals in measurement, tag, field key.
- Quoted string value with escaped quote → unescaped payload, quotes stripped.
- Int (`82i`) and uint (`82u`) → trailing letter stripped.
- Bool short (`t`/`f`) and long (`true`/`false`) → normalized `true`/`false`.
- Missing timestamp → tuples carry the supplied `now`.
- Present timestamp → tuples carry the parsed nanoseconds.
- Blank line and comment line → empty vec.
- Malformed line (no fields) → empty vec, no panic.

### `websocket.rs` integration tests

- Bind an ephemeral port (`127.0.0.1:0`), connect a `tungstenite` client, send
  a line-protocol text frame for a channel present in a test registry, assert
  the value lands in the store via `ScalarIngest` and the topic appears in
  `discovered`.
- Assert `conn_state` transitions to `LIVE` after a client connects.
```

