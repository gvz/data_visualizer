# Ingest Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a ZMQ SUB ingest thread that decodes protobuf batches using prost-reflect (no codegen), applies EU scaling, and writes samples to the live ChannelStore.

**Architecture:** A dedicated ingest thread opens a ZMQ SUB socket, subscribes per-topic, and loops on recv. Each received message is decoded once into a `DynamicMessage` using a `DescriptorPool` built at startup from a `.proto` file (via protox). A `TopicRouter` maps each topic to a list of `ChannelBinding`s (one per channel on that topic), and `decode_batch` iterates the bindings to write typed samples to the store. Connection state is exposed via an `Arc<AtomicU8>` polled by the main thread's toolbar.

**Tech Stack:** `zmq 0.10` (ZMQ subscriber wrapping libzmq), `prost-reflect 0.14` (dynamic proto reflection), `protox 0.7` (runtime `.proto` file compilation without protoc), existing `LiveStore`/`ChannelStore` interfaces.

## Global Constraints

- Rust stable (1.97.1 minimum via Nix flake dev shell: `nix develop`)
- Never change signatures of frozen interfaces: `VizPanel`, `PanelRegistry::with_builtins/register/build`, `ChannelStore`, `ChannelSnapshot`, `PanelEntry`, `ChannelRegistry`, `ChannelId`, `ChannelMeta`, `SampleType`, `NumericVal`, `Sample`, `TimeWindow`
- EU scaling formula: `scaled = raw * eu_scale + eu_offset`; applied to `Float` and `Int` channels only; `Bool` channels use raw bool value (ignore EU); `Text` channels skip EU entirely
- Timestamp extraction returns `i64` nanoseconds since Unix epoch; samples with missing or unextractable timestamp are silently skipped (no panic)
- Unknown topics (no channels registered for that topic) are silently ignored on recv — no log spam per message
- Proto decode failure: `eprintln!` the error and return 0 samples written (no panic, no crash)
- `zmq` crate wraps libzmq (zeromq C library provided by `nix develop`); use `zmq = "0.10"`
- prost-reflect exact method names may vary slightly from plan text; follow the compiler and `cargo doc --open` for `prost_reflect` if a name doesn't compile
- ZMQ message format: multipart `[topic_bytes_frame, proto_payload_frame]`; subscribe one per unique topic from `ChannelRegistry`
- All commit messages: plain description only, NO Co-Authored-By, NO AI attribution, NO emoji

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `Cargo.toml` | Modify | Add `zmq`, `prost-reflect`, `protox` deps; add `tempfile` dev-dep |
| `src/ingest/mod.rs` | Create | Public API: `ConnState` constants, `IngestHandle`, `IngestConfig`, `spawn_ingest` |
| `src/ingest/loader.rs` | Create | `ProtoSchema` (load .proto → DescriptorPool), `ChannelDesc` (field path resolution) |
| `src/ingest/router.rs` | Create | `ChannelBinding`, `TopicRouter` (topic → Vec<ChannelBinding>) |
| `src/ingest/decode.rs` | Create | `decode_batch` (bytes → EU-scaled writes to store) |
| `src/ingest/thread.rs` | Create | `run_loop` (ZMQ SUB recv, timeout-based ConnState, exponential backoff reconnect) |
| `src/lib.rs` | Modify | Add `pub mod ingest` |
| `src/main.rs` | Modify | Parse `--endpoint`/`--schema` CLI args, call `spawn_ingest` unless `--demo` |
| `src/app.rs` | Modify | Accept `Option<Arc<AtomicU8>>` conn_state; show dynamic LIVE/CONNECTING/TIMEOUT in toolbar |

---

### Task 1: Cargo deps and ingest module scaffold

**Files:**
- Modify: `Cargo.toml`
- Create: `src/ingest/mod.rs`
- Create: `src/ingest/loader.rs`, `src/ingest/router.rs`, `src/ingest/decode.rs`, `src/ingest/thread.rs` (empty stubs)
- Modify: `src/lib.rs`

**Interfaces:**
- Produces:
  - `pub const CONNECTING: u8 = 0` — initial state, no data received yet
  - `pub const LIVE: u8 = 1` — data received within the last 5s
  - `pub const TIMEOUT: u8 = 2` — no data received for > 5s after being live
  - `pub struct IngestConfig { pub endpoint: String, pub proto_path: std::path::PathBuf }`
  - `pub struct IngestHandle { pub conn_state: Arc<AtomicU8> }`
  - `pub fn spawn_ingest(config: IngestConfig, registry: &ChannelRegistry, store: Arc<dyn ChannelStore>) -> anyhow::Result<IngestHandle>` — stub returning `Err(anyhow!("not yet implemented"))`

- [ ] **Step 1: Add deps to Cargo.toml**

Add these to the `[dependencies]` section:
```toml
zmq = "0.10"
prost-reflect = "0.14"
protox = "0.7"
```

Add a `[dev-dependencies]` section (or add to existing if present):
```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write failing compilation test**

Create `src/ingest/mod.rs`:
```rust
#[cfg(test)]
mod tests {
    #[test]
    fn ingest_module_compiles() {}
}
```

Add to `src/lib.rs` after the other `pub mod` declarations:
```rust
pub mod ingest;
```

Create four empty stub files (each containing just `// placeholder`):
- `src/ingest/loader.rs`
- `src/ingest/router.rs`
- `src/ingest/decode.rs`
- `src/ingest/thread.rs`

Run: `cargo test ingest_module_compiles`
Expected: PASS (compilation success proves deps resolve)

- [ ] **Step 3: Write the ConnState constants test**

