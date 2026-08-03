# Proprietary Source Bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let organizations ingest data from proprietary transports/encodings by
spawning their closed-source adapter as a child process that emits a fixed
columnar Protobuf `Batch` over its stdout, while datavis stays GPL-3.0.

**Architecture:** A new `SubprocessSource` implements the existing `DataSource`
trait. datavis spawns the org's executable, reads length-framed `Batch` messages
off its stdout, bulk-decodes packed columnar arrays (no reflection), and writes
them into the shared `ChannelStore`. The org's binary links none of datavis's
code (arm's-length pipe) so it is not a GPL derivative work.

**Tech Stack:** Rust, `prost` (generated struct via `build.rs`), `protox`
(protoc-less compile), `std::process`, existing `ChannelStore`/`ChannelRegistry`.

## Global Constraints

- License: datavis is GPL-3.0. The boundary MUST stay a separate process over a
  pipe — no in-process/dynamically-linked proprietary plugins.
- Wire preamble: magic `b"DVS\x01"` (4 bytes) + `u8` version, version `1`.
- Frame: `u32` LE `body_len`, then `body_len` bytes of Protobuf `Batch`.
- Frame cap: reject any frame with `body_len > 16 * 1024 * 1024` (16 MiB).
- Payload schema is FIXED and built in (fields/tags below); bridges do not ship a
  `.proto`. Bridge channels declare only `topic` + `sample_type` (no
  `proto_path`/`ts_path`).
- Per-column timestamps (`t_ns`, ns since Unix epoch, `sfixed64`); columns in one
  frame may differ in length (different sample rates).
- Bad magic / unknown version = permanent → stop, do not restart. Child
  exit/crash = transient → restart with exponential backoff (250 ms → cap 5 s).
- `cargo build` must succeed both with default features and
  `--no-default-features` (bridge is not behind the `scripting` feature).
- The `Batch` proto (exact fields/tags):
  ```proto
  syntax = "proto3";
  package datavis.bridge;
  message Batch  { repeated Column cols = 1; }
  message Column {
    string topic = 1;
    repeated sfixed64 t_ns = 2;
    oneof values { DoubleCol doubles = 3; Sint64Col ints = 4; StringCol strings = 5; }
  }
  message DoubleCol { repeated double   v = 1; }
  message Sint64Col { repeated sfixed64 v = 1; }
  message StringCol { repeated string   v = 1; }
  ```

---

### Task 1: Fixed `Batch` schema — proto, build.rs codegen, schema module

Generate the `Batch` struct from `proto/batch.proto` at build time with `protox`
(no `protoc`) + `prost-build`, and emit the `FileDescriptorSet` bytes for MCAP.

**Files:**
- Create: `proto/batch.proto`
- Create: `build.rs`
- Create: `src/ingest/bridge/mod.rs`
- Create: `src/ingest/bridge/schema.rs`
- Modify: `Cargo.toml` (add deps + build-deps)
- Modify: `src/ingest/mod.rs` (add `pub mod bridge;`)

**Interfaces:**
- Produces:
  - `datavis::ingest::bridge::schema::pb::{Batch, Column, DoubleCol, Sint64Col, StringCol}`
  - `datavis::ingest::bridge::schema::pb::column::Values` (oneof enum with
    variants `Doubles(DoubleCol)`, `Ints(Sint64Col)`, `Strings(StringCol)`)
  - `datavis::ingest::bridge::schema::batch_schema_bytes() -> &'static [u8]`

- [ ] **Step 1: Add dependencies to `Cargo.toml`**

Under `[dependencies]` add:
```toml
prost = "0.13"
```
Add a new `[build-dependencies]` section (place it after `[dev-dependencies]`):
```toml
[build-dependencies]
protox = "0.7"
prost = "0.13"
prost-build = "0.13"
```

- [ ] **Step 2: Create `proto/batch.proto`**

```proto
syntax = "proto3";
package datavis.bridge;

message Batch {
  repeated Column cols = 1;
}

message Column {
  string topic = 1;
  repeated sfixed64 t_ns = 2;
  oneof values {
    DoubleCol doubles = 3;
    Sint64Col ints = 4;
    StringCol strings = 5;
  }
}

message DoubleCol { repeated double   v = 1; }
message Sint64Col { repeated sfixed64 v = 1; }
message StringCol { repeated string   v = 1; }
```

- [ ] **Step 3: Create `build.rs`**

```rust
use std::path::PathBuf;

use prost::Message;

fn main() {
    // Compile the fixed bridge schema with protox (pure Rust, no protoc).
    let fds = protox::compile(["proto/batch.proto"], ["proto"])
        .expect("compiling proto/batch.proto");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // FileDescriptorSet bytes for the MCAP recording header.
    std::fs::write(out.join("batch.fds"), fds.encode_to_vec())
        .expect("writing batch.fds");

    // Generate the prost struct from the same descriptor set (no protoc).
    prost_build::Config::new()
        .compile_fds(fds)
        .expect("prost-build compile_fds");

    println!("cargo:rerun-if-changed=proto/batch.proto");
    println!("cargo:rerun-if-changed=build.rs");
}
```

- [ ] **Step 4: Create `src/ingest/bridge/schema.rs`**

```rust
//! The fixed columnar wire schema shared with external bridge processes.
//!
//! Generated at build time from `proto/batch.proto` (see `build.rs`). The
//! payload is decoded directly into these `prost` structs — no reflection.

/// Generated `prost` types for the `datavis.bridge` package.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/datavis.bridge.rs"));
}

/// Serialized `FileDescriptorSet` for the `Batch` schema, embedded into the
/// MCAP recording header (mirrors `ZmqSource::schema_bytes`).
pub fn batch_schema_bytes() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/batch.fds"))
}
```

- [ ] **Step 5: Create `src/ingest/bridge/mod.rs`**

```rust
//! Out-of-process ingest: spawn an organization's proprietary adapter and read
//! a fixed columnar Protobuf `Batch` off its stdout. See
//! `docs/superpowers/specs/2026-08-03-proprietary-source-bridge-design.md`.

pub mod schema;
```

- [ ] **Step 6: Register the module in `src/ingest/mod.rs`**

Add to the `pub mod` list (alongside `pub mod source;`):
```rust
pub mod bridge;
```

- [ ] **Step 7: Write the failing test**

Append to `src/ingest/bridge/schema.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn batch_round_trips_through_prost() {
        let batch = pb::Batch {
            cols: vec![pb::Column {
                topic: "accel".to_string(),
                t_ns: vec![1_000, 2_000],
                values: Some(pb::column::Values::Doubles(pb::DoubleCol {
                    v: vec![1.5, 2.5],
                })),
            }],
        };
        let bytes = batch.encode_to_vec();
        let decoded = pb::Batch::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.cols.len(), 1);
        assert_eq!(decoded.cols[0].topic, "accel");
        assert_eq!(decoded.cols[0].t_ns, vec![1_000, 2_000]);
        match &decoded.cols[0].values {
            Some(pb::column::Values::Doubles(d)) => assert_eq!(d.v, vec![1.5, 2.5]),
            other => panic!("wrong oneof variant: {other:?}"),
        }
    }

    #[test]
    fn schema_bytes_are_non_empty() {
        assert!(!batch_schema_bytes().is_empty());
    }
}
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p datavis bridge::schema`
Expected: PASS (2 tests). If the generated file name differs, check
`target/*/build/*/out/` for the emitted `.rs` and adjust the `include!` in
Step 4 to match the actual `<package>.rs` name.

- [ ] **Step 9: Verify both feature sets build**

Run: `cargo build && cargo build --no-default-features`
Expected: both succeed.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock build.rs proto/batch.proto src/ingest/bridge/ src/ingest/mod.rs
git commit -m "feat(ingest): fixed columnar Batch wire schema for source bridges"
```

---

### Task 2: Frame reader (preamble + length-framed reader)

Pure reader over any `std::io::Read`: validates the preamble, then yields frame
bodies, enforcing the 16 MiB cap. Error variants distinguish permanent
(bad preamble) from transient (oversized) from I/O.

**Files:**
- Create: `src/ingest/bridge/frame.rs`
- Modify: `src/ingest/bridge/mod.rs` (add `pub mod frame;`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  - `datavis::ingest::bridge::frame::{MAGIC, VERSION, MAX_FRAME_BYTES, FrameError, FrameReader}`
  - `FrameReader::new(r: R) -> Self`
  - `FrameReader::read_preamble(&mut self) -> Result<(), FrameError>`
  - `FrameReader::next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError>`
    (`Ok(None)` on clean EOF)

- [ ] **Step 1: Write the failing test**

Create `src/ingest/bridge/frame.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn framed(bodies: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        for b in bodies {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        out
    }

    #[test]
    fn reads_preamble_then_frames_then_eof() {
        let mut r = FrameReader::new(Cursor::new(framed(&[b"hello", b"world"])));
        r.read_preamble().unwrap();
        assert_eq!(r.next_frame().unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(r.next_frame().unwrap().as_deref(), Some(&b"world"[..]));
        assert_eq!(r.next_frame().unwrap(), None);
    }

    #[test]
    fn bad_magic_is_permanent() {
        let mut bytes = framed(&[b"x"]);
        bytes[0] = b'Z';
        let mut r = FrameReader::new(Cursor::new(bytes));
        assert!(matches!(r.read_preamble(), Err(FrameError::BadPreamble)));
    }

    #[test]
    fn unknown_version_is_permanent() {
        let mut bytes = framed(&[b"x"]);
        bytes[4] = 99;
        let mut r = FrameReader::new(Cursor::new(bytes));
        assert!(matches!(r.read_preamble(), Err(FrameError::BadPreamble)));
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());
        let mut r = FrameReader::new(Cursor::new(bytes));
        r.read_preamble().unwrap();
        assert!(matches!(r.next_frame(), Err(FrameError::Oversized(_))));
    }

    #[test]
    fn partial_length_prefix_is_io_error() {
        // Preamble + only 2 of the 4 length bytes → not a clean EOF.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(VERSION);
        bytes.extend_from_slice(&[0u8, 0u8]);
        let mut r = FrameReader::new(Cursor::new(bytes));
        r.read_preamble().unwrap();
        assert!(matches!(r.next_frame(), Err(FrameError::Io(_))));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis bridge::frame`
Expected: FAIL (does not compile — `FrameReader` undefined).

- [ ] **Step 3: Write the implementation**

Prepend to `src/ingest/bridge/frame.rs`:
```rust
use std::io::{ErrorKind, Read};

/// Stream magic: catches a non-bridge binary piped in by mistake.
pub const MAGIC: [u8; 4] = *b"DVS\x01";
/// Current wire protocol version.
pub const VERSION: u8 = 1;
/// Reject any frame larger than this; guards against a desynced stream.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Why a frame read failed. `BadPreamble` is permanent (do not restart the
/// child); `Oversized` is transient corruption (kill + restart); `Io` covers
/// a dead pipe / partial read (restart).
#[derive(Debug)]
pub enum FrameError {
    BadPreamble,
    Oversized(u32),
    Io(std::io::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::BadPreamble => write!(f, "bad stream preamble (magic/version)"),
            FrameError::Oversized(n) => write!(f, "frame length {n} exceeds cap"),
            FrameError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

/// Reads the preamble once, then length-prefixed frame bodies, from any
/// byte stream (a child's stdout in production; a `Cursor` in tests).
pub struct FrameReader<R: Read> {
    inner: R,
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read and validate the 5-byte preamble. Call exactly once, first.
    pub fn read_preamble(&mut self) -> Result<(), FrameError> {
        let mut buf = [0u8; 5];
        self.inner.read_exact(&mut buf).map_err(FrameError::Io)?;
        if buf[0..4] != MAGIC || buf[4] != VERSION {
            return Err(FrameError::BadPreamble);
        }
        Ok(())
    }

    /// Read the next frame body. `Ok(None)` marks a clean end of stream
    /// (the child closed stdout on a frame boundary).
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let mut len_buf = [0u8; 4];
        match self.read_full(&mut len_buf)? {
            0 => return Ok(None), // clean EOF on a frame boundary
            4 => {}
            _ => {
                return Err(FrameError::Io(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "truncated frame length prefix",
                )))
            }
        }
        let len = u32::from_le_bytes(len_buf);
        if len > MAX_FRAME_BYTES {
            return Err(FrameError::Oversized(len));
        }
        let mut body = vec![0u8; len as usize];
        self.inner.read_exact(&mut body).map_err(FrameError::Io)?;
        Ok(Some(body))
    }

    /// Read up to `buf.len()` bytes, tolerating a clean 0-byte EOF. Returns the
    /// number of bytes read (0 = EOF before any byte, `buf.len()` = full).
    fn read_full(&mut self, buf: &mut [u8]) -> Result<usize, FrameError> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.inner.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(FrameError::Io(e)),
            }
        }
        Ok(filled)
    }
}
```

- [ ] **Step 4: Register the module**

In `src/ingest/bridge/mod.rs` add:
```rust
pub mod frame;
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p datavis bridge::frame`
Expected: PASS (5 tests).

- [ ] **Step 6: Commit**

```bash
git add src/ingest/bridge/frame.rs src/ingest/bridge/mod.rs
git commit -m "feat(ingest): bridge frame reader with preamble + 16MiB cap"
```

---

### Task 3: Bridge router — topic map + columnar decode into the store

Build a `topic → channel` map from the registry (bridge channels have a `topic`
but no `proto_path`), and apply a decoded `Batch` to the store with per-column
bulk writes, EU scaling, and length/type/unknown-topic handling.

**Files:**
- Create: `src/ingest/bridge/router.rs`
- Modify: `src/ingest/bridge/mod.rs` (add `pub mod router;`)
- Modify: `src/ingest/decode.rs` (make `resolve_ts` reusable)

**Interfaces:**
- Consumes: `pb::{Batch, Column, column::Values}` (Task 1); `ChannelRegistry`,
  `ChannelStore`, `NumericVal`, `SampleType`, `ChannelId`.
- Produces:
  - `datavis::ingest::bridge::router::BridgeRouter`
  - `BridgeRouter::build(registry: &ChannelRegistry) -> BridgeRouter`
  - `BridgeRouter::apply(&self, batch: &pb::Batch, store: &dyn ChannelStore) -> usize`
    (returns the number of samples written)

- [ ] **Step 1: Make `resolve_ts` reusable**

In `src/ingest/decode.rs`, change the signature of `resolve_ts` from private to
crate-visible so the bridge reuses the identical "0 ⇒ stamp now" rule:
```rust
pub(crate) fn resolve_ts(ts: Option<i64>) -> i64 {
```
(Only the `fn resolve_ts` line changes — `fn` → `pub(crate) fn`.)

- [ ] **Step 2: Write the failing test**

Create `src/ingest/bridge/router.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::bridge::schema::pb;
    use crate::store::LiveStore;
    use crate::types::{ChannelSnapshot, TimeWindow};

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."accel"]
topic = "accel"
type = "float"
eu_scale = 2.0
eu_offset = 1.0

