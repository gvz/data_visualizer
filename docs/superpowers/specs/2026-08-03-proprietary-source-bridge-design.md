# Proprietary Source Bridge — Design

**Date:** 2026-08-03
**Status:** Approved for planning

## Goal

Let organizations ingest data from **proprietary transports and encodings**
into datavis without disclosing their protocol code, while datavis itself
stays GPL-3.0.

## Problem & license constraint

datavis is GPL-3.0. Any proprietary code **linked** into the binary — even
via dynamic loading — is treated by the FSF as a derivative work and would
have to ship its source. That rules out an in-process plugin trait for
closed-source adapters.

The organization's secret spans **both** the transport (how it reaches the
data) and the encoding (proprietary framing/decode). Generic MQTT/WebSocket
transports therefore do not help: the transport itself is secret.

The solution is a process boundary. The org's adapter runs as a **separate
program** that handles everything secret and emits **plain samples** over a
documented pipe. A separate program communicating at arm's length over a
pipe links none of datavis's code, so it is not a derivative work and the
org keeps its source closed.

## Architecture

A new source type, `SubprocessSource`, implements the existing
`DataSource` trait (`src/ingest/source.rs`) — a sibling of `ZmqSource`. It
reuses the ZMQ decode machinery (`ProtoSchema` in `src/ingest/loader.rs`,
`TopicRouter` in `src/ingest/router.rs`) and the same `SourceHandle` shape
(`schema_bytes: Some`, `discovery: None`, channels declared upfront in
config exactly like ZMQ).

The only difference from ZMQ: instead of reading Protobuf frames off a ZMQ
SUB socket, `SubprocessSource` spawns the org's executable as a **child
process** and reads Protobuf frames off the child's **stdout**.

```
 org's proprietary binary            datavis (GPL)
 ┌────────────────────┐   stdout    ┌──────────────────────────┐
 │ secret transport   │  (pipe)     │ SubprocessSource thread  │
 │ secret decode      │ ──frames──► │  frame reader            │
 │ → plain samples    │             │  → TopicRouter (reused)  │
 │ emits Protobuf     │   stderr    │  → ChannelStore          │
 └────────────────────┘ ──────────► │  child lifecycle + log   │
                                     └──────────────────────────┘
```

## Wire protocol (child stdout → datavis)

A pipe is a raw byte stream, so the protocol is explicitly framed. Version
lives in a **stream preamble** read once per child launch (one child is one
protocol version; per-frame version bytes would be pure waste).

### Stream preamble (once, at child start)

```
┌──────────────┬──────────┐
│ magic        │ u8       │   magic = "DVS\x01" (4 bytes)
│ "DVS\x01"    │ version  │   version = 1 (current)
└──────────────┴──────────┘
```

The magic catches a non-bridge binary piped in by mistake. datavis reads
these 5 bytes first and validates them before reading any frame.

### Frame (repeated, after the preamble)

```
┌──────────┬───────────┬───────────────┬─────────────────────┐
│ u32 LE   │ u16 LE    │ topic bytes   │ Protobuf payload    │
│ body_len │ topic_len │ (UTF-8)       │ body_len-2-topic_len│
└──────────┴───────────┴───────────────┴─────────────────────┘
   4 bytes    2 bytes     topic_len          remainder
```

- `body_len` counts every byte after the 4-byte prefix. The reader reads 4
  bytes, then exactly `body_len` more — clean resync, no delimiter
  ambiguity in binary payloads.
- `topic` routes exactly like a ZMQ topic: fed as `(topic, payload)` into
  the existing `TopicRouter` → decode against the schema. No new decode
  logic.
- All integers little-endian, fixed layout — trivial to emit from
  C/C++/Python/Rust/Go.

### Limits & versioning

- **Frame cap:** reject any frame whose `body_len` exceeds a fixed maximum
  (16 MiB). Guards against a desynced/garbage stream exhausting memory.
- **Version:** current version is `1`. Future protocol changes bump the
  byte; datavis validates against a known-accepted set.

The frame layout is published as the **external source protocol** so bridge
authors have a stable contract, alongside a short reference bridge to copy.

## Config schema

Source *connections* today (ZMQ endpoint/schema, MQTT broker, WS listen)
are CLI args, while per-channel routing lives in `[channels]`. A bridge
needs more than an endpoint string (command + args + schema) and several
may run at once, which CLI args do not express cleanly. Bridges are
therefore declared in `config.toml` as an array of tables (mirroring the
existing `[[scripts.instances]]` pattern):