In `src/ingest/mod.rs`, add the test:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_module_compiles() {}

    #[test]
    fn conn_state_constants_are_distinct() {
        assert_eq!(CONNECTING, 0u8);
        assert_eq!(LIVE, 1u8);
        assert_eq!(TIMEOUT, 2u8);
        assert_ne!(CONNECTING, LIVE);
        assert_ne!(LIVE, TIMEOUT);
        assert_ne!(CONNECTING, TIMEOUT);
    }
}
```

Run: `cargo test conn_state_constants_are_distinct`
Expected: FAIL (CONNECTING not defined)

- [ ] **Step 4: Implement the scaffold**

Replace `src/ingest/mod.rs` with:
```rust
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::anyhow;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;

pub mod decode;
pub mod loader;
pub mod router;
pub mod thread;

pub const CONNECTING: u8 = 0;
pub const LIVE: u8 = 1;
pub const TIMEOUT: u8 = 2;

pub struct IngestConfig {
    pub endpoint: String,
    pub proto_path: PathBuf,
}

pub struct IngestHandle {
    pub conn_state: Arc<AtomicU8>,
}

pub fn spawn_ingest(
    _config: IngestConfig,
    _registry: &ChannelRegistry,
    _store: Arc<dyn ChannelStore>,
) -> anyhow::Result<IngestHandle> {
    Err(anyhow!("not yet implemented"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_module_compiles() {}

    #[test]
    fn conn_state_constants_are_distinct() {
        assert_eq!(CONNECTING, 0u8);
        assert_eq!(LIVE, 1u8);
        assert_eq!(TIMEOUT, 2u8);
        assert_ne!(CONNECTING, LIVE);
        assert_ne!(LIVE, TIMEOUT);
        assert_ne!(CONNECTING, TIMEOUT);
    }
}
```

- [ ] **Step 5: Run all tests**

Run: `cargo test`
Expected: all existing tests pass; `conn_state_constants_are_distinct` passes.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/lib.rs src/ingest/
git commit -m "feat(ingest): add ZMQ/prost-reflect deps and ingest module scaffold"
```

---

### Task 2: Proto schema loader

**Files:**
- Modify: `src/ingest/loader.rs`

**Interfaces:**
- Consumes: `protox::compile`, `prost_reflect::{DescriptorPool, MessageDescriptor}`
- Produces:
  - `pub fn parse_field_path(path: &str) -> anyhow::Result<(String, Vec<String>)>` — splits `"AccelBatch.samples.x"` into `("AccelBatch", vec!["samples", "x"])`; errors if fewer than 2 dot-separated segments
  - `pub struct ChannelDesc { pub msg_desc: MessageDescriptor, pub val_path: Vec<String>, pub ts_path: Vec<String> }` — `val_path` and `ts_path` are the field steps after the message type name (e.g., `["samples", "x"]` from `"AccelBatch.samples.x"`)
  - `pub struct ProtoSchema { /* pool: DescriptorPool (private) */ }`
  - `impl ProtoSchema { pub fn from_path(proto_file: &std::path::Path) -> anyhow::Result<Self> }` — compiles the `.proto` file into a `DescriptorPool` using protox
  - `impl ProtoSchema { pub fn resolve(&self, proto_path: &str, ts_path: &str) -> anyhow::Result<ChannelDesc> }` — parses both paths, validates they share the same message type name, looks up the message descriptor, returns `ChannelDesc`
  - `impl ProtoSchema { #[cfg(test)] pub fn pool_for_test(&self) -> &DescriptorPool }` — test-only accessor for the descriptor pool (needed by Task 4 to build test messages)

- [ ] **Step 1: Write failing tests**

Replace `src/ingest/loader.rs` with:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_test_proto(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
        let path = dir.join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{}", content).unwrap();
        path
    }

    #[test]
    fn parse_three_segments() {
        let (msg, path) = parse_field_path("AccelBatch.samples.x").unwrap();
        assert_eq!(msg, "AccelBatch");
        assert_eq!(path, vec!["samples", "x"]);
    }

    #[test]
    fn parse_two_segments() {
        let (msg, path) = parse_field_path("FlatMsg.value").unwrap();
        assert_eq!(msg, "FlatMsg");
        assert_eq!(path, vec!["value"]);
    }

    #[test]
    fn parse_one_segment_is_err() {
        assert!(parse_field_path("NoField").is_err());
    }

    #[test]
    fn parse_empty_is_err() {
        assert!(parse_field_path("").is_err());
    }

    #[test]
    fn schema_loads_and_resolves_batch_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), r#"
syntax = "proto3";
message AccelBatch {
  repeated Sample samples = 1;
  message Sample {
    int64 t_ns = 1;
    float x = 2;
  }
}
"#);
        let schema = ProtoSchema::from_path(&path).unwrap();
        let desc = schema.resolve("AccelBatch.samples.x", "AccelBatch.samples.t_ns").unwrap();
        assert_eq!(desc.val_path, vec!["samples", "x"]);
        assert_eq!(desc.ts_path, vec!["samples", "t_ns"]);
        assert_eq!(desc.msg_desc.name(), "AccelBatch");
    }

    #[test]
    fn schema_loads_and_resolves_flat_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), r#"
syntax = "proto3";
message FlatMsg {
  int64 t_ns = 1;
  float value = 2;
}
"#);
        let schema = ProtoSchema::from_path(&path).unwrap();
        let desc = schema.resolve("FlatMsg.value", "FlatMsg.t_ns").unwrap();
        assert_eq!(desc.val_path, vec!["value"]);
        assert_eq!(desc.ts_path, vec!["t_ns"]);
    }

    #[test]
    fn resolve_unknown_message_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), "syntax = \"proto3\";\n");
        let schema = ProtoSchema::from_path(&path).unwrap();
        assert!(schema.resolve("NoSuchMsg.x", "NoSuchMsg.t").is_err());
    }

    #[test]
    fn resolve_mismatched_message_names_is_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path(), r#"
