# External Source Protocol — Bridge Adapter Guide

This document specifies the wire contract between **datavis** and an external
"bridge" adapter process. It contains everything a third party needs to write
a proprietary adapter without touching any datavis source code.

---

## License — closed-source adapters are permitted

datavis is licensed under **GPL-3.0**. The GPL normally requires derivative
works to be distributed under the same licence. However, a bridge adapter is a
**separate process** that communicates with datavis over a pipe; it does not
link against any datavis library or include any of its source code. The
subprocess relationship is therefore *not* a derivative work under the GPL, and
your organisation may keep the adapter source closed.

---

## Stream structure

A bridge writes to its **stdout**. The stream is divided into two parts:

1. A **preamble** — written once at the start of the stream.
2. A sequence of **frames** — each carrying a serialised `Batch` message.

### Preamble

```
┌─────────────────────────────┬───────────────┐
│  magic  (4 bytes, fixed)    │ version (1 B) │
│  "DVS\x01"  (0x44 56 53 01) │  0x01         │
└─────────────────────────────┴───────────────┘
```

- The four magic bytes are the ASCII string `DVS` followed by the byte `0x01`.
- The version byte is currently `1`.
- Write these five bytes exactly once before any frames.

If the preamble is missing or the version is unrecognised, datavis treats the
mismatch as a **permanent protocol error** and will **not** restart the adapter.

### Frame

Each frame immediately follows the previous one (no separators between frames):

```
┌──────────────────────────────┬───────────────────────────────────┐
│  body_len  (u32, LE, 4 B)   │  body  (body_len bytes, Protobuf) │
└──────────────────────────────┴───────────────────────────────────┘
```

- `body_len` is a **4-byte unsigned little-endian** integer.
- Frames whose `body_len` exceeds **16 MiB (16,777,216 bytes)** are rejected
  as a guard against stream de-sync; datavis will kill and restart the adapter.
- `body` is a serialised `Batch` Protobuf message (schema below).

---

## Protobuf schema — `Batch`

datavis has the schema compiled in. Adapters do **not** ship a `.proto` file.
The canonical schema is:

```proto
syntax = "proto3";
package datavis.bridge;

message Batch  { repeated Column cols = 1; }

message Column {
    string topic = 1;
    repeated sfixed64 t_ns = 2;                 // per-column timestamps, ns since Unix epoch
    oneof values { DoubleCol doubles = 3; Sint64Col ints = 4; StringCol strings = 5; }
}

message DoubleCol { repeated double   v = 1; }
message Sint64Col { repeated sfixed64 v = 1; }
message StringCol { repeated string   v = 1; }
```

### Rules

- **`t_ns` length must equal the value column's length.** If `Column.values`
  carries a `DoubleCol` with 10 elements, then `t_ns` must also have exactly 10
  elements. datavis will drop malformed columns.
- **Exactly one `oneof` variant per column.** A `Column` with no `values` set,
  or with a variant that does not match the channel's declared `sample_type`,
  will be skipped by the router.
- **Different columns may have different lengths.** A single `Batch` can mix
  columns sampled at different rates (e.g. 10 accelerometer readings alongside
  1 status string).

---

## Channel mapping

Each `Column.topic` must match a `[channels."…"]` entry in the datavis
configuration that declares a `topic` and a `sample_type`:

| `sample_type` | Expected `Column.values` variant |
|---------------|----------------------------------|
| `float`       | `DoubleCol`                      |
| `int`         | `Sint64Col`                      |
| `text`        | `StringCol`                      |

Topics must be **unique across all sources** registered in the config. Columns
whose topic is not found in the registry are silently dropped.

Example config entries for the three channel types:

```toml
[channels."accel"]
topic       = "accel"
sample_type = "float"

[channels."state"]
topic       = "state"
sample_type = "int"

[channels."log"]
topic       = "log"
sample_type = "text"
```

---

## Configuring a bridge in `config.toml`

Declare each adapter as a `[[sources.bridge]]` table:

```toml
[[sources.bridge]]
name    = "vendor-x"          # shown in the status bar and logs
command = "/opt/vendor-x/adapter"
args    = ["--device", "/dev/ttyUSB0"]   # optional; defaults to []

[[sources.bridge]]
name    = "vendor-y"
command = "vendor-y-adapter"  # resolved via PATH
```

| Field     | Type            | Required | Description                                 |
|-----------|-----------------|----------|---------------------------------------------|
| `name`    | string          | yes      | Human-facing label for logs and the UI      |
| `command` | string          | yes      | Path or PATH-resolvable name of the binary  |
| `args`    | array of string | no       | Command-line arguments; defaults to `[]`    |

---

## Lifecycle

1. **Spawn.** datavis spawns `command` with `args`. The child's **stdin** is
   closed (null). The child's **stdout** is the data pipe. The child's
   **stderr** is captured and forwarded to the datavis log, prefixed with the
   source name.

2. **Run.** datavis reads the preamble, then reads frames in a loop. Each frame
   is decoded and routed to the matching channels.

3. **Restart on exit.** If the child exits for any reason other than a protocol
   error, datavis waits and then respawns it with **exponential backoff**
   starting at 250 ms and capping at 5 s. A run that successfully delivers at
   least one frame resets the backoff to 250 ms on the next restart.

4. **Permanent stop on bad preamble / unknown version.** A bad magic or
   unrecognised version byte causes datavis to log the error, stop the source,
   and **not** restart it. Fix the adapter binary and restart datavis.

5. **Clean shutdown.** When datavis exits it kills the child process.

---

## Reference adapter

A minimal working adapter is at [`examples/echo_bridge.rs`](../examples/echo_bridge.rs).
It writes the preamble once and then emits a single `Batch` containing three
columns at different sample rates and types. Run it standalone to inspect the
raw bytes:

```sh
cargo run --example echo_bridge | xxd | head
```

A real adapter replaces the hard-coded samples with data from a proprietary
transport but keeps the same framing. The example is the complete wire contract
in executable form.