```toml
[[sources.bridge]]
name    = "vendor-x"                 # shown in status bar / logs
command = "/opt/vendor-x/adapter"    # the org's proprietary executable
args    = ["--device", "/dev/tty0"]  # optional; defaults to empty
schema  = "vendor-x.proto"           # proto_path, resolved relative to config.toml

# channels it feeds are declared the normal way, identical to ZMQ:
[channels."vx/accel_x"]
topic       = "accel"
proto_path  = "AccelBatch.samples.x"
ts_path     = "AccelBatch.samples.t_ns"
sample_type = "f64"
```

- New struct `BridgeConfig { name, command, args, schema }`, deserialized
  from `[[sources.bridge]]`. `main.rs` iterates the entries and spawns one
  `SubprocessSource` each, mirroring how it builds `ZmqSource`.
- Channel→topic routing reuses the **existing** `topic` + `proto_path`
  fields and `TopicRouter` — no new routing concept. Each bridge builds its
  router from the registry exactly as ZMQ does.
- **Documented constraint:** topics must be unique across coexisting
  sources. A bridge and ZMQ cannot both emit topic `accel` with different
  schemas — same registry, distinct transports.
- ZMQ/MQTT/WS remain on their current CLI args; no migration.

## Lifecycle, status & error handling

**Spawn:** `std::process::Command` with stdin null, stdout piped (frames),
stderr piped (logs). A thread reads stdout; stderr lines are logged with
the source name prefixed, so bridge authors can debug their adapter through
datavis's own log.

**Status** (drives the existing status-bar indicator via `conn_state`):

- `CONNECTING` at spawn and while restarting.
- `LIVE` on the first valid frame.
- `TIMEOUT` if no frame arrives within the heartbeat window — reuses ZMQ's
  existing timeout logic in `src/ingest/thread.rs`.

**Exit handling — two distinct paths:**

| Cause | Action |
|---|---|
| Child exits/crashes (any exit code) | **Warn** in log (`bridge "<name>" exited code N, restarting`), respawn with exponential backoff (250 ms → cap ~5 s); reset backoff after it stays LIVE for a sustained period. |
| Bad magic / unknown version at preamble | **Error** in log, **stop** — permanent mismatch, restart cannot fix it; mark the source failed. |
| Oversized / desynced frame mid-stream | **Warn**, kill child and restart (treated as transient corruption). |

**Recording:** forwards raw payloads to `record_sender` exactly as ZMQ
does; `schema_bytes: Some(schema)` populates the MCAP header. Bridge data
records and replays like any native source — no special-casing.

**No orphaned children:** datavis owns these children, so on app shutdown
(or source drop) it must **kill** them — a bridge must never outlive
datavis. The detached ZMQ thread has no stop signal today; `SubprocessSource`
holds the `Child` handle and kills it on shutdown. This is the one piece of
genuinely new ground versus ZMQ.

## Testing

**Unit — frame reader** (pure, no process):

- Well-formed preamble + N frames → correct `(topic, payload)` sequence.
- Bytes split across read boundaries mid-frame → reader buffers and
  resyncs.
- Oversized `body_len` → rejected.
- Bad magic → rejected; unknown version → rejected (distinct handling:
  permanent vs transient).

**Unit — config:** deserialize `[[sources.bridge]]` TOML → `BridgeConfig`,
including the missing-`args` default (empty).

**Integration — real child:** ship a tiny **reference bridge** binary in
the crate (e.g. `examples/echo_bridge.rs`) that writes the preamble plus a
few Protobuf frames. It doubles as the copy-paste reference for the docs.
Tests spawn `SubprocessSource` against it and assert:

- Samples land in the `ChannelStore` with correct values.
- A child that exits → respawn observed (status returns to LIVE).
- A child emitting bad magic → source marked failed, **no** restart loop.

**Lifecycle — no orphans:** spawn a long-sleeping child, drop the source,
assert the child PID is gone (kill-on-shutdown works).

## Deliverables

1. `SubprocessSource` in `src/ingest/` implementing `DataSource`, reusing
   `ProtoSchema` + `TopicRouter`.
2. Frame reader with preamble/version validation and the 16 MiB cap.
3. `BridgeConfig` + `[[sources.bridge]]` deserialization; `main.rs` wiring.
4. Child lifecycle: backoff restart, stderr→log, status, kill-on-shutdown.
5. `examples/echo_bridge.rs` reference bridge.
6. Docs: the external source protocol (preamble + frame layout + version
   policy + topic-uniqueness constraint) and a "how to add a proprietary
   source" guide.

## Out of scope (YAGNI)

- In-process / dynamically-loaded native plugins (GPL derivative-work
  problem; would need relicensing or a linking exception).
- Startup schema handshake (schema comes from the config `schema` path,
  matching ZMQ).
- Runtime topic discovery / drag-to-bind for bridges (`discovery: None`;
  channels are declared upfront like ZMQ).
- Migrating ZMQ/MQTT/WS from CLI args to config sections.
- Bidirectional communication (datavis → child); the pipe is one-way.