syntax = "proto3";
message A { int64 t = 1; float v = 2; }
message B { int64 t = 1; float v = 2; }
"#);
        let schema = ProtoSchema::from_path(&path).unwrap();
        assert!(schema.resolve("A.v", "B.t").is_err());
    }

    #[test]
    fn from_path_nonexistent_file_is_err() {
        assert!(ProtoSchema::from_path(std::path::Path::new("/nonexistent/schema.proto")).is_err());
    }
}
```

Run: `cargo test loader::tests`
Expected: FAIL (functions not defined)

- [ ] **Step 2: Implement parse_field_path**

Add to the top of `src/ingest/loader.rs` (before the tests module):
```rust
use std::path::Path;

use anyhow::{anyhow, Context};
use prost_reflect::{DescriptorPool, MessageDescriptor};

pub fn parse_field_path(path: &str) -> anyhow::Result<(String, Vec<String>)> {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.len() < 2 {
        return Err(anyhow!(
            "field path {:?} must have at least 2 dot-separated segments (MessageType.field)",
            path
        ));
    }
    let msg_name = parts[0].to_string();
    let field_steps: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
    Ok((msg_name, field_steps))
}
```

Run: `cargo test parse_` (runs all parse_* tests)
Expected: all 4 parse tests pass.

- [ ] **Step 3: Implement ProtoSchema and ChannelDesc**

Add after `parse_field_path`:
```rust
pub struct ChannelDesc {
    pub msg_desc: MessageDescriptor,
    /// Field steps after the message type name, e.g. ["samples", "x"].
    pub val_path: Vec<String>,
    /// Field steps for timestamp, e.g. ["samples", "t_ns"].
    pub ts_path: Vec<String>,
}

pub struct ProtoSchema {
    pool: DescriptorPool,
}

impl ProtoSchema {
    pub fn from_path(proto_file: &Path) -> anyhow::Result<Self> {
        let include_dir = proto_file.parent().unwrap_or(Path::new("."));
        let fds = protox::compile([proto_file], [include_dir])
            .with_context(|| format!("compiling proto schema {}", proto_file.display()))?;
        let pool = DescriptorPool::from_file_descriptor_set(fds)
            .context("building descriptor pool from compiled schema")?;
        Ok(Self { pool })
    }

    pub fn resolve(&self, proto_path: &str, ts_path: &str) -> anyhow::Result<ChannelDesc> {
        let (val_msg, val_steps) = parse_field_path(proto_path)?;
        let (ts_msg, ts_steps) = parse_field_path(ts_path)?;
        if val_msg != ts_msg {
            return Err(anyhow!(
                "proto_path and ts_path must share the same message type: \
                 proto_path uses {:?} but ts_path uses {:?}",
                val_msg,
                ts_msg
            ));
        }
        let msg_desc = self
            .pool
            .get_message_by_name(&val_msg)
            .ok_or_else(|| anyhow!("message type {:?} not found in proto schema", val_msg))?;
        Ok(ChannelDesc { msg_desc, val_path: val_steps, ts_path: ts_steps })
    }

    #[cfg(test)]
    pub fn pool_for_test(&self) -> &DescriptorPool {
        &self.pool
    }
}
```

**API note:** `protox::compile` takes iterables of file paths and include directories. If the compiler disagrees with the exact signature (e.g., it requires `&[&str]` or string slices), pass `&[proto_file.to_str().unwrap()]` and `&[include_dir.to_str().unwrap()]` instead. `DescriptorPool::from_file_descriptor_set` may be named differently in your installed version — check `cargo doc --open` for `prost_reflect::DescriptorPool` and use the constructor that takes a `prost_types::FileDescriptorSet`.

- [ ] **Step 4: Run all loader tests**

Run: `cargo test loader`
Expected: all 8 loader tests pass.

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/ingest/loader.rs
git commit -m "feat(ingest): proto schema loader with field path resolution"
```

---

### Task 3: Topic router

**Files:**
- Modify: `src/ingest/router.rs`

**Interfaces:**
- Consumes: `loader::{ProtoSchema, ChannelDesc}`, `config::ChannelRegistry`, `types::{ChannelId, SampleType}`
- Produces:
  - `pub struct ChannelBinding { pub id: ChannelId, pub msg_desc: MessageDescriptor, pub val_path: Vec<String>, pub ts_path: Vec<String>, pub eu_scale: f64, pub eu_offset: f64, pub sample_type: SampleType }`
  - `pub struct TopicRouter { /* map: HashMap<String, Vec<ChannelBinding>> (private) */ }`
  - `impl TopicRouter { pub fn build(registry: &ChannelRegistry, schema: &ProtoSchema) -> Self }` — iterates all channels; for each: calls `schema.resolve(cfg.proto_path, cfg.ts_path)`, builds `ChannelBinding`, pushes into `HashMap<String, Vec<ChannelBinding>>` keyed by `cfg.topic`. Channels whose `resolve()` fails get an `eprintln!` warning and are skipped.
  - `impl TopicRouter { pub fn topics(&self) -> impl Iterator<Item = &str> + '_ }` — unique topics in the map
  - `impl TopicRouter { pub fn bindings_for(&self, topic: &str) -> &[ChannelBinding] }` — empty slice if topic unknown

- [ ] **Step 1: Write failing tests**

Replace `src/ingest/router.rs` with:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::loader::ProtoSchema;
    use std::io::Write;

    fn write_test_proto(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message AccelBatch {{
  repeated Sample samples = 1;
  message Sample {{
    int64 t_ns = 1;
    float x = 2;
    float y = 3;
  }}
}}
message StatusBatch {{
  repeated Sample samples = 1;
  message Sample {{
    int64 t_ns = 1;
    int64 state = 2;
  }}
}}
"#).unwrap();
        path
    }

    fn test_schema() -> (ProtoSchema, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_proto(dir.path());
        (ProtoSchema::from_path(&path).unwrap(), dir)
    }

    fn test_registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"