[channels."state"]
topic = "state"
type = "int"

[channels."log"]
topic = "log"
type = "text"
"#,
        )
        .unwrap()
    }

    fn col_doubles(topic: &str, t: Vec<i64>, v: Vec<f64>) -> pb::Column {
        pb::Column { topic: topic.into(), t_ns: t, values: Some(pb::column::Values::Doubles(pb::DoubleCol { v })) }
    }

    #[test]
    fn applies_scaled_doubles_and_routes_multiple_rates() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        // Two channels, different lengths (different rates) in one batch.
        let batch = pb::Batch {
            cols: vec![
                col_doubles("accel", vec![10, 20, 30], vec![1.0, 2.0, 3.0]),
                pb::Column {
                    topic: "state".into(),
                    t_ns: vec![10],
                    values: Some(pb::column::Values::Ints(pb::Sint64Col { v: vec![7] })),
                },
            ],
        };
        let n = router.apply(&batch, &store);
        assert_eq!(n, 4);

        let accel = reg.id("accel").unwrap();
        match store.snapshot(accel, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![10, 20, 30]);
                // EU: raw*2+1 → 3,5,7
                assert_eq!(vals, vec![3.0, 5.0, 7.0]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn routes_strings_to_text_channel() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        let batch = pb::Batch {
            cols: vec![pb::Column {
                topic: "log".into(),
                t_ns: vec![5, 6],
                values: Some(pb::column::Values::Strings(pb::StringCol {
                    v: vec!["a".into(), "b".into()],
                })),
            }],
        };
        assert_eq!(router.apply(&batch, &store), 2);
        let log = reg.id("log").unwrap();
        match store.snapshot(log, ALL) {
            ChannelSnapshot::Text { lines } => {
                assert_eq!(lines.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn length_mismatch_drops_column_keeps_siblings() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        let batch = pb::Batch {
            cols: vec![
                col_doubles("accel", vec![1, 2], vec![9.0]), // len mismatch → dropped
                col_doubles("accel", vec![3], vec![9.0]),    // ok → 1 sample
            ],
        };
        assert_eq!(router.apply(&batch, &store), 1);
    }

    #[test]
    fn type_incompatible_column_is_dropped() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        // strings into the numeric "accel" channel → dropped.
        let batch = pb::Batch {
            cols: vec![pb::Column {
                topic: "accel".into(),
                t_ns: vec![1],
                values: Some(pb::column::Values::Strings(pb::StringCol { v: vec!["x".into()] })),
            }],
        };
        assert_eq!(router.apply(&batch, &store), 0);
    }

    #[test]
    fn unknown_topic_is_dropped() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        let batch = pb::Batch { cols: vec![col_doubles("nope", vec![1], vec![1.0])] };
        assert_eq!(router.apply(&batch, &store), 0);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p datavis bridge::router`
Expected: FAIL (does not compile — `BridgeRouter` undefined).

- [ ] **Step 4: Write the implementation**

Prepend to `src/ingest/bridge/router.rs`:
```rust
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::config::ChannelRegistry;
use crate::ingest::bridge::schema::pb;
use crate::ingest::decode::resolve_ts;
use crate::store::ChannelStore;
use crate::types::{ChannelId, NumericVal, SampleType};

struct BridgeBinding {
    id: ChannelId,
    sample_type: SampleType,
    eu_scale: f64,
    eu_offset: f64,
    name: String,
}

/// Maps a bridge column `topic` to its channel and applies decoded `Batch`
/// messages to the store. A bridge channel is any registry channel with a
/// `topic` set; `topic` uniquely identifies one channel (unlike ZMQ, where
/// several channels share a topic via distinct `proto_path`s).
pub struct BridgeRouter {
    map: HashMap<String, BridgeBinding>,
    /// Unknown topics already logged, so the warning fires once each.
    warned: Mutex<HashSet<String>>,
}

impl BridgeRouter {
    pub fn build(registry: &ChannelRegistry) -> Self {
        let mut map: HashMap<String, BridgeBinding> = HashMap::new();
        for id in registry.iter_ids() {
            let cfg = registry.config(id);
            let Some(topic) = &cfg.topic else { continue };
            let meta = registry.meta(id);
            let binding = BridgeBinding {
                id,
                sample_type: meta.sample_type,
                eu_scale: cfg.eu_scale,
                eu_offset: cfg.eu_offset,
                name: meta.name.clone(),
            };
            if map.insert(topic.clone(), binding).is_some() {
                eprintln!("bridge: duplicate topic {topic:?}; keeping the last-declared channel");
            }
        }
        Self { map, warned: Mutex::new(HashSet::new()) }
    }

    /// Apply one decoded batch; returns the number of samples written.
    pub fn apply(&self, batch: &pb::Batch, store: &dyn ChannelStore) -> usize {
        let mut written = 0;
        for col in &batch.cols {
            let Some(b) = self.map.get(&col.topic) else {
                self.warn_unknown(&col.topic);
                continue;
            };
            written += self.apply_column(b, col, store);
        }
        written
    }

    fn apply_column(&self, b: &BridgeBinding, col: &pb::Column, store: &dyn ChannelStore) -> usize {
        let ts = &col.t_ns;
        match (&col.values, b.sample_type) {
            (Some(pb::column::Values::Strings(s)), SampleType::Text) => {
                if s.v.len() != ts.len() {
                    self.warn_len(b, ts.len(), s.v.len());
                    return 0;
                }
                for (t, line) in ts.iter().zip(&s.v) {
                    store.write_text(b.id, resolve_ts(Some(*t)), line.clone());
                }
                s.v.len()
            }
            (Some(pb::column::Values::Doubles(d)), st) if st != SampleType::Text => {
                if d.v.len() != ts.len() {
                    self.warn_len(b, ts.len(), d.v.len());
                    return 0;
                }
                for (t, raw) in ts.iter().zip(&d.v) {
                    store.write_numeric(b.id, resolve_ts(Some(*t)), scale(b, *raw));
                }
                d.v.len()
            }
            (Some(pb::column::Values::Ints(i)), st) if st != SampleType::Text => {
                if i.v.len() != ts.len() {
                    self.warn_len(b, ts.len(), i.v.len());
                    return 0;
                }
                for (t, raw) in ts.iter().zip(&i.v) {
                    store.write_numeric(b.id, resolve_ts(Some(*t)), scale(b, *raw as f64));
                }
                i.v.len()
            }
            (Some(_), _) => {
                eprintln!(
                    "bridge: column type incompatible with channel {:?} ({:?}); dropping",
                    b.name, b.sample_type
                );
                0
            }
            (None, _) => 0, // empty column: no value set
        }
    }

    fn warn_unknown(&self, topic: &str) {
        if let Ok(mut w) = self.warned.lock() {
            if w.insert(topic.to_string()) {
                eprintln!("bridge: unknown topic {topic:?} (not in config); dropping column");
            }
        }
    }

    fn warn_len(&self, b: &BridgeBinding, ts: usize, vals: usize) {
        eprintln!(
            "bridge: channel {:?} length mismatch (t_ns={ts}, values={vals}); dropping column",
            b.name
        );
    }
}

/// Apply the channel's engineering-unit transform and coerce to its type.
fn scale(b: &BridgeBinding, raw: f64) -> NumericVal {
    let v = raw * b.eu_scale + b.eu_offset;
    match b.sample_type {
        SampleType::Float => NumericVal::Float(v),
        SampleType::Int => NumericVal::Int(v as i64),
        SampleType::Bool => NumericVal::Bool(v != 0.0),
        SampleType::Text => NumericVal::Float(v), // unreachable: guarded by caller
    }
}
```

- [ ] **Step 5: Register the module**

In `src/ingest/bridge/mod.rs` add:
```rust
pub mod router;
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p datavis bridge::router`
Expected: PASS (5 tests).

- [ ] **Step 7: Commit**

```bash
git add src/ingest/bridge/router.rs src/ingest/bridge/mod.rs src/ingest/decode.rs
git commit -m "feat(ingest): bridge topic router + columnar decode into store"
```

---

### Task 4: Bridge config — `[[sources.bridge]]` deserialization

Parse zero or more bridge declarations out of the shared `config.toml`.

**Files:**
- Create: `src/ingest/bridge/config.rs`
- Modify: `src/ingest/bridge/mod.rs` (add `pub mod config;`)

**Interfaces:**
- Produces:
  - `datavis::ingest::bridge::config::BridgeConfig { name: String, command: String, args: Vec<String> }`
  - `BridgeConfig::list_from_toml_str(s: &str) -> anyhow::Result<Vec<BridgeConfig>>`

- [ ] **Step 1: Write the failing test**

Create `src/ingest/bridge/config.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_yields_empty() {
        let v = BridgeConfig::list_from_toml_str("default_window_s = 5.0\n").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn parses_bridges_with_default_args() {
        let v = BridgeConfig::list_from_toml_str(
            "[[sources.bridge]]\nname = \"vendor-x\"\ncommand = \"/opt/x/adapter\"\n\
             args = [\"--device\", \"/dev/tty0\"]\n\n\
             [[sources.bridge]]\nname = \"vendor-y\"\ncommand = \"adapter-y\"\n",
        )
        .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "vendor-x");
        assert_eq!(v[0].command, "/opt/x/adapter");
        assert_eq!(v[0].args, vec!["--device", "/dev/tty0"]);
        assert_eq!(v[1].name, "vendor-y");
        assert!(v[1].args.is_empty()); // default
    }

    #[test]
    fn ignores_other_sections() {
        let v = BridgeConfig::list_from_toml_str(
            "[channels.\"a\"]\ntype = \"float\"\n\n\
             [[sources.bridge]]\nname = \"b\"\ncommand = \"c\"\n",
        )
        .unwrap();
        assert_eq!(v.len(), 1);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis bridge::config`
Expected: FAIL (does not compile).

- [ ] **Step 3: Write the implementation**

Prepend to `src/ingest/bridge/config.rs`:
```rust
use anyhow::Context;
use serde::Deserialize;

/// One `[[sources.bridge]]` entry: an external adapter datavis spawns.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    /// Human-facing name shown in the status bar and logs.
    pub name: String,
    /// Path or PATH-resolvable name of the org's proprietary executable.
    pub command: String,
    /// Arguments passed to the executable; defaults to empty.
    #[serde(default)]
    pub args: Vec<String>,
}

// `[sources]` may grow other keys later, so this wrapper does not deny unknown
// fields; and the top-level doc ignores every section except `sources`.
#[derive(Deserialize)]
struct DocWrapper {
    sources: Option<RawSources>,
}

#[derive(Deserialize)]
struct RawSources {
    #[serde(default)]
    bridge: Vec<BridgeConfig>,
}

impl BridgeConfig {
    /// Extract the `[[sources.bridge]]` array from a full config.toml. An absent
    /// section yields an empty list.
    pub fn list_from_toml_str(s: &str) -> anyhow::Result<Vec<BridgeConfig>> {
        let doc: DocWrapper = toml::from_str(s).context("parsing [[sources.bridge]]")?;
        Ok(doc.sources.map(|s| s.bridge).unwrap_or_default())
    }
}
```

- [ ] **Step 4: Register the module**

In `src/ingest/bridge/mod.rs` add:
```rust
pub mod config;
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p datavis bridge::config`
Expected: PASS (3 tests).

- [ ] **Step 6: Commit**

```bash
git add src/ingest/bridge/config.rs src/ingest/bridge/mod.rs
git commit -m "feat(ingest): parse [[sources.bridge]] config entries"
```

---

### Task 5: Reference bridge example (`echo_bridge`)

A tiny standalone program that emits the preamble plus a few `Batch` frames
(two channels at different rates, all three column types). It is both the
integration-test fixture for Task 6 and the copy-paste reference for the docs.

**Files:**
- Create: `examples/echo_bridge.rs`

**Interfaces:**
- Consumes: `pb::{Batch, Column, ...}` and `frame::{MAGIC, VERSION}` (Tasks 1–2).
- Produces: an example binary runnable via `cargo run --example echo_bridge`,
  and the compiled path `target/<profile>/examples/echo_bridge` used by Task 6.

- [ ] **Step 1: Write the example**

Create `examples/echo_bridge.rs`:
```rust
//! Reference "bridge": the minimal program datavis spawns. A real adapter
//! replaces the hard-coded samples with data from its proprietary transport,
//! but the framing below is the entire contract.
//!
//! Run standalone to inspect the bytes:
//!   cargo run --example echo_bridge | xxd | head

use std::io::{self, Write};

use datavis::ingest::bridge::frame::{MAGIC, VERSION};
use datavis::ingest::bridge::schema::pb;
use prost::Message;

fn write_frame<W: Write>(w: &mut W, batch: &pb::Batch) -> io::Result<()> {
    let body = batch.encode_to_vec();
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    Ok(())
}

fn main() -> io::Result<()> {
    let mut out = io::stdout().lock();

    // Preamble: once, at the very start of the stream.
    out.write_all(&MAGIC)?;
    out.write_all(&[VERSION])?;

    // One frame carrying three channels at different rates / types.
    let batch = pb::Batch {
        cols: vec![
            pb::Column {
                topic: "accel".into(),
                t_ns: vec![1_000, 2_000, 3_000],
                values: Some(pb::column::Values::Doubles(pb::DoubleCol {
                    v: vec![0.1, 0.2, 0.3],
                })),
            },
            pb::Column {
                topic: "state".into(),
                t_ns: vec![1_500],
                values: Some(pb::column::Values::Ints(pb::Sint64Col { v: vec![1] })),
            },
            pb::Column {
                topic: "log".into(),
                t_ns: vec![1_500],
                values: Some(pb::column::Values::Strings(pb::StringCol {
                    v: vec!["armed".into()],
                })),
            },
        ],
    };
    write_frame(&mut out, &batch)?;
    out.flush()?;
    Ok(())
}
```

- [ ] **Step 2: Verify it builds and emits bytes**

Run: `cargo run --example echo_bridge | wc -c`
Expected: a non-zero byte count (builds and runs).

- [ ] **Step 3: Commit**

```bash
git add examples/echo_bridge.rs
git commit -m "feat(ingest): reference echo_bridge example"
```

---

### Task 6: `SubprocessSource` + `ChildGuard` (spawn, lifecycle, kill-on-shutdown)

The `DataSource` that spawns the child, reads frames, drives status, restarts on
transient failure, stops on a permanent preamble error, logs stderr, and kills
the child on shutdown.

**Files:**
- Modify: `src/ingest/source.rs` (add `ChildGuard`; add `child_guard` field to `SourceHandle`)
- Modify: `src/ingest/source.rs` tests, `src/app.rs`, `src/ingest/mqtt.rs`,
  `src/ingest/websocket.rs`, `src/ingest/mod.rs`, `src/script/mod.rs`
  (add `child_guard: None` at each existing `SourceHandle { .. }` site)
- Create: `src/ingest/bridge/source.rs`
- Modify: `src/ingest/bridge/mod.rs` (add `pub mod source;` and re-export)

**Interfaces:**
- Consumes: `BridgeConfig` (Task 4), `BridgeRouter` (Task 3), `FrameReader`/
  `FrameError` (Task 2), `batch_schema_bytes` + `pb::Batch` (Task 1), the
  `DataSource`/`SourceHandle` contract, `ChannelStore`, `RecordMsg`.
- Produces:
  - `datavis::ingest::source::ChildGuard`
  - `SourceHandle.child_guard: Option<ChildGuard>`
  - `datavis::ingest::bridge::SubprocessSource`
  - `SubprocessSource::new(cfg: BridgeConfig, registry: &ChannelRegistry) -> SubprocessSource`
  - `impl DataSource for SubprocessSource`

- [ ] **Step 1: Add `ChildGuard` and the `SourceHandle` field**

In `src/ingest/source.rs`, add imports at the top (merge with existing `use`s):
```rust
use std::sync::atomic::AtomicBool;
```
Add the guard type (place it just after the `SourceHandle` struct definition):
```rust
/// Kills a spawned child process when dropped. `SubprocessSource` puts one in
/// its `SourceHandle`; because the app owns every handle for its lifetime, the
/// child is killed when the app shuts down — a bridge never outlives datavis.
pub struct ChildGuard {
    /// Signals the reader/restart thread to stop respawning.
    pub(crate) stop: Arc<AtomicBool>,
    /// The currently-running child, if any, shared with the reader thread.
    pub(crate) current: Arc<Mutex<Option<std::process::Child>>>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = self.current.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
```
Add the field to `SourceHandle` (after `schema_bytes`):
```rust
    /// Present only for `SubprocessSource`: kills the child on shutdown. All
    /// other sources set this to `None`.
    pub child_guard: Option<ChildGuard>,
```

- [ ] **Step 2: Set `child_guard: None` at every existing construction site**

Add `child_guard: None,` to each `SourceHandle { .. }` literal:
- `src/ingest/source.rs` test at the `handle_holds_optional_capabilities` test.
- `src/ingest/mod.rs` (ZMQ `spawn`).
- `src/ingest/mqtt.rs` (`spawn`).
- `src/ingest/websocket.rs` (`spawn`).
- `src/script/mod.rs` (`spawn`).
- `src/app.rs` at all three sites (`make_handle` closure, `mqtt`, `zmq`).

- [ ] **Step 3: Verify the crate still compiles**

Run: `cargo build`
Expected: success (every `SourceHandle` literal now has `child_guard`). Fix any
site the compiler flags as missing the field.

- [ ] **Step 4: Write the failing integration test**

Create `src/ingest/bridge/source.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use crate::config::ChannelRegistry;
    use crate::ingest::source::DataSource;
    use crate::ingest::LIVE;
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::{ChannelSnapshot, TimeWindow};

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."accel"]
topic = "accel"
type = "float"

[channels."state"]
topic = "state"
type = "int"

[channels."log"]
topic = "log"
type = "text"
"#,
        )
        .unwrap()
    }

    // Path to the compiled `echo_bridge` example (built alongside tests).
    fn echo_bridge_bin() -> std::path::PathBuf {
        // target/<profile>/examples/echo_bridge, relative to the test binary.
        let mut dir = std::env::current_exe().unwrap();
        dir.pop(); // test binary name
        if dir.ends_with("deps") {
            dir.pop();
        }
        dir.join("examples").join(if cfg!(windows) { "echo_bridge.exe" } else { "echo_bridge" })
    }

    fn wait_until<F: Fn() -> bool>(f: F, within: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < within {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        f()
    }

    #[test]
    fn spawns_child_and_ingests_samples() {
        let reg = registry();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let cfg = crate::ingest::bridge::config::BridgeConfig {
            name: "echo".into(),
            command: echo_bridge_bin().to_string_lossy().into_owned(),
            args: vec![],
        };
        let src = SubprocessSource::new(cfg, &reg);
        let handle = Box::new(src).spawn(store.clone());

        let accel = reg.id("accel").unwrap();
        let got = wait_until(
            || matches!(store.snapshot(accel, ALL), ChannelSnapshot::Float { ts, .. } if ts.len() == 3),
            Duration::from_secs(5),
        );
        assert!(got, "expected 3 accel samples from the child");
        assert_eq!(handle.conn_state.load(Ordering::Relaxed), LIVE);
        assert!(handle.child_guard.is_some());
        assert!(handle.schema_bytes.as_ref().is_some_and(|b| !b.is_empty()));
    }

    #[test]
    fn dropping_handle_kills_child() {
        let reg = registry();
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        // `sleep` never emits a frame and never exits on its own.
        let cfg = crate::ingest::bridge::config::BridgeConfig {
            name: "sleeper".into(),
            command: "sleep".into(),
            args: vec!["30".into()],
        };
        // Preamble will never arrive; the child stays alive until we drop.
        let src = SubprocessSource::new(cfg, &reg);
        let handle = Box::new(src).spawn(store);
        // Give the thread time to spawn the child.
        std::thread::sleep(Duration::from_millis(200));
        let current = handle.child_guard.as_ref().unwrap().current.clone();
        let pid = current.lock().unwrap().as_ref().map(|c| c.id());
        assert!(pid.is_some(), "child should be running");
        drop(handle); // ChildGuard::drop kills + reaps it
        // After drop, the shared slot is emptied.
        assert!(current.lock().unwrap().is_none());
    }
}
```

- [ ] **Step 5: Run test to verify it fails**

Run: `cargo test -p datavis bridge::source`
Expected: FAIL (does not compile — `SubprocessSource` undefined).

- [ ] **Step 6: Write the implementation**

Prepend to `src/ingest/bridge/source.rs`:
```rust
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::Sender;

