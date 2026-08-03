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
shares `ZmqSource`'s `SourceHandle` shape (`schema_bytes: Some`,
`discovery: None`, channels declared upfront in config) and its registry
topic→channel lookup, but it does **not** reuse the reflective ZMQ decode
path (`ProtoSchema` + `TopicRouter` field-path traversal). Instead it
decodes a **fixed, built-in columnar schema** via a generated `prost` type
— no per-sample reflection (see Wire protocol).

The only difference from ZMQ in structure: instead of reading Protobuf
frames off a ZMQ SUB socket, `SubprocessSource` spawns the org's executable
as a **child process** and reads frames off the child's **stdout**.

```
 org's proprietary binary            datavis (GPL)
 ┌────────────────────┐   stdout    ┌──────────────────────────┐
 │ secret transport   │  (pipe)     │ SubprocessSource thread  │
 │ secret decode      │ ──frames──► │  frame reader            │
 │ → plain samples    │             │  → columnar decode       │
 │ emits Batch proto  │   stderr    │  → ChannelStore          │
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
┌──────────┬────────────────────────────┐
│ u32 LE   │ Protobuf `Batch` message   │
│ body_len │ (body_len bytes)           │
└──────────┴────────────────────────────┘
   4 bytes         body_len
```

- `body_len` counts the Protobuf bytes that follow the 4-byte prefix. The
  reader reads 4 bytes, then exactly `body_len` more — clean resync, no
  delimiter ambiguity in binary payloads.
- Integers little-endian, fixed prefix — trivial to emit from any language.

### Payload: fixed columnar `Batch` schema

The payload is **not** an arbitrary per-bridge message. It is a fixed
schema, built into datavis, decoded by a generated `prost` type — no
reflection. Columns are packed fixed-width arrays, decoded in bulk (near
memcpy) and appended straight to the store.

```proto
message Batch {
  repeated Column cols = 1;         // one or many channels per frame
}

message Column {
  string   topic = 1;               // routes to a channel via the registry
  repeated sfixed64 t_ns = 2;       // packed timestamps, length N (ns)
  oneof values {                    // exactly one; length must equal N
    DoubleCol  doubles = 3;
    Sint64Col  ints    = 4;
    StringCol  strings = 5;
  }
}

// `repeated` cannot sit directly in a `oneof`, so each variant is a
// single-field wrapper message. Bridge authors must follow this shape.
message DoubleCol { repeated double   v = 1; }   // packed
message Sint64Col { repeated sfixed64 v = 1; }   // packed
message StringCol { repeated string   v = 1; }
```

- **Per-column timestamps** ⇒ each channel carries its own timeline and
  length, so channels at **different sample rates** coexist in one frame.
  A bridge may batch many channels per frame or emit one column per frame.
- **Typed columns:** `doubles` for numeric channels, `ints` (`sfixed64`)
  for exact integer/state channels beyond 2^53, `strings` for log/text
  channels. Chosen once per column, not per sample — decode branches once.
- Decoded values are cast/checked against the channel's `sample_type` at
  ingest.
- `t_ns` length must equal the chosen column's length; the column type must
  be compatible with the channel's `sample_type` (see error handling).

### Limits & versioning

- **Frame cap:** reject any frame whose `body_len` exceeds a fixed maximum
  (16 MiB). Guards against a desynced/garbage stream exhausting memory.
- **Version:** current version is `1`. Future schema/protocol changes bump
  the byte; datavis validates against a known-accepted set.
- `schema_bytes` for the MCAP header is the **static** `Batch` descriptor,
  known at compile time — no per-bridge schema to serialize.

The preamble, frame layout, and `Batch` schema are published as the
**external source protocol** so bridge authors have a stable contract,
alongside a short reference bridge to copy.

## Config schema

Source *connections* today (ZMQ endpoint/schema, MQTT broker, WS listen)
are CLI args, while per-channel routing lives in `[channels]`. A bridge
needs more than an endpoint string (command + args) and several may run at
once, which CLI args do not express cleanly. Bridges are therefore declared
in `config.toml` as an array of tables (mirroring the existing
`[[scripts.instances]]` pattern):

```toml
[[sources.bridge]]
name    = "vendor-x"                 # shown in status bar / logs
command = "/opt/vendor-x/adapter"    # the org's proprietary executable
args    = ["--device", "/dev/tty0"]  # optional; defaults to empty

# channels it feeds are declared with just topic + sample_type — no
# proto_path / ts_path, because the payload schema is fixed:
[channels."vx/accel_x"]
topic       = "accel"                # matches Column.topic on the wire
sample_type = "f64"
```