[channels."accel.y"]
topic = "accel"
proto_path = "AccelBatch.samples.y"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
eu_scale = 2.0
eu_offset = -1.0

[channels."motor.state"]
topic = "status"
proto_path = "StatusBatch.samples.state"
ts_path = "StatusBatch.samples.t_ns"
type = "int"
"#).unwrap()
    }

    #[test]
    fn router_routes_two_topics() {
        let (schema, _dir) = test_schema();
        let registry = test_registry();
        let router = TopicRouter::build(&registry, &schema);

        assert_eq!(router.bindings_for("accel").len(), 2);
        assert_eq!(router.bindings_for("status").len(), 1);
        assert!(router.bindings_for("unknown").is_empty());
    }

    #[test]
    fn router_preserves_eu_scale() {
        let (schema, _dir) = test_schema();
        let registry = test_registry();
        let router = TopicRouter::build(&registry, &schema);
        let accel = router.bindings_for("accel");
        let y = accel.iter().find(|b| b.val_path.last().map(|s| s.as_str()) == Some("y")).unwrap();
        assert_eq!(y.eu_scale, 2.0);
        assert_eq!(y.eu_offset, -1.0);
    }

    #[test]
    fn router_topics_iterator() {
        let (schema, _dir) = test_schema();
        let registry = test_registry();
        let router = TopicRouter::build(&registry, &schema);
        let mut topics: Vec<&str> = router.topics().collect();
        topics.sort();
        assert_eq!(topics, vec!["accel", "status"]);
    }

    #[test]
    fn router_skips_channel_with_unknown_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.proto");
        std::fs::write(&path, b"syntax = \"proto3\";\n").unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."bad"]
topic = "t"
proto_path = "NoMsg.field"
ts_path = "NoMsg.t"
type = "float"
"#).unwrap();
        let router = TopicRouter::build(&registry, &schema);
        assert!(router.bindings_for("t").is_empty());
    }
}
```

Run: `cargo test router::tests`
Expected: FAIL (TopicRouter not defined)

- [ ] **Step 2: Implement TopicRouter**

Add before the tests module in `src/ingest/router.rs`:
```rust
use std::collections::HashMap;

use prost_reflect::MessageDescriptor;

use crate::config::ChannelRegistry;
use crate::ingest::loader::ProtoSchema;
use crate::types::{ChannelId, SampleType};

pub struct ChannelBinding {
    pub id: ChannelId,
    pub msg_desc: MessageDescriptor,
    pub val_path: Vec<String>,
    pub ts_path: Vec<String>,
    pub eu_scale: f64,
    pub eu_offset: f64,
    pub sample_type: SampleType,
}

pub struct TopicRouter {
    map: HashMap<String, Vec<ChannelBinding>>,
}

impl TopicRouter {
    pub fn build(registry: &ChannelRegistry, schema: &ProtoSchema) -> Self {
        let mut map: HashMap<String, Vec<ChannelBinding>> = HashMap::new();
        for id in registry.iter_ids() {
            let cfg = registry.config(id);
            let meta = registry.meta(id);
            match schema.resolve(&cfg.proto_path, &cfg.ts_path) {
                Ok(desc) => {
                    let binding = ChannelBinding {
                        id,
                        msg_desc: desc.msg_desc,
                        val_path: desc.val_path,
                        ts_path: desc.ts_path,
                        eu_scale: cfg.eu_scale,
                        eu_offset: cfg.eu_offset,
                        sample_type: meta.sample_type,
                    };
                    map.entry(cfg.topic.clone()).or_default().push(binding);
                }
                Err(e) => {
                    eprintln!("ingest: skipping channel {:?}: {e}", meta.name);
                }
            }
        }
        Self { map }
    }

    pub fn topics(&self) -> impl Iterator<Item = &str> + '_ {
        self.map.keys().map(|s| s.as_str())
    }

    pub fn bindings_for(&self, topic: &str) -> &[ChannelBinding] {
        self.map.get(topic).map(|v| v.as_slice()).unwrap_or(&[])
    }
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test router`
Expected: all 4 router tests pass.

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/ingest/router.rs
git commit -m "feat(ingest): topic router maps ZMQ topics to channel bindings"
```

---

### Task 4: Batch decoder