use crate::config::ChannelRegistry;
use crate::ingest::bridge::config::BridgeConfig;
use crate::ingest::bridge::frame::{FrameError, FrameReader};
use crate::ingest::bridge::router::BridgeRouter;
use crate::ingest::bridge::schema::{batch_schema_bytes, pb};
use crate::ingest::source::{ChildGuard, DataSource, SourceHandle};
use crate::ingest::{CONNECTING, LIVE, TIMEOUT};
use crate::record::RecordMsg;
use crate::store::ChannelStore;
use prost::Message;

/// Idle window before the status indicator drops from LIVE to TIMEOUT.
const TIMEOUT_AFTER: Duration = Duration::from_secs(5);

/// A `DataSource` that spawns an external adapter and reads a fixed columnar
/// `Batch` stream off its stdout.
pub struct SubprocessSource {
    cfg: BridgeConfig,
    router: BridgeRouter,
    schema_bytes: Vec<u8>,
}

impl SubprocessSource {
    pub fn new(cfg: BridgeConfig, registry: &ChannelRegistry) -> Self {
        Self {
            cfg,
            router: BridgeRouter::build(registry),
            schema_bytes: batch_schema_bytes().to_vec(),
        }
    }
}

impl DataSource for SubprocessSource {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let this = *self;
        let name = this.cfg.name.clone();
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));
        let record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let current: Arc<Mutex<Option<std::process::Child>>> = Arc::new(Mutex::new(None));
        let last_frame_ns = Arc::new(AtomicI64::new(0));
        let schema_bytes = this.schema_bytes.clone();

        // Reader / restart thread.
        {
            let conn = conn_state.clone();
            let rec = record_sender.clone();
            let stop = stop.clone();
            let current = current.clone();
            let last = last_frame_ns.clone();
            std::thread::spawn(move || {
                run_loop(this.cfg, this.router, store, conn, rec, stop, current, last);
            });
        }

        // Watchdog: downgrade LIVE → TIMEOUT after an idle gap.
        {
            let conn = conn_state.clone();
            let stop = stop.clone();
            let last = last_frame_ns.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    if conn.load(Ordering::Relaxed) == LIVE {
                        let last_ns = last.load(Ordering::Relaxed);
                        if crate::types::now_ns() - last_ns > TIMEOUT_AFTER.as_nanos() as i64 {
                            conn.store(TIMEOUT, Ordering::Relaxed);
                        }
                    }
                }
            });
        }

        SourceHandle {
            name,
            conn_state,
            record_sender,
            discovery: None,
            schema_bytes: Some(schema_bytes),
            child_guard: Some(ChildGuard { stop, current }),
        }
    }
}
```

`name` and `schema_bytes` are cloned before the reader thread moves `this.cfg`
and `this.router`, so the `SourceHandle` can still name the source afterwards.

Then add the reader loop below the `impl`:
```rust
#[allow(clippy::too_many_arguments)]
fn run_loop(
    cfg: BridgeConfig,
    router: BridgeRouter,
    store: Arc<dyn ChannelStore>,
    conn: Arc<AtomicU8>,
    record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    stop: Arc<AtomicBool>,
    current: Arc<Mutex<Option<std::process::Child>>>,
    last_frame_ns: Arc<AtomicI64>,
) {
    let topic: std::sync::Arc<str> = std::sync::Arc::from(cfg.name.as_str());
    let mut backoff = Duration::from_millis(250);
    while !stop.load(Ordering::Relaxed) {
        conn.store(CONNECTING, Ordering::Relaxed);

        let mut child = match Command::new(&cfg.command)
            .args(&cfg.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("bridge {:?}: failed to spawn {:?}: {e}", cfg.name, cfg.command);
                if sleep_or_stop(&stop, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
                continue;
            }
        };

        let stdout = child.stdout.take().expect("piped stdout");
        // Forward the child's stderr to our log, prefixed with the source name.
        if let Some(stderr) = child.stderr.take() {
            let name = cfg.name.clone();
            std::thread::spawn(move || log_stderr(name, stderr));
        }
        *current.lock().unwrap() = Some(child);

        match read_stream(stdout, &router, store.as_ref(), &conn, &record_sender, &last_frame_ns, &topic) {
            StreamEnd::PermanentProtocol => {
                eprintln!("bridge {:?}: protocol mismatch; not restarting", cfg.name);
                conn.store(TIMEOUT, Ordering::Relaxed);
                reap(&current);
                return; // permanent — do not respawn
            }
            StreamEnd::Ended => {
                reap(&current);
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                eprintln!("bridge {:?}: child ended; restarting in {:?}", cfg.name, backoff);
                if sleep_or_stop(&stop, backoff) {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
        // A stream that produced frames resets the backoff on the next healthy run.
        if conn.load(Ordering::Relaxed) == LIVE {
            backoff = Duration::from_millis(250);
        }
    }
}

enum StreamEnd {
    PermanentProtocol,
    Ended,
}

#[allow(clippy::too_many_arguments)]
fn read_stream<R: Read>(
    stdout: R,
    router: &BridgeRouter,
    store: &dyn ChannelStore,
    conn: &Arc<AtomicU8>,
    record_sender: &Arc<Mutex<Option<Sender<RecordMsg>>>>,
    last_frame_ns: &Arc<AtomicI64>,
    topic: &std::sync::Arc<str>,
) -> StreamEnd {
    let mut reader = FrameReader::new(stdout);
    if let Err(e) = reader.read_preamble() {
        return match e {
            FrameError::BadPreamble => StreamEnd::PermanentProtocol,
            _ => StreamEnd::Ended, // child died before/mid preamble → restart
        };
    }
    loop {
        match reader.next_frame() {
            Ok(None) => return StreamEnd::Ended, // clean EOF
            Ok(Some(body)) => {
                match pb::Batch::decode(body.as_slice()) {
                    Ok(batch) => {
                        router.apply(&batch, store);
                    }
                    Err(e) => {
                        eprintln!("bridge: batch decode error: {e}; skipping frame");
                        continue;
                    }
                }
                conn.store(LIVE, Ordering::Relaxed);
                last_frame_ns.store(crate::types::now_ns(), Ordering::Relaxed);
                forward_to_recorder(record_sender, topic, &body);
            }
            Err(FrameError::BadPreamble) => return StreamEnd::PermanentProtocol,
            Err(e) => {
                eprintln!("bridge: frame error: {e}");
                return StreamEnd::Ended; // oversized/io → restart
            }
        }
    }
}

fn forward_to_recorder(
    record_sender: &Arc<Mutex<Option<Sender<RecordMsg>>>>,
    topic: &std::sync::Arc<str>,
    body: &[u8],
) {
    if let Ok(guard) = record_sender.try_lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.try_send(RecordMsg::Proto {
                topic: topic.clone(),
                data: body.to_vec(),
                ts: crate::types::now_ns(),
            });
        }
    }
}

fn log_stderr(name: String, stderr: impl Read) {
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(stderr);
    for line in reader.lines().map_while(Result::ok) {
        eprintln!("bridge {name}: {line}");
    }
}

fn reap(current: &Arc<Mutex<Option<std::process::Child>>>) {
    if let Ok(mut guard) = current.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Sleep for `dur`, waking early if `stop` is set. Returns `true` if stopping.
fn sleep_or_stop(stop: &Arc<AtomicBool>, dur: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < dur {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.load(Ordering::Relaxed)
}
```

- [ ] **Step 7: Register and re-export the module**

In `src/ingest/bridge/mod.rs` add:
```rust
pub mod source;

pub use config::BridgeConfig;
pub use source::SubprocessSource;
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p datavis bridge::source`
Expected: PASS (2 tests). The integration test builds the `echo_bridge`
example; if the harness does not build examples before tests, run
`cargo build --example echo_bridge` first. The `dropping_handle_kills_child`
test uses `sleep`, which exists on Linux/macOS; it is skipped conceptually on
Windows — gate it with `#[cfg(unix)]` if the CI matrix runs it on Windows.

- [ ] **Step 9: Verify both feature sets build**

Run: `cargo build && cargo build --no-default-features`
Expected: both succeed.

- [ ] **Step 10: Commit**

```bash
git add src/ingest/source.rs src/ingest/bridge/ src/app.rs src/ingest/mqtt.rs src/ingest/websocket.rs src/ingest/mod.rs src/script/mod.rs
git commit -m "feat(ingest): SubprocessSource with lifecycle + kill-on-shutdown"
```

---

### Task 7: Wire bridges into startup (`main.rs`)

Read `[[sources.bridge]]` from the resolved config and spawn one
`SubprocessSource` per entry, pushing each handle into `sources`.

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `BridgeConfig::list_from_toml_str`, `SubprocessSource::new`,
  `DataSource::spawn`, the existing `sources: Vec<SourceHandle>` and
  `layout_path`.

- [ ] **Step 1: Add the wiring**

In `src/main.rs`, after the WebSocket source block (the `if let Some(listen) =
arg_value(&args, "--ws-listen")` block ending around line 79) and before the
scripting section, insert:
```rust
    // External bridges: spawn each org-provided adapter declared in
    // [[sources.bridge]]. datavis owns the child process lifecycle.
    let config_text = std::fs::read_to_string(&layout_path).unwrap_or_default();
    match datavis::ingest::bridge::BridgeConfig::list_from_toml_str(&config_text) {
        Ok(bridges) => {
            for bridge in bridges {
                let src = datavis::ingest::bridge::SubprocessSource::new(bridge, channels.as_ref());
                sources.push(Box::new(src).spawn(store.clone()));
            }
        }
        Err(e) => eprintln!("config: ignoring malformed [[sources.bridge]]: {e}"),
    }
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build`
Expected: success.

- [ ] **Step 3: Manual smoke test**

Run:
```bash
cargo build --example echo_bridge
printf 'default_window_s = 10.0\n\n[channels."accel"]\ntopic = "accel"\ntype = "float"\n\n[[sources.bridge]]\nname = "echo"\ncommand = "target/debug/examples/echo_bridge"\n' > /tmp/bridge-smoke.toml
```
Then confirm the config parses and a bridge is constructed by running the app
with that config for a moment (or rely on Task 6's integration test as the
automated equivalent). Expected: no `ignoring malformed` message; the `accel`
channel receives samples.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(ingest): spawn [[sources.bridge]] adapters at startup"
```

---

### Task 8: Documentation — external source protocol + how-to guide

Publish the wire contract and a guide so third parties can write an adapter.

**Files:**
- Create: `docs/external-source-protocol.md`
- Modify: `src/ingest/bridge/mod.rs` (link the doc from the module rustdoc)

**Interfaces:**
- Consumes: nothing (documentation only).

- [ ] **Step 1: Write the protocol document**

Create `docs/external-source-protocol.md` covering, with the exact values from
this plan's Global Constraints:
- The license rationale (separate process = not a GPL derivative work).
- The stream preamble: `DVS\x01` + version byte `1`.
- The frame: `u32` LE `body_len` + `Batch` protobuf; the 16 MiB cap.
- The full `Batch`/`Column`/`*Col` schema (copy the proto from Global
  Constraints) and the rule that `t_ns` length must equal the chosen column's
  length, one `oneof` variant per column.
- Channel mapping: each `Column.topic` matches a `[channels."…"]` entry that
  declares `topic` + `sample_type`; topics must be unique across sources.
- Lifecycle: datavis spawns the `command`, restarts it with backoff on exit,
  stops permanently on a bad preamble/version, and captures stderr to its log.
- A worked example pointing at `examples/echo_bridge.rs`.

- [ ] **Step 2: Link the doc from the module**

At the top of `src/ingest/bridge/mod.rs`, extend the module doc comment:
```rust
//! The wire contract is documented in `docs/external-source-protocol.md`; the
//! reference adapter is `examples/echo_bridge.rs`.
```

- [ ] **Step 3: Verify docs build**

Run: `cargo doc --no-deps`
Expected: success, no broken-link warnings for the bridge module.

- [ ] **Step 4: Commit**

```bash
git add docs/external-source-protocol.md src/ingest/bridge/mod.rs
git commit -m "docs: external source protocol + bridge authoring guide"
```

---

## Notes / known limitations

- **Recording/replay:** bridge frames are forwarded to the recorder as
  `RecordMsg::Proto` under the source name with the static `Batch` schema, so
  sessions capture the raw bytes. Full replay *reconstruction* into individual
  channels is not wired in this plan (bridge channels carry no `proto_path` for
  the replay decoder to use); treat cross-session replay of bridge channels as a
  follow-up. This is a deliberate narrowing of the spec's "records and replays
  like any native source" wording.