- New struct `BridgeConfig { name, command, args }`, deserialized from
  `[[sources.bridge]]`. `main.rs` iterates the entries and spawns one
  `SubprocessSource` each, mirroring how it builds `ZmqSource`. No `schema`
  field — the payload schema is fixed and built in.
- Bridge channels drop `proto_path` / `ts_path` (there is no per-bridge
  proto to reference); they declare only `topic` (matched against
  `Column.topic`) and `sample_type`. Topic→channel resolution reuses the
  existing registry lookup.
- **Documented constraint:** topics must be unique across coexisting
  sources. A bridge and ZMQ cannot both emit topic `accel` — same registry,
  distinct transports.
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
| Column with `t_ns` length ≠ values length | **Warn** (`bridge "<name>" topic "<t>" length mismatch`), drop that column; keep the rest of the frame. |
| Column type incompatible with channel `sample_type` (e.g. `strings` into a numeric channel) | **Warn**, drop that column; keep the rest of the frame. |
| Column `topic` not in the registry | **Warn** once per unknown topic, drop the column. |

**Recording:** forwards the raw `Batch` frame bytes to `record_sender` as
ZMQ forwards its raw payloads; `schema_bytes` (the static `Batch`
descriptor) populates the MCAP header. Bridge data records and replays like
any native source — no special-casing.

**No orphaned children:** datavis owns these children, so on app shutdown
(or source drop) it must **kill** them — a bridge must never outlive
datavis. The detached ZMQ thread has no stop signal today; `SubprocessSource`
holds the `Child` handle and kills it on shutdown. This is the one piece of
genuinely new ground versus ZMQ.

## Testing

**Unit — frame reader** (pure, no process):

- Well-formed preamble + N frames → correct sequence of `Batch` messages.
- Bytes split across read boundaries mid-frame → reader buffers and
  resyncs.
- Oversized `body_len` → rejected.
- Bad magic → rejected; unknown version → rejected (distinct handling:
  permanent vs transient).

**Unit — columnar decode** (pure): a `Batch` with

- multiple columns at **different lengths** (different rates) → each routed
  to its channel with correct `(t_ns, value)` pairs.
- one column of each type (`doubles`, `ints`, `strings`) → correct values,
  cast to the channel `sample_type`.
- `t_ns`/values length mismatch → column dropped, siblings kept.
- type-incompatible column and unknown-topic column → dropped, siblings
  kept.

**Unit — config:** deserialize `[[sources.bridge]]` TOML → `BridgeConfig`,
including the missing-`args` default (empty).

**Integration — real child:** ship a tiny **reference bridge** binary in
the crate (e.g. `examples/echo_bridge.rs`) that writes the preamble plus a
few `Batch` frames (including two channels at different rates). It doubles
as the copy-paste reference for the docs.
Tests spawn `SubprocessSource` against it and assert:

- Samples land in the `ChannelStore` with correct values.
- A child that exits → respawn observed (status returns to LIVE).
- A child emitting bad magic → source marked failed, **no** restart loop.

**Lifecycle — no orphans:** spawn a long-sleeping child, drop the source,
assert the child PID is gone (kill-on-shutdown works).

## Deliverables

1. Fixed `Batch`/`Column` schema (`.proto` + generated `prost` type) built
   into datavis; static `schema_bytes` for MCAP.
2. `SubprocessSource` in `src/ingest/` implementing `DataSource`: columnar
   decode + registry topic lookup, casting columns to `sample_type`.
3. Frame reader with preamble/version validation and the 16 MiB cap.
4. `BridgeConfig` + `[[sources.bridge]]` deserialization; `main.rs` wiring.
5. Child lifecycle: backoff restart, stderr→log, status, kill-on-shutdown.
6. `examples/echo_bridge.rs` reference bridge (multi-rate, all column
   types).
7. Docs: the external source protocol (preamble + frame layout + `Batch`
   schema + version policy + topic-uniqueness constraint) and a "how to add
   a proprietary source" guide.

## Out of scope (YAGNI)

- In-process / dynamically-loaded native plugins (GPL derivative-work
  problem; would need relicensing or a linking exception).
- Startup schema handshake (the payload schema is fixed and built in, so
  there is nothing to negotiate).
- Arbitrary per-bridge Protobuf messages / reflective field-path decoding
  (the fixed columnar `Batch` schema replaces it — faster, less config).
- Runtime topic discovery / drag-to-bind for bridges (`discovery: None`;
  channels are declared upfront like ZMQ).
- Migrating ZMQ/MQTT/WS from CLI args to config sections.
- Bidirectional communication (datavis → child); the pipe is one-way.