**Files:**
- Modify: `src/ingest/decode.rs`
- Modify: `src/ingest/loader.rs` (add `pool_for_test` — already declared in Task 2; verify it's there)

**Interfaces:**
- Consumes: `router::ChannelBinding`, `store::ChannelStore`, `types::{NumericVal, SampleType}`
- Produces:
  - `pub fn decode_batch(data: &[u8], bindings: &[ChannelBinding], store: &dyn ChannelStore) -> usize` — decodes `data` as the message described by `bindings[0].msg_desc`, iterates all bindings, writes samples to store; returns total sample count written. Returns 0 on decode error (after eprintln). Returns 0 immediately if `bindings` is empty.

Internal helpers (private):
- `fn decode_channel(msg: &DynamicMessage, binding: &ChannelBinding, store: &dyn ChannelStore) -> usize` — dispatches to batch (path len 2) or single-value (path len 1) mode
- `fn decode_batch_channel(msg, binding, store) -> usize` — iterates a repeated message field, writes one sample per element
- `fn decode_single_channel(msg, binding, store) -> usize` — reads one value from the top-level message
- `fn get_named_field(msg: &DynamicMessage, name: &str) -> Option<Value>` — looks up field by name and returns owned Value
- `fn write_value(binding: &ChannelBinding, ts: i64, val: &Value, store: &dyn ChannelStore) -> bool`
- `fn extract_ts(val: &Value) -> Option<i64>` — extracts i64 from I64/U64/I32/U32 variants; None for others
- `fn extract_numeric(val: &Value, sample_type: SampleType, eu_scale: f64, eu_offset: f64) -> Option<NumericVal>` — applies EU to Float/Int; raw bool for Bool; None for Text/unrecognized
- `fn extract_text(val: &Value) -> Option<String>` — Some only for `Value::String`; None otherwise

**EU scaling rules:**
- `SampleType::Float` → `NumericVal::Float(raw * eu_scale + eu_offset)`
- `SampleType::Int` → `NumericVal::Int((raw * eu_scale + eu_offset) as i64)`
- `SampleType::Bool` → `NumericVal::Bool(raw != 0.0)` (EU ignored, use raw bool interpretation)
- `SampleType::Text` → `None` from `extract_numeric` (handled separately via `extract_text`)

- [ ] **Step 1: Write failing tests**

Replace `src/ingest/decode.rs` with:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::loader::ProtoSchema;
    use crate::ingest::router::TopicRouter;
    use crate::store::LiveStore;
    use crate::types::{ChannelSnapshot, TimeWindow};
    use prost::Message as _;
    use prost_reflect::{DynamicMessage, Value};
    use std::io::Write;

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    fn make_schema_and_registry() -> (ProtoSchema, tempfile::TempDir, ChannelRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message AccelBatch {{
  repeated Sample samples = 1;
  message Sample {{
    int64 t_ns = 1;
    float x = 2;
  }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
eu_scale = 2.0
eu_offset = 1.0
"#).unwrap();
        (schema, dir, registry)
    }

    fn encode_accel_batch(schema: &ProtoSchema, samples: &[(i64, f32)]) -> Vec<u8> {
        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("AccelBatch").unwrap();
        let sample_desc = pool.get_message_by_name("AccelBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let x_field = sample_desc.get_field_by_name("x").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();

        let list: Vec<Value> = samples
            .iter()
            .map(|(t, x)| {
                let mut s = DynamicMessage::new(sample_desc.clone());
                s.set_field(&t_field, Value::I64(*t));
                s.set_field(&x_field, Value::F32(*x));
                Value::Message(s)
            })
            .collect();
        let mut batch = DynamicMessage::new(batch_desc);
        batch.set_field(&samples_field, Value::List(list));
        batch.encode_to_vec()
    }

    #[test]
    fn decode_batch_writes_eu_scaled_samples() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        let data = encode_accel_batch(&schema, &[(1_000_000_000, 2.0), (2_000_000_000, 3.0)]);

        let count = decode_batch(&data, bindings, &store);
        assert_eq!(count, 2);

        let ch = registry.id("accel.x").unwrap();
        match store.snapshot(ch, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000i64, 2_000_000_000i64]);
                // EU: raw * 2.0 + 1.0 → 2.0*2+1=5.0, 3.0*2+1=7.0
                assert!((vals[0] - 5.0_f64).abs() < 1e-4, "expected 5.0, got {}", vals[0]);
                assert!((vals[1] - 7.0_f64).abs() < 1e-4, "expected 7.0, got {}", vals[1]);
            }
            other => panic!("wrong snapshot variant: {other:?}"),
        }
    }

    #[test]
    fn decode_batch_empty_bindings_returns_zero() {
        let (_, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        assert_eq!(decode_batch(&[1, 2, 3], &[], &store), 0);
    }

    #[test]
    fn decode_batch_bad_bytes_returns_zero_no_panic() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        // Malformed proto bytes — must not panic, must return 0.
        assert_eq!(decode_batch(b"not valid protobuf at all!!!", bindings, &store), 0);
    }

    #[test]
    fn decode_batch_empty_repeated_returns_zero() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        // Empty batch: 0 samples.
        let data = encode_accel_batch(&schema, &[]);
        assert_eq!(decode_batch(&data, bindings, &store), 0);
    }
}
```

Run: `cargo test decode::tests`
Expected: FAIL (decode_batch not defined)

- [ ] **Step 2: Implement decode.rs**

Add before the tests module in `src/ingest/decode.rs`:
```rust
use prost_reflect::{DynamicMessage, Value};

use crate::ingest::router::ChannelBinding;
use crate::store::ChannelStore;
use crate::types::{NumericVal, SampleType};

pub fn decode_batch(data: &[u8], bindings: &[ChannelBinding], store: &dyn ChannelStore) -> usize {
    if bindings.is_empty() {
        return 0;
    }
    let msg_desc = &bindings[0].msg_desc;
    let msg = match DynamicMessage::decode(msg_desc.clone(), data) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("ingest: proto decode error: {e}");
            return 0;
        }
    };
    let mut total = 0;
    for binding in bindings {
        total += decode_channel(&msg, binding, store);
    }
    total
}

fn decode_channel(msg: &DynamicMessage, binding: &ChannelBinding, store: &dyn ChannelStore) -> usize {
    match binding.val_path.len() {
        2 => decode_batch_channel(msg, binding, store),
        1 => decode_single_channel(msg, binding, store),
        n => {
            eprintln!("ingest: unsupported field path depth {n} for channel {:?}", binding.val_path);
            0
        }
    }
}

fn decode_batch_channel(msg: &DynamicMessage, binding: &ChannelBinding, store: &dyn ChannelStore) -> usize {
    let repeated_name = &binding.val_path[0];
    let val_leaf = &binding.val_path[1];
    // ts_path has same structure: [repeated_field, ts_leaf] or just [ts_leaf] for flat paths.
    let ts_leaf = binding.ts_path.get(1).unwrap_or(&binding.ts_path[0]);

    let Some(field_desc) = msg.descriptor().get_field_by_name(repeated_name) else {
        return 0;
    };
    let repeated = msg.get_field(&field_desc).into_owned();
    let Value::List(samples) = repeated else {
        return 0;
    };

    let mut count = 0;
    for sample_val in &samples {
        let Value::Message(sample_msg) = sample_val else {
            continue;
        };
        let Some(ts) = get_named_field(sample_msg, ts_leaf).and_then(|v| extract_ts(&v)) else {
            continue;
        };
        let Some(val_v) = get_named_field(sample_msg, val_leaf) else {
            continue;
        };
        if write_value(binding, ts, &val_v, store) {
            count += 1;
        }
    }
    count
}

fn decode_single_channel(msg: &DynamicMessage, binding: &ChannelBinding, store: &dyn ChannelStore) -> usize {
    let val_leaf = &binding.val_path[0];
    let ts_leaf = &binding.ts_path[0];

    let Some(ts) = get_named_field(msg, ts_leaf).and_then(|v| extract_ts(&v)) else {
        return 0;
    };
    let Some(val_v) = get_named_field(msg, val_leaf) else {
        return 0;
    };
    usize::from(write_value(binding, ts, &val_v, store))
}

fn get_named_field(msg: &DynamicMessage, name: &str) -> Option<Value> {
    let field_desc = msg.descriptor().get_field_by_name(name)?;
    Some(msg.get_field(&field_desc).into_owned())
}

fn write_value(binding: &ChannelBinding, ts: i64, val: &Value, store: &dyn ChannelStore) -> bool {
    match binding.sample_type {
        SampleType::Text => {
            if let Some(s) = extract_text(val) {
                store.write_text(binding.id, ts, s);
                true
            } else {
                false
            }
        }
        st => {
            if let Some(nv) = extract_numeric(val, st, binding.eu_scale, binding.eu_offset) {
                store.write_numeric(binding.id, ts, nv);
                true
            } else {
                false
            }
        }
    }
}

fn extract_ts(val: &Value) -> Option<i64> {
    match val {
        Value::I64(v) => Some(*v),
        Value::U64(v) => Some(*v as i64),
        Value::I32(v) => Some(*v as i64),
        Value::U32(v) => Some(*v as i64),
        _ => None,
    }
}

fn extract_numeric(
    val: &Value,
    sample_type: SampleType,
    eu_scale: f64,
    eu_offset: f64,
) -> Option<NumericVal> {
    let raw: f64 = match val {
        Value::F64(v) => *v,
        Value::F32(v) => *v as f64,
        Value::I64(v) => *v as f64,
        Value::I32(v) => *v as f64,
        Value::U64(v) => *v as f64,
        Value::U32(v) => *v as f64,
        Value::Bool(b) => if *b { 1.0 } else { 0.0 },
        _ => return None,
    };
    match sample_type {
        SampleType::Float => Some(NumericVal::Float(raw * eu_scale + eu_offset)),
        SampleType::Int => Some(NumericVal::Int((raw * eu_scale + eu_offset) as i64)),
        SampleType::Bool => Some(NumericVal::Bool(raw != 0.0)),
        SampleType::Text => None,
    }
}

fn extract_text(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}
```

**API notes:**
- `DynamicMessage::decode(msg_desc.clone(), data)` — if this exact signature doesn't compile, use `{ let mut m = DynamicMessage::new(msg_desc.clone()); prost::Message::merge(&mut m, data)?; m }` instead.
- `msg.get_field(&field_desc).into_owned()` — `get_field` returns `Cow<'_, Value>`; `.into_owned()` materializes it. If the API returns `Value` directly, drop `.into_owned()`.
- `prost_reflect::Value` — verify the variants against `cargo doc --open` for your installed version; adjust match arms if variant names differ (e.g., `Value::String` might be `Value::String(Bytes)` in some versions — check).

- [ ] **Step 3: Run all decode tests**

Run: `cargo test decode`
Expected: all 4 decode tests pass.

Run: `cargo test`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/ingest/decode.rs src/ingest/loader.rs
git commit -m "feat(ingest): batch decoder with EU scaling and field path navigation"
```

---

### Task 5: ZMQ recv loop and app wire-up

**Files:**
- Modify: `src/ingest/thread.rs`
- Modify: `src/ingest/mod.rs` (implement `spawn_ingest`)
- Modify: `src/app.rs` (add `conn_state` field, update `new` signature, update toolbar)
- Modify: `src/main.rs` (parse CLI args, call `spawn_ingest`)

**Interfaces:**
- Consumes: `zmq` crate, `ingest::{CONNECTING, LIVE, TIMEOUT, IngestConfig}`, `router::TopicRouter`, `decode::decode_batch`
- Produces:
  - `pub fn run_loop(endpoint: String, router: TopicRouter, store: Arc<dyn ChannelStore>, state: Arc<AtomicU8>)` — loops forever (panics only if the ZMQ context itself can't be created, which is unrecoverable)
  - `spawn_ingest` fully implemented: loads `ProtoSchema`, builds `TopicRouter`, spawns `thread::run_loop`, returns `IngestHandle`
  - `DataVisApp::new` gains a 6th parameter: `conn_state: Option<Arc<AtomicU8>>`
  - `src/main.rs` parses `--endpoint <url>` (default `tcp://localhost:5555`) and `--schema <path>` (default `schema.proto`); in demo mode skips `spawn_ingest` and passes `None` for conn_state

**ConnState transitions in run_loop:**
- On entry to the inner recv loop: `state = CONNECTING`
- After successful `recv_multipart`: `state = LIVE`; record `last_live = Instant::now()`
- After `EAGAIN` timeout (no message within 1s): if `last_live.elapsed() > 5s` → `state = TIMEOUT`
- After non-EAGAIN ZMQ error: `eprintln!`, break inner loop to trigger reconnect

**Reconnect backoff:**
- Start at 100ms, double each attempt, cap at 5000ms
- On successful recv (inner loop ran without error): reset backoff to 100ms

**ZMQ setup:**
```rust
let ctx = zmq::Context::new();
let socket = ctx.socket(zmq::SUB)?;
socket.set_rcvtimeo(1_000)?;          // 1s recv timeout
for topic in router.topics() {
    socket.set_subscribe(topic.as_bytes())?;
}
socket.connect(&endpoint)?;
```

- [ ] **Step 1: Write tests**

Add to the `tests` module in `src/ingest/mod.rs`:
```rust
#[test]
fn spawn_ingest_missing_schema_returns_err() {
    use std::sync::Arc;
    let registry = crate::config::ChannelRegistry::from_toml_str(r#"
[channels."x"]
topic = "t"
proto_path = "A.v"
ts_path = "A.t"
type = "float"
"#).unwrap();
    let store: Arc<dyn crate::store::ChannelStore> =
        Arc::new(crate::store::LiveStore::from_registry(&registry));
    let result = spawn_ingest(
        IngestConfig {
            endpoint: "tcp://localhost:55999".to_string(),
            proto_path: std::path::PathBuf::from("/nonexistent/schema.proto"),
        },
        &registry,
        store,
    );
    assert!(result.is_err(), "expected Err for missing schema file");
}

#[test]
fn ingest_handle_initial_state_is_connecting() {
    use std::sync::atomic::Ordering;
    let state = Arc::new(AtomicU8::new(CONNECTING));
    assert_eq!(state.load(Ordering::Relaxed), CONNECTING);
}
```

Run: `cargo test ingest::tests::spawn_ingest_missing_schema_returns_err`
Expected: FAIL (still returns "not yet implemented" Err, but we need it to return Err for the *right* reason — missing file)

Note: this test will pass once spawn_ingest tries to load the schema and fails. The current stub returns a generic Err, so the test actually passes. Verify: the test should pass even before Step 3.

Run: `cargo test ingest::tests`
Expected: both new tests PASS (the stub returns Err for any input, matching the missing-schema expectation).

- [ ] **Step 2: Implement thread.rs**

Replace `src/ingest/thread.rs` with:
```rust
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ingest::decode::decode_batch;
use crate::ingest::router::TopicRouter;
use crate::ingest::{CONNECTING, LIVE, TIMEOUT};
use crate::store::ChannelStore;

pub fn run_loop(
    endpoint: String,
    router: TopicRouter,
    store: Arc<dyn ChannelStore>,
    state: Arc<AtomicU8>,
) {
    let mut backoff_ms = 100u64;
    loop {
        state.store(CONNECTING, Ordering::Relaxed);
        match connect_and_recv(&endpoint, &router, store.as_ref(), &state) {
            Ok(()) => {
                backoff_ms = 100;
            }
            Err(e) => {
                eprintln!("ingest: recv loop error: {e}; reconnecting in {backoff_ms}ms");
                std::thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(5_000);
            }
        }
    }
}

fn connect_and_recv(
    endpoint: &str,
    router: &TopicRouter,
    store: &dyn ChannelStore,
    state: &Arc<AtomicU8>,
) -> anyhow::Result<()> {
    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::SUB)?;
    socket.set_rcvtimeo(1_000)?;
    for topic in router.topics() {
        socket.set_subscribe(topic.as_bytes())?;
    }
    socket.connect(endpoint)?;

    // Assume no data has arrived yet; treat as if last_live was 10s ago.
    let mut last_live = Instant::now() - Duration::from_secs(10);

    loop {
        match socket.recv_multipart(0) {
            Ok(parts) if parts.len() >= 2 => {
                let topic = std::str::from_utf8(&parts[0]).unwrap_or("");
                let bindings = router.bindings_for(topic);
                decode_batch(&parts[1], bindings, store);
                state.store(LIVE, Ordering::Relaxed);
                last_live = Instant::now();
            }
            Ok(_) => {
                // Malformed multipart (wrong frame count); ignore.
            }
            Err(zmq::Error::EAGAIN) => {
                if last_live.elapsed() > Duration::from_secs(5) {
                    state.store(TIMEOUT, Ordering::Relaxed);
                }
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }
}
```

- [ ] **Step 3: Implement spawn_ingest in mod.rs**

Replace the stub body of `spawn_ingest` in `src/ingest/mod.rs`:
```rust
pub fn spawn_ingest(
    config: IngestConfig,
    registry: &ChannelRegistry,
    store: Arc<dyn ChannelStore>,
) -> anyhow::Result<IngestHandle> {
    let schema = loader::ProtoSchema::from_path(&config.proto_path)?;
    let router = router::TopicRouter::build(registry, &schema);
    let conn_state = Arc::new(AtomicU8::new(CONNECTING));
    let state_clone = conn_state.clone();
    let endpoint = config.endpoint.clone();
    std::thread::spawn(move || {
        thread::run_loop(endpoint, router, store, state_clone);
    });
    Ok(IngestHandle { conn_state })
}
```

Also add `use std::sync::atomic::AtomicU8;` and `use std::sync::Arc;` at the top if not already present (they should be from the scaffold).

- [ ] **Step 4: Update app.rs**

Add `conn_state: Option<Arc<std::sync::atomic::AtomicU8>>` to `DataVisApp`:
```rust
pub struct DataVisApp {
    store: Arc<dyn ChannelStore>,
    channels: ChannelRegistry,
    registry: PanelRegistry,
    workspace: Workspace,
    layout_path: PathBuf,
    add_panel: AddPanelDialog,
    new_screen_name: String,
    status: String,
    conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
}
```

Update `DataVisApp::new` signature and body:
```rust
pub fn new(
    store: Arc<dyn ChannelStore>,
    channels: ChannelRegistry,
    registry: PanelRegistry,
    workspace: Workspace,
    layout_path: PathBuf,
    conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
) -> Self {
    let panel_type = registry
        .type_names()
        .first()
        .map(|s| s.to_string())
        .unwrap_or_default();
    Self {
        store,
        channels,
        registry,
        workspace,
        layout_path,
        add_panel: AddPanelDialog { panel_type, ..Default::default() },
        new_screen_name: String::new(),
        status: String::new(),
        conn_state,
    }
}
```

In `toolbar()`, replace the hardcoded `LIVE` label:
```rust
// Remove this line:
//   ui.colored_label(egui::Color32::LIGHT_GREEN, "LIVE");
// Replace with:
let (label, color) = match self
    .conn_state
    .as_ref()
    .map(|s| s.load(std::sync::atomic::Ordering::Relaxed))
{
    None | Some(crate::ingest::LIVE) => ("LIVE", egui::Color32::LIGHT_GREEN),
    Some(crate::ingest::CONNECTING) => ("CONNECTING", egui::Color32::YELLOW),
    Some(crate::ingest::TIMEOUT) => ("TIMEOUT", egui::Color32::RED),
    Some(_) => ("?", egui::Color32::GRAY),
};
ui.colored_label(color, label);
```

Update the tests in `app.rs` that call `DataVisApp::new` — add `None` as the last argument:
```rust
// In any test that constructs DataVisApp directly, pass None for conn_state.
// (The existing tests use build_panel_entry only, so no DataVisApp construction to update.)
```

- [ ] **Step 5: Update main.rs**

Replace `src/main.rs` with:
```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::anyhow;

use datavis::app::DataVisApp;
use datavis::config::{ChannelRegistry, LayoutConfig};
use datavis::ingest::IngestConfig;
use datavis::store::{ChannelStore, LiveStore};
use datavis::viz::PanelRegistry;
use datavis::workspace::Workspace;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let demo = args.iter().any(|a| a == "--demo");
    let endpoint =
        arg_value(&args, "--endpoint").unwrap_or_else(|| "tcp://localhost:5555".to_string());
    let schema_path =
        arg_value(&args, "--schema").unwrap_or_else(|| "schema.proto".to_string());
    let layout_path = PathBuf::from("layout.toml");

    let channels = ChannelRegistry::load(Path::new("channels.toml"))?;
    let layout = LayoutConfig::load(&layout_path)?;

    let store = Arc::new(LiveStore::from_registry(&channels));

    let conn_state = if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
        None
    } else {
        let config = IngestConfig {
            endpoint,
            proto_path: PathBuf::from(&schema_path),
        };
        match datavis::ingest::spawn_ingest(config, &channels, store.clone()) {
            Ok(handle) => Some(handle.conn_state),
            Err(e) => {
                eprintln!("ingest: failed to start ({e}); running without live data");
                None
            }
        }
    };

    let registry = PanelRegistry::with_builtins();
    let workspace = Workspace::from_config(&layout, &registry, &channels);
    let dyn_store: Arc<dyn ChannelStore> = store;
    let app = DataVisApp::new(dyn_store, channels, registry, workspace, layout_path, conn_state);

    eframe::run_native(
        "datavis",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}
```

- [ ] **Step 6: Run all tests**

Run: `cargo test`
Expected: all tests pass (including `spawn_ingest_missing_schema_returns_err` and `ingest_handle_initial_state_is_connecting`).

Run: `cargo build`
Expected: zero errors, zero warnings (fix any that appear).

Run: `cargo run -- --demo`
Expected: app launches, toolbar shows "LIVE" in green (demo mode → conn_state = None → defaults to LIVE label).

- [ ] **Step 7: Commit**

```bash
git add src/ingest/mod.rs src/ingest/thread.rs src/app.rs src/main.rs
git commit -m "feat(ingest): ZMQ SUB recv loop, reconnect backoff, and app status bar"
```

---

## Self-Review

**Spec coverage check:**

| Requirement | Task | Status |
|-------------|------|--------|
| ZMQ SUB socket, topic subscriptions | Task 5 thread.rs | ✓ |
| prost-reflect dynamic decode from .proto file | Task 2 loader.rs | ✓ |
| Field path navigation (proto_path, ts_path) | Task 2 ChannelDesc, Task 4 decode.rs | ✓ |
| EU scaling `raw * scale + offset` | Task 4 extract_numeric | ✓ |
| write_numeric / write_text to ChannelStore | Task 4 write_value | ✓ |
| Connection state tracking | Task 1 AtomicU8 constants, Task 5 thread.rs | ✓ |
| Reconnect with exponential backoff | Task 5 run_loop outer loop | ✓ |
| Decode failure → log + skip, no crash | Task 4 decode_batch | ✓ |
| Unknown topic → silently skip | Task 5: empty bindings_for → decode_batch returns early | ✓ |
| Missing timestamp → skip sample | Task 4 decode_batch_channel: continue on None ts | ✓ |
| Topic grouping (N channels per topic) | Task 3 TopicRouter: Vec<ChannelBinding> per topic | ✓ |
| New deps zmq, prost-reflect, protox | Task 1 Cargo.toml | ✓ |
| Connection state visible in toolbar | Task 5 app.rs | ✓ |

**Placeholder scan:** None found.

**Type consistency:**
- `ChannelDesc.val_path: Vec<String>` → `ChannelBinding.val_path: Vec<String>` → used in decode.rs ✓
- `IngestHandle.conn_state: Arc<AtomicU8>` → passed to `DataVisApp::new` as `Option<Arc<AtomicU8>>` ✓
- `DataVisApp::new` 6-arg signature → `main.rs` passes 6 args ✓
- `decode_batch(data: &[u8], bindings: &[ChannelBinding], store: &dyn ChannelStore) -> usize` → called from thread.rs ✓
