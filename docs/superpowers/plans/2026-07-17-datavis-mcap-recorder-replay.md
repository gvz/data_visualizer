# MCAP Recorder + Replay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add manual record-to-MCAP and full-file playback to datavis; viz panels require zero code changes.

**Architecture:** Ingest thread pushes raw ZMQ bytes into a bounded SPSC queue when recording is active; a recorder thread drains the queue and writes MCAP frames (one channel per ZMQ topic, embedded proto schema). Replay loads the entire file into a `PlaybackStore` that implements `ChannelStore`; the app swaps `self.store` so panels are none the wiser. Playback position is advanced in `DataVisApp::update()`.

**Tech Stack:** `mcap = "0.8"`, `crossbeam-channel = "0.5"`, `rfd = "0.14"`, `prost-reflect` (already present)

## Global Constraints

- Rust stable (1.97.1, `nix develop` to enter shell)
- `cargo test` must pass at end of every task
- No Co-Authored-By in commit messages; no emoji in commit messages
- `QUEUE_CAP = 8192` (bounded SPSC queue cap)
- Flush interval: 1 second
- `RecordMsg = (Arc<str>, Vec<u8>, i64)` — (topic, raw_proto_bytes, log_time_ns)
- MCAP schema encoding: `"protobuf"`, message encoding: `"protobuf"`
- Schema data = FileDescriptorSet bytes from `protox::compile` (same pool used by ingest)
- `PlaybackStore::latest()` returns last sample at-or-before `position_ns`, not the absolute last
- Do not change any `src/viz/*.rs` files — panels must work with replay unchanged
- `prost` accessible via `prost_reflect::prost` (no direct `prost` dep needed)
- `rfd` file dialog blocks briefly on the main thread — that is acceptable for v1
- Recordings saved as `recording_<YYYYMMDDTHHmmss>.mcap` in the current working directory

---

### Task 1: Deps + record module scaffold + queue

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`
- Create: `src/record/mod.rs`
- Create: `src/record/queue.rs`
- Create: `src/record/writer.rs` (empty stub — filled in Task 3)
- Create: `src/record/playback.rs` (empty stub — filled in Task 4)

**Interfaces:**
- Produces:
  - `crate::record::RecordMsg` = `(Arc<str>, Vec<u8>, i64)`
  - `crate::record::record_channel() -> (Sender<RecordMsg>, Receiver<RecordMsg>)`
  - `crate::record::QUEUE_CAP: usize` = `8192`
  - `crate::record::RecordHandle { pub gap_count: Arc<AtomicU64>, pub record_failed: Arc<AtomicBool> }`
  - `crate::record::start_recording(...)` — stub returning `Err(anyhow!("not yet implemented"))`

- [ ] **Step 1: Add deps to Cargo.toml**

```toml
# In [dependencies]:
mcap = "0.8"
rfd = "0.14"
crossbeam-channel = "0.5"
```

Full `[dependencies]` block after change:
```toml
[dependencies]
eframe = "0.28"
serde = { version = "1.0", features = ["derive"] }
toml = "0.8"
anyhow = "1.0"
egui_plot = "0.28"
rustfft = "6"
serde_json = "1"
egui_tiles = { version = "0.9", features = ["serde"] }
zmq = "0.10"
prost-reflect = "0.14"
protox = "0.7"
mcap = "0.8"
rfd = "0.14"
crossbeam-channel = "0.5"
```

- [ ] **Step 2: Write failing tests in `src/record/queue.rs`**

```rust
use std::sync::Arc;
use crossbeam_channel::{Receiver, Sender};

pub type RecordMsg = (Arc<str>, Vec<u8>, i64);

pub const QUEUE_CAP: usize = 8192;

pub fn record_channel() -> (Sender<RecordMsg>, Receiver<RecordMsg>) {
    crossbeam_channel::bounded(QUEUE_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_roundtrip() {
        let (tx, rx) = record_channel();
        let topic: Arc<str> = Arc::from("accel");
        let data = vec![1u8, 2, 3];
        tx.try_send((topic.clone(), data.clone(), 42_000_000)).unwrap();
        let (t, d, n) = rx.try_recv().unwrap();
        assert_eq!(t.as_ref(), "accel");
        assert_eq!(d, data);
        assert_eq!(n, 42_000_000);
    }

    #[test]
    fn queue_full_returns_err_does_not_block() {
        let (tx, _rx) = record_channel();
        for i in 0..QUEUE_CAP {
            tx.try_send((Arc::from("t"), vec![i as u8], i as i64)).unwrap();
        }
        assert!(tx.try_send((Arc::from("t"), vec![], 0)).is_err());
    }
}
```

- [ ] **Step 3: Create `src/record/mod.rs`**

```rust
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

pub mod playback;
pub mod queue;
pub mod writer;

pub use queue::{record_channel, RecordMsg, QUEUE_CAP};

pub struct RecordHandle {
    pub gap_count: Arc<AtomicU64>,
    pub record_failed: Arc<AtomicBool>,
    stop_tx: crossbeam_channel::Sender<()>,
}

impl Drop for RecordHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
    }
}

/// Stub — implemented in Task 3.
pub fn start_recording(
    _output_dir: &std::path::Path,
    _registry: &crate::config::ChannelRegistry,
    _schema_bytes: Vec<u8>,
    _receiver: crossbeam_channel::Receiver<RecordMsg>,
) -> anyhow::Result<RecordHandle> {
    Err(anyhow::anyhow!("start_recording: not yet implemented"))
}
```

- [ ] **Step 4: Create stub files**

`src/record/writer.rs`:
```rust
// Filled in Task 3.
```

`src/record/playback.rs`:
```rust
// Filled in Task 4.
```

- [ ] **Step 5: Add `pub mod record` to `src/lib.rs`**

Current `src/lib.rs`:
```rust
pub mod app;
pub mod config;
pub mod demo;
pub mod ingest;
pub mod store;
pub mod types;
pub mod viz;
pub mod workspace;
```

Add one line:
```rust
pub mod app;
pub mod config;
pub mod demo;
pub mod ingest;
pub mod record;
pub mod store;
pub mod types;
pub mod viz;
pub mod workspace;
```

- [ ] **Step 6: Run tests**

```bash
cargo test 2>&1 | tail -5
```

Expected: all tests pass including `queue_roundtrip` and `queue_full_returns_err_does_not_block`.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/record/
git commit -m "feat: add record module scaffold, queue, and new cargo deps"
```

---

### Task 2: ProtoSchema::schema\_bytes + ChannelStore::now\_ns

**Files:**
- Modify: `src/ingest/loader.rs`
- Modify: `src/store/mod.rs`

**Interfaces:**
- Consumes: existing `ProtoSchema { pool: DescriptorPool }`, existing `ChannelStore` trait
- Produces:
  - `ProtoSchema::schema_bytes(&self) -> &[u8]` — FileDescriptorSet bytes for embedding in MCAP
  - `ChannelStore::now_ns(&self) -> i64` — default impl returns wall clock; `PlaybackStore` will override

Note: viz panels do NOT call `crate::types::now_ns()` — they anchor their windows on `store.latest()`. No panel changes are needed.

- [ ] **Step 1: Write failing tests**

Add to `src/ingest/loader.rs` test module (append inside the existing `#[cfg(test)] mod tests { ... }` block):

```rust
#[test]
fn schema_bytes_is_valid_file_descriptor_set() {
    use prost_reflect::prost::Message as _;
    let dir = tempfile::tempdir().unwrap();
    let path = write_test_proto(dir.path(), r#"
syntax = "proto3";
message Ping { int64 t_ns = 1; float v = 2; }
"#);
    let schema = ProtoSchema::from_path(&path).unwrap();
    let bytes = schema.schema_bytes();
    assert!(!bytes.is_empty());
    // Must decode back to a valid FileDescriptorSet.
    let fds = prost_types::FileDescriptorSet::decode(bytes).unwrap();
    assert!(!fds.file.is_empty());
}
```

Add to `src/store/mod.rs` test module (new block):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LiveStore;
    use crate::config::ChannelRegistry;

    fn empty_registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(r#"
[channels."x"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap()
    }

    #[test]
    fn live_store_now_ns_returns_wall_clock() {
        let reg = empty_registry();
        let store = LiveStore::from_registry(&reg);
        let before = crate::types::now_ns();
        let got = store.now_ns();
        let after = crate::types::now_ns();
        assert!(got >= before, "now_ns should be >= before");
        assert!(got <= after, "now_ns should be <= after");
    }
}
```

- [ ] **Step 2: Run to see failures**

```bash
cargo test schema_bytes_is_valid 2>&1 | tail -10
cargo test live_store_now_ns 2>&1 | tail -10
```

Expected: both fail (method not found / module not found).

- [ ] **Step 3: Update `ProtoSchema` in `src/ingest/loader.rs`**

Change the struct and `from_path`:

Old struct:
```rust
pub struct ProtoSchema {
    pool: DescriptorPool,
}
```

New struct:
```rust
pub struct ProtoSchema {
    pool: DescriptorPool,
    schema_bytes: Vec<u8>,
}
```

Old `from_path`:
```rust
pub fn from_path(proto_file: &Path) -> anyhow::Result<Self> {
    let include_dir = proto_file.parent().unwrap_or(Path::new("."));
    let fds = protox::compile([proto_file], [include_dir])
        .with_context(|| format!("compiling proto schema {}", proto_file.display()))?;
    let pool = DescriptorPool::from_file_descriptor_set(fds)
        .context("building descriptor pool from compiled schema")?;
    Ok(Self { pool })
}
```

New `from_path` (add `use prost_reflect::prost::Message as _;` at the top of the method):
```rust
pub fn from_path(proto_file: &Path) -> anyhow::Result<Self> {
    use prost_reflect::prost::Message as _;
    let include_dir = proto_file.parent().unwrap_or(Path::new("."));
    let fds = protox::compile([proto_file], [include_dir])
        .with_context(|| format!("compiling proto schema {}", proto_file.display()))?;
    let schema_bytes = fds.encode_to_vec();
    let pool = DescriptorPool::from_file_descriptor_set(fds)
        .context("building descriptor pool from compiled schema")?;
    Ok(Self { pool, schema_bytes })
}
```

Add method after `resolve`:
```rust
pub fn schema_bytes(&self) -> &[u8] {
    &self.schema_bytes
}
```

Also add `prost_types` import at the top of the file (needed for the decode-back test):
```rust
use prost_types;
```

Wait — `prost_types` is already a transitive dep via `prost-reflect`. To use it in tests, add to `Cargo.toml` dev-dependencies if needed, or reference via `prost_reflect::prost_types`. Actually, the test uses `prost_types::FileDescriptorSet::decode`. Check if `prost_types` is directly available: it is, as it is re-exported by `prost-reflect` via its own dep tree, and `protox` itself brings it in. Add it to dev-deps to be explicit:

In `Cargo.toml` `[dev-dependencies]`:
```toml
prost-types = "0.13"
```

(Check the prost-types version used by prost-reflect 0.14; it should be 0.13.x. Run `cargo tree -p prost-types` to confirm and use the same major.)

- [ ] **Step 4: Add `now_ns` default to `ChannelStore` trait in `src/store/mod.rs`**

Old trait:
```rust
pub trait ChannelStore: Send + Sync {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal);
    fn write_text(&self, channel: ChannelId, ts: i64, line: String);
    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot;
    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)>;
    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta;
}
```

New trait (add `now_ns` with default):
```rust
pub trait ChannelStore: Send + Sync {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal);
    fn write_text(&self, channel: ChannelId, ts: i64, line: String);
    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot;
    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)>;
    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta;
    /// Wall clock by default; PlaybackStore overrides to return playback position.
    fn now_ns(&self) -> i64 {
        crate::types::now_ns()
    }
}
```

- [ ] **Step 5: Run tests**

```bash
cargo test schema_bytes_is_valid 2>&1 | tail -10
cargo test live_store_now_ns 2>&1 | tail -10
cargo test 2>&1 | tail -5
```

Expected: both new tests pass, all existing tests pass.

If `prost_types::FileDescriptorSet` not found, run `cargo tree -p prost-types` to find the correct import path and adjust the test.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/ingest/loader.rs src/store/mod.rs
git commit -m "feat: ProtoSchema captures schema bytes; ChannelStore gains now_ns default"
```

---

### Task 3: MCAP writer + start\_recording

**Files:**
- Create: `src/record/writer.rs`
- Modify: `src/record/mod.rs` (replace stub `start_recording`)

**Interfaces:**
- Consumes: `RecordMsg`, `QUEUE_CAP`, `RecordHandle`, `ChannelRegistry`, `schema_bytes: &[u8]`
- Produces:
  - `McapRecorder` (internal to writer.rs — not pub)
  - `start_recording(output_dir, registry, schema_bytes, receiver) -> anyhow::Result<RecordHandle>`

- [ ] **Step 1: Write failing tests in `src/record/writer.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::record::queue::record_channel;
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn minimal_registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(r#"
[channels."x"]
topic = "accel"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap()
    }

    #[test]
    fn roundtrip_write_read() {
        let dir = tempfile::tempdir().unwrap();
        let registry = minimal_registry();
        let schema_bytes = b"fake_fds_bytes".to_vec();

        let (tx, rx) = record_channel();
        let handle = start_recording(dir.path(), &registry, schema_bytes, rx).unwrap();

        tx.try_send((Arc::from("accel"), vec![0x01, 0x02], 1_000_000_000)).unwrap();
        tx.try_send((Arc::from("accel"), vec![0x03, 0x04], 2_000_000_000)).unwrap();

        // Drop sender and handle to stop recording and flush file.
        drop(tx);
        drop(handle);

        // Give recorder thread time to finish writing.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Find the written file.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "mcap").unwrap_or(false))
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one .mcap file");

        let mcap_path = entries[0].path();
        let bytes = std::fs::read(&mcap_path).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&bytes)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].channel.topic, "accel");
        assert_eq!(messages[0].data.as_ref(), &[0x01u8, 0x02]);
        assert_eq!(messages[0].log_time, 1_000_000_000u64);
        assert_eq!(messages[1].data.as_ref(), &[0x03u8, 0x04]);
        assert_eq!(messages[1].log_time, 2_000_000_000u64);
    }

    #[test]
    fn unknown_topic_messages_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let registry = minimal_registry();
        let (tx, rx) = record_channel();
        let handle = start_recording(dir.path(), &registry, vec![], rx).unwrap();

        // "gyro" is not in the registry — should be silently dropped
        tx.try_send((Arc::from("gyro"), vec![0xFF], 1_000)).unwrap();
        tx.try_send((Arc::from("accel"), vec![0x01], 2_000)).unwrap();
        drop(tx);
        drop(handle);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "mcap").unwrap_or(false))
            .collect();
        let bytes = std::fs::read(&entries[0].path()).unwrap();
        let messages: Vec<_> = mcap::MessageStream::new(&bytes).unwrap()
            .collect::<Result<_, _>>().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].channel.topic, "accel");
    }
}
```

- [ ] **Step 2: Run to see failure**

```bash
cargo test record::writer 2>&1 | tail -10
```

Expected: compile error (McapRecorder, start_recording not defined).

- [ ] **Step 3: Implement `src/record/writer.rs`**

```rust
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, BufWriter};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::config::ChannelRegistry;
use crate::record::queue::RecordMsg;

struct McapRecorder {
    writer: mcap::Writer<BufWriter<File>>,
    channel_refs: HashMap<String, Arc<mcap::Channel>>,
    last_flush: Instant,
    sequence: u32,
}

impl McapRecorder {
    fn new(path: &Path, registry: &ChannelRegistry, schema_bytes: &[u8]) -> anyhow::Result<Self> {
        let file = BufWriter::new(File::create(path)?);
        let mut writer = mcap::Writer::new(file)?;

        let schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(schema_bytes.to_vec()),
        });

        let mut channel_refs = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for id in registry.iter_ids() {
            let topic = registry.config(id).topic.clone();
            if seen.insert(topic.clone()) {
                let channel = Arc::new(mcap::Channel {
                    topic: topic.clone(),
                    schema: Some(schema.clone()),
                    message_encoding: "protobuf".to_string(),
                    metadata: BTreeMap::new(),
                });
                writer.add_channel(&channel)?;
                channel_refs.insert(topic, channel);
            }
        }

        Ok(Self { writer, channel_refs, last_flush: Instant::now(), sequence: 0 })
    }

    fn write_msg(&mut self, topic: &str, data: &[u8], log_time_ns: i64) -> anyhow::Result<()> {
        let Some(channel) = self.channel_refs.get(topic) else {
            return Ok(()); // unknown topic, skip
        };
        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        self.writer.write(&mcap::Message {
            channel: channel.clone(),
            sequence: seq,
            log_time: log_time_ns as u64,
            publish_time: log_time_ns as u64,
            data: Cow::Borrowed(data),
        })?;
        if self.last_flush.elapsed() >= Duration::from_secs(1) {
            self.writer.flush()?;
            self.last_flush = Instant::now();
        }
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        self.writer.finish()?;
        Ok(())
    }
}

pub(super) fn recorder_thread_fn(
    recorder: McapRecorder,
    record_rx: Receiver<RecordMsg>,
    stop_rx: crossbeam_channel::Receiver<()>,
    record_failed: Arc<AtomicBool>,
) {
    if let Err(e) = recorder_loop(recorder, record_rx, stop_rx) {
        eprintln!("recorder: write error: {e}");
        record_failed.store(true, Ordering::Relaxed);
    }
}

fn recorder_loop(
    mut recorder: McapRecorder,
    record_rx: Receiver<RecordMsg>,
    stop_rx: crossbeam_channel::Receiver<()>,
) -> anyhow::Result<()> {
    loop {
        crossbeam_channel::select! {
            recv(record_rx) -> result => match result {
                Ok((topic, data, ts)) => recorder.write_msg(&topic, &data, ts)?,
                Err(_) => break,
            },
            recv(stop_rx) -> _ => break,
        }
    }
    // Drain any messages that arrived before stop.
    while let Ok((topic, data, ts)) = record_rx.try_recv() {
        recorder.write_msg(&topic, &data, ts)?;
    }
    recorder.finish()
}

/// Called from `src/record/mod.rs::start_recording`.
pub(super) fn spawn_recorder(
    output_dir: &Path,
    registry: &ChannelRegistry,
    schema_bytes: &[u8],
    receiver: Receiver<RecordMsg>,
    gap_count: Arc<AtomicU64>,
    record_failed: Arc<AtomicBool>,
) -> anyhow::Result<crossbeam_channel::Sender<()>> {
    let now = chrono::Local::now();
    let filename = format!("recording_{}.mcap", now.format("%Y%m%dT%H%M%S"));
    let path = output_dir.join(filename);
    let recorder = McapRecorder::new(&path, registry, schema_bytes)?;
    let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let rf = record_failed.clone();
    std::thread::spawn(move || recorder_thread_fn(recorder, receiver, stop_rx, rf));
    Ok(stop_tx)
}
```

Note: `chrono` is not in the deps. Use `std::time::SystemTime` instead for the timestamp:

Replace the `chrono`-based filename generation with:
```rust
use std::time::{SystemTime, UNIX_EPOCH};
let secs = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs();
let filename = format!("recording_{secs}.mcap");
```

- [ ] **Step 4: Replace stub `start_recording` in `src/record/mod.rs`**

Old stub:
```rust
pub fn start_recording(
    _output_dir: &std::path::Path,
    _registry: &crate::config::ChannelRegistry,
    _schema_bytes: Vec<u8>,
    _receiver: crossbeam_channel::Receiver<RecordMsg>,
) -> anyhow::Result<RecordHandle> {
    Err(anyhow::anyhow!("start_recording: not yet implemented"))
}
```

New implementation:
```rust
pub fn start_recording(
    output_dir: &std::path::Path,
    registry: &crate::config::ChannelRegistry,
    schema_bytes: Vec<u8>,
    receiver: crossbeam_channel::Receiver<RecordMsg>,
) -> anyhow::Result<RecordHandle> {
    let gap_count = Arc::new(AtomicU64::new(0));
    let record_failed = Arc::new(AtomicBool::new(false));
    let stop_tx = writer::spawn_recorder(
        output_dir,
        registry,
        &schema_bytes,
        receiver,
        gap_count.clone(),
        record_failed.clone(),
    )?;
    Ok(RecordHandle { gap_count, record_failed, stop_tx })
}
```

Also update `RecordHandle` to add `stop_tx` field (private):
```rust
pub struct RecordHandle {
    pub gap_count: Arc<AtomicU64>,
    pub record_failed: Arc<AtomicBool>,
    stop_tx: crossbeam_channel::Sender<()>,
}
```

And the Drop impl was already added in Task 1. Verify it is present:
```rust
impl Drop for RecordHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
    }
}
```

- [ ] **Step 5: Fix the mcap API if needed**

The `mcap::Writer::add_channel` method may not exist in mcap 0.8 — channels are registered implicitly on first `write`. If `add_channel` does not exist, remove those calls and let the writer register channels on first write. Check:

```bash
cargo doc --open -p mcap 2>/dev/null || cargo check 2>&1 | head -20
```

If `add_channel` is missing: remove the `writer.add_channel(&channel)?;` lines from `McapRecorder::new`. The channels will be registered automatically when `writer.write(...)` is first called.

Also verify `mcap::Writer::flush` exists; if not, remove flush calls (writer flushes on finish).

- [ ] **Step 6: Run tests**

```bash
cargo test record::writer 2>&1 | tail -15
cargo test 2>&1 | tail -5
```

Expected: `roundtrip_write_read` and `unknown_topic_messages_are_skipped` pass.

- [ ] **Step 7: Commit**

```bash
git add src/record/mod.rs src/record/writer.rs
git commit -m "feat: MCAP recorder thread, McapRecorder, start_recording"
```

---

### Task 4: PlaybackStore

**Files:**
- Create: `src/record/playback.rs`

**Interfaces:**
- Consumes:
  - `decode_batch(data: &[u8], bindings: &[ChannelBinding], store: &dyn ChannelStore) -> usize` from `crate::ingest::decode`
  - `TopicRouter::build(registry, schema)` + `router.bindings_for(topic)` from `crate::ingest::router`
  - `ProtoSchema` from `crate::ingest::loader`
  - `ChannelStore` trait (will impl)
  - `ChannelRegistry::iter_ids()`, `ChannelRegistry::meta(id) -> &ChannelMeta`
  - `ChannelMeta { sample_type: SampleType, ... }`
  - `ChannelSnapshot`, `TimeWindow`, `ChannelId`, `Sample`, `NumericVal`, `SampleType`
- Produces:
  - `PlaybackStore` implementing `ChannelStore`
  - `PlaybackStore::load(path, registry, schema) -> anyhow::Result<Arc<PlaybackStore>>`
  - `PlaybackStore::position_ns: Arc<AtomicI64>` (pub field — app holds clone to advance)
  - `PlaybackStore::duration_ns: i64` (pub field)
  - `PlaybackStore::start_ns: i64` (pub field)

**Important behavior:** `PlaybackStore::latest(channel)` returns the last sample whose timestamp is `<= position_ns.load(Relaxed)`. `PlaybackStore::snapshot(channel, window)` binary-searches the sorted ts array. `PlaybackStore::now_ns()` returns `position_ns.load(Relaxed)`. These three together make viz panels show data anchored at the current playback position without any panel code changes.

- [ ] **Step 1: Write failing tests**

`src/record/playback.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::loader::ProtoSchema;
    use crate::types::{ChannelSnapshot, TimeWindow};
    use std::io::Write;
    use std::sync::atomic::Ordering;

    fn make_proto_and_registry() -> (ProtoSchema, tempfile::TempDir, ChannelRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message AccelBatch {{
  repeated Sample samples = 1;
  message Sample {{ int64 t_ns = 1; float x = 2; }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
        (schema, dir, registry)
    }

    fn write_test_mcap(
        path: &std::path::Path,
        schema: &ProtoSchema,
        messages: &[(i64, f32)],  // (t_ns, x)
    ) {
        use crate::ingest::router::TopicRouter;
        use prost_reflect::prost::Message as _;
        use prost_reflect::{DynamicMessage, Value};
        use std::borrow::Cow;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("AccelBatch").unwrap();
        let sample_desc = pool.get_message_by_name("AccelBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let x_field = sample_desc.get_field_by_name("x").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();

        let mcap_schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Borrowed(&[]),
        });
        let channel = Arc::new(mcap::Channel {
            topic: "accel".to_string(),
            schema: Some(mcap_schema),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        });
        let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut writer = mcap::Writer::new(file).unwrap();
        for (t_ns, x) in messages {
            let list = vec![{
                let mut s = DynamicMessage::new(sample_desc.clone());
                s.set_field(&t_field, Value::I64(*t_ns));
                s.set_field(&x_field, Value::F32(*x));
                Value::Message(s)
            }];
            let mut batch = DynamicMessage::new(batch_desc.clone());
            batch.set_field(&samples_field, Value::List(list));
            let data = batch.encode_to_vec();
            writer.write(&mcap::Message {
                channel: channel.clone(),
                sequence: 0,
                log_time: *t_ns as u64,
                publish_time: *t_ns as u64,
                data: Cow::Owned(data),
            }).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn load_and_snapshot_returns_data_in_window() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (2_000_000_000, 2.0),
            (3_000_000_000, 3.0),
        ]);
        let store = PlaybackStore::load(&path, &registry, &schema).unwrap();
        let id = registry.id("accel.x").unwrap();
        let window = TimeWindow { start_ns: 1_000_000_000, end_ns: 3_000_000_000 };
        let snap = store.snapshot(id, window);
        match snap {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts.len(), 2);
                assert_eq!(ts[0], 1_000_000_000);
                assert_eq!(ts[1], 2_000_000_000);
                // EU scale 1.0, offset 0.0 (defaults) → values unchanged
                assert!((vals[0] - 1.0_f64).abs() < 1e-4);
                assert!((vals[1] - 2.0_f64).abs() < 1e-4);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn now_ns_returns_position_not_wall_clock() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[(1_000_000_000, 1.0)]);
        let store = PlaybackStore::load(&path, &registry, &schema).unwrap();
        let expected_pos = 42_999_999_999i64;
        store.position_ns.store(expected_pos, Ordering::Relaxed);
        assert_eq!(store.now_ns(), expected_pos);
    }

    #[test]
    fn latest_returns_sample_at_or_before_position() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (3_000_000_000, 3.0),
        ]);
        let store = PlaybackStore::load(&path, &registry, &schema).unwrap();
        let id = registry.id("accel.x").unwrap();

        // Position before any sample → None
        store.position_ns.store(0, Ordering::Relaxed);
        assert!(store.latest(id).is_none());

        // Position at first sample → returns first
        store.position_ns.store(1_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest(id).unwrap();
        assert_eq!(ts, 1_000_000_000);

        // Position between samples → returns first
        store.position_ns.store(2_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest(id).unwrap();
        assert_eq!(ts, 1_000_000_000);

        // Position at second sample → returns second
        store.position_ns.store(3_000_000_000, Ordering::Relaxed);
        let (ts, _) = store.latest(id).unwrap();
        assert_eq!(ts, 3_000_000_000);
    }

    #[test]
    fn duration_and_start_ns_computed_from_data() {
        let (schema, _dir, registry) = make_proto_and_registry();
        let dir2 = tempfile::tempdir().unwrap();
        let path = dir2.path().join("test.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (4_000_000_000, 4.0),
        ]);
        let store = PlaybackStore::load(&path, &registry, &schema).unwrap();
        assert_eq!(store.start_ns, 1_000_000_000);
        assert_eq!(store.duration_ns, 3_000_000_000);
    }
}
```

- [ ] **Step 2: Run to see failures**

```bash
cargo test record::playback 2>&1 | tail -10
```

Expected: compile error (PlaybackStore not defined).

- [ ] **Step 3: Implement `src/record/playback.rs`**

```rust
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;

use crate::config::ChannelRegistry;
use crate::ingest::decode::decode_batch;
use crate::ingest::loader::ProtoSchema;
use crate::ingest::router::TopicRouter;
use crate::store::ChannelStore;
use crate::types::{
    ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, SampleType, TimeWindow,
};

enum PlaybackChannel {
    Float { ts: Vec<i64>, vals: Vec<f64> },
    Int { ts: Vec<i64>, vals: Vec<i64> },
    Bool { ts: Vec<i64>, vals: Vec<u8> },
    Text { lines: Vec<(i64, String)> },
}

impl PlaybackChannel {
    fn for_type(sample_type: SampleType) -> Self {
        match sample_type {
            SampleType::Float => PlaybackChannel::Float { ts: vec![], vals: vec![] },
            SampleType::Int => PlaybackChannel::Int { ts: vec![], vals: vec![] },
            SampleType::Bool => PlaybackChannel::Bool { ts: vec![], vals: vec![] },
            SampleType::Text => PlaybackChannel::Text { lines: vec![] },
        }
    }
}

pub struct PlaybackStore {
    channels: Vec<Mutex<PlaybackChannel>>,
    metas: Vec<ChannelMeta>,
    pub position_ns: Arc<AtomicI64>,
    pub duration_ns: i64,
    pub start_ns: i64,
}

impl PlaybackStore {
    fn new(registry: &ChannelRegistry) -> Self {
        let channels = registry
            .iter_ids()
            .map(|id| Mutex::new(PlaybackChannel::for_type(registry.meta(id).sample_type)))
            .collect();
        let metas = registry.iter_ids().map(|id| registry.meta(id).clone()).collect();
        Self {
            channels,
            metas,
            position_ns: Arc::new(AtomicI64::new(0)),
            duration_ns: 0,
            start_ns: 0,
        }
    }

    fn sort_and_finalize(&mut self) {
        let mut global_min = i64::MAX;
        let mut global_max = i64::MIN;

        for ch in &self.channels {
            let mut ch = ch.lock().unwrap();
            match &mut *ch {
                PlaybackChannel::Float { ts, vals } => {
                    let mut pairs: Vec<(i64, f64)> =
                        ts.iter().copied().zip(vals.iter().copied()).collect();
                    pairs.sort_unstable_by_key(|(t, _)| *t);
                    *ts = pairs.iter().map(|(t, _)| *t).collect();
                    *vals = pairs.iter().map(|(_, v)| *v).collect();
                    if let (Some(&mn), Some(&mx)) = (ts.first(), ts.last()) {
                        global_min = global_min.min(mn);
                        global_max = global_max.max(mx);
                    }
                }
                PlaybackChannel::Int { ts, vals } => {
                    let mut pairs: Vec<(i64, i64)> =
                        ts.iter().copied().zip(vals.iter().copied()).collect();
                    pairs.sort_unstable_by_key(|(t, _)| *t);
                    *ts = pairs.iter().map(|(t, _)| *t).collect();
                    *vals = pairs.iter().map(|(_, v)| *v).collect();
                    if let (Some(&mn), Some(&mx)) = (ts.first(), ts.last()) {
                        global_min = global_min.min(mn);
                        global_max = global_max.max(mx);
                    }
                }
                PlaybackChannel::Bool { ts, vals } => {
                    let mut pairs: Vec<(i64, u8)> =
                        ts.iter().copied().zip(vals.iter().copied()).collect();
                    pairs.sort_unstable_by_key(|(t, _)| *t);
                    *ts = pairs.iter().map(|(t, _)| *t).collect();
                    *vals = pairs.iter().map(|(_, v)| *v).collect();
                    if let (Some(&mn), Some(&mx)) = (ts.first(), ts.last()) {
                        global_min = global_min.min(mn);
                        global_max = global_max.max(mx);
                    }
                }
                PlaybackChannel::Text { lines } => {
                    lines.sort_unstable_by_key(|(t, _)| *t);
                    if let (Some((mn, _)), Some((mx, _))) = (lines.first(), lines.last()) {
                        global_min = global_min.min(*mn);
                        global_max = global_max.max(*mx);
                    }
                }
            }
        }

        if global_min <= global_max {
            self.start_ns = global_min;
            self.duration_ns = global_max - global_min;
            self.position_ns.store(global_min, Ordering::Relaxed);
        }
    }

    pub fn load(
        path: &Path,
        registry: &ChannelRegistry,
        schema: &ProtoSchema,
    ) -> anyhow::Result<Arc<Self>> {
        let router = TopicRouter::build(registry, schema);
        let mut store = Self::new(registry);

        let bytes = std::fs::read(path)
            .with_context(|| format!("reading MCAP file {}", path.display()))?;
        for message in mcap::MessageStream::new(&bytes)
            .context("opening MCAP message stream")?
        {
            let msg = message.context("reading MCAP message")?;
            let bindings = router.bindings_for(&msg.channel.topic);
            decode_batch(&msg.data, bindings, &store);
        }

        store.sort_and_finalize();
        Ok(Arc::new(store))
    }
}

impl ChannelStore for PlaybackStore {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal) {
        let mut ch = self.channels[channel.0 as usize].lock().unwrap();
        match (&mut *ch, val) {
            (PlaybackChannel::Float { ts: tvec, vals }, NumericVal::Float(v)) => {
                tvec.push(ts);
                vals.push(v);
            }
            (PlaybackChannel::Int { ts: tvec, vals }, NumericVal::Int(v)) => {
                tvec.push(ts);
                vals.push(v);
            }
            (PlaybackChannel::Bool { ts: tvec, vals }, NumericVal::Bool(v)) => {
                tvec.push(ts);
                vals.push(v as u8);
            }
            _ => {}
        }
    }

    fn write_text(&self, channel: ChannelId, ts: i64, line: String) {
        let mut ch = self.channels[channel.0 as usize].lock().unwrap();
        if let PlaybackChannel::Text { lines } = &mut *ch {
            lines.push((ts, line));
        }
    }

    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot {
        let ch = self.channels[channel.0 as usize].lock().unwrap();
        match &*ch {
            PlaybackChannel::Float { ts, vals } => {
                let start = ts.partition_point(|&t| t < window.start_ns);
                let end = ts.partition_point(|&t| t < window.end_ns);
                ChannelSnapshot::Float {
                    ts: ts[start..end].to_vec(),
                    vals: vals[start..end].to_vec(),
                }
            }
            PlaybackChannel::Int { ts, vals } => {
                let start = ts.partition_point(|&t| t < window.start_ns);
                let end = ts.partition_point(|&t| t < window.end_ns);
                ChannelSnapshot::Int {
                    ts: ts[start..end].to_vec(),
                    vals: vals[start..end].to_vec(),
                }
            }
            PlaybackChannel::Bool { ts, vals } => {
                let start = ts.partition_point(|&t| t < window.start_ns);
                let end = ts.partition_point(|&t| t < window.end_ns);
                ChannelSnapshot::Bool {
                    ts: ts[start..end].to_vec(),
                    vals: vals[start..end].to_vec(),
                }
            }
            PlaybackChannel::Text { lines } => {
                let start = lines.partition_point(|(t, _)| *t < window.start_ns);
                let end = lines.partition_point(|(t, _)| *t < window.end_ns);
                ChannelSnapshot::Text { lines: lines[start..end].to_vec() }
            }
        }
    }

    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)> {
        let pos = self.position_ns.load(Ordering::Relaxed);
        let ch = self.channels[channel.0 as usize].lock().unwrap();
        match &*ch {
            PlaybackChannel::Float { ts, vals } => {
                // Last index where ts <= pos
                let idx = ts.partition_point(|&t| t <= pos);
                if idx == 0 { return None; }
                Some((ts[idx - 1], Sample::Float(vals[idx - 1])))
            }
            PlaybackChannel::Int { ts, vals } => {
                let idx = ts.partition_point(|&t| t <= pos);
                if idx == 0 { return None; }
                Some((ts[idx - 1], Sample::Int(vals[idx - 1])))
            }
            PlaybackChannel::Bool { ts, vals } => {
                let idx = ts.partition_point(|&t| t <= pos);
                if idx == 0 { return None; }
                Some((ts[idx - 1], Sample::Bool(vals[idx - 1] != 0)))
            }
            PlaybackChannel::Text { lines } => {
                let idx = lines.partition_point(|(t, _)| *t <= pos);
                if idx == 0 { return None; }
                Some((lines[idx - 1].0, Sample::Text(lines[idx - 1].1.clone())))
            }
        }
    }

    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta {
        &self.metas[channel.0 as usize]
    }

    fn now_ns(&self) -> i64 {
        self.position_ns.load(Ordering::Relaxed)
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test record::playback 2>&1 | tail -20
cargo test 2>&1 | tail -5
```

Expected: all 4 playback tests pass, no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/record/playback.rs
git commit -m "feat: PlaybackStore — load MCAP, ChannelStore impl with position-anchored replay"
```

---

### Task 5: Ingest thread integration

**Files:**
- Modify: `src/ingest/thread.rs`
- Modify: `src/ingest/mod.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes:
  - `RecordMsg`, `record_channel()` from `crate::record`
  - `ProtoSchema::schema_bytes()` from Task 2
- Produces:
  - `IngestHandle::record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>`
  - `IngestHandle::schema_bytes: Vec<u8>`
  - `spawn_ingest` unchanged external signature (returns same `IngestHandle` type, now richer)

The app installs a sender into `record_sender` when recording starts and takes it out when recording stops. Dropping the sender closes the queue, causing the recorder thread to finish naturally.

- [ ] **Step 1: Write failing tests**

Add to `src/ingest/mod.rs` test module:

```rust
#[test]
fn ingest_handle_has_record_sender() {
    // Just compile-checks the field types are accessible.
    let sender: Arc<std::sync::Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>> =
        Arc::new(std::sync::Mutex::new(None));
    drop(sender);
}

#[test]
fn schema_bytes_via_spawn_ingest_are_non_empty() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let proto_path = dir.path().join("test.proto");
    let mut f = std::fs::File::create(&proto_path).unwrap();
    write!(f, "syntax = \"proto3\";\nmessage M {{ int64 t = 1; float v = 2; }}\n").unwrap();
    let registry = crate::config::ChannelRegistry::from_toml_str(r#"
[channels."x"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap();
    let store: Arc<dyn crate::store::ChannelStore> =
        Arc::new(crate::store::LiveStore::from_registry(&registry));
    let handle = spawn_ingest(
        IngestConfig {
            endpoint: "tcp://localhost:59999".to_string(),
            proto_path,
        },
        &registry,
        store,
    ).unwrap();
    assert!(!handle.schema_bytes.is_empty(), "schema_bytes must be populated");
}
```

- [ ] **Step 2: Run to see failures**

```bash
cargo test ingest_handle_has_record_sender 2>&1 | tail -5
cargo test schema_bytes_via_spawn_ingest 2>&1 | tail -5
```

Expected: compile errors (fields don't exist yet).

- [ ] **Step 3: Update `IngestHandle` and `spawn_ingest` in `src/ingest/mod.rs`**

Add imports at top:
```rust
use std::sync::Mutex;
use crate::record::RecordMsg;
```

New `IngestHandle`:
```rust
pub struct IngestHandle {
    pub conn_state: Arc<AtomicU8>,
    pub record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
    pub schema_bytes: Vec<u8>,
}
```

New `spawn_ingest`:
```rust
pub fn spawn_ingest(
    config: IngestConfig,
    registry: &ChannelRegistry,
    store: Arc<dyn ChannelStore>,
) -> anyhow::Result<IngestHandle> {
    let schema = loader::ProtoSchema::from_path(&config.proto_path)?;
    let schema_bytes = schema.schema_bytes().to_vec();
    let router = router::TopicRouter::build(registry, &schema);
    let conn_state = Arc::new(AtomicU8::new(CONNECTING));
    let state_clone = conn_state.clone();
    let endpoint = config.endpoint.clone();
    let record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>> =
        Arc::new(Mutex::new(None));
    let record_sender_clone = record_sender.clone();
    std::thread::spawn(move || {
        thread::run_loop(endpoint, router, store, state_clone, record_sender_clone);
    });
    Ok(IngestHandle { conn_state, record_sender, schema_bytes })
}
```

- [ ] **Step 4: Update `run_loop` and `connect_and_recv` in `src/ingest/thread.rs`**

New signature:
```rust
pub fn run_loop(
    endpoint: String,
    router: TopicRouter,
    store: Arc<dyn ChannelStore>,
    state: Arc<AtomicU8>,
    record_sender: Arc<std::sync::Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>,
) {
    let mut backoff_ms = 100u64;
    loop {
        state.store(CONNECTING, Ordering::Relaxed);
        match connect_and_recv(&endpoint, &router, store.as_ref(), &state, &record_sender) {
            Ok(()) => { backoff_ms = 100; }
            Err(e) => {
                eprintln!("ingest: recv loop error: {e}; reconnecting in {backoff_ms}ms");
                std::thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(5_000);
            }
        }
    }
}
```

New `connect_and_recv` (add record push after `decode_batch`):
```rust
fn connect_and_recv(
    endpoint: &str,
    router: &TopicRouter,
    store: &dyn ChannelStore,
    state: &Arc<AtomicU8>,
    record_sender: &Arc<std::sync::Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>,
) -> anyhow::Result<()> {
    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::SUB)?;
    socket.set_rcvtimeo(1_000)?;
    for topic in router.topics() {
        socket.set_subscribe(topic.as_bytes())?;
    }
    socket.connect(endpoint)?;

    let mut last_live = Instant::now() - Duration::from_secs(10);

    loop {
        match socket.recv_multipart(0) {
            Ok(parts) if parts.len() >= 2 => {
                let topic = std::str::from_utf8(&parts[0]).unwrap_or("");
                let bindings = router.bindings_for(topic);
                decode_batch(&parts[1], bindings, store);
                state.store(LIVE, Ordering::Relaxed);
                last_live = Instant::now();

                // Push to recorder queue if recording is active.
                if let Ok(guard) = record_sender.try_lock() {
                    if let Some(tx) = guard.as_ref() {
                        let log_time_ns = crate::types::now_ns();
                        let topic_arc: std::sync::Arc<str> = topic.into();
                        let _ = tx.try_send((topic_arc, parts[1].clone(), log_time_ns));
                    }
                }
            }
            Ok(_) => {}
            Err(zmq::Error::EAGAIN) => {
                if last_live.elapsed() > Duration::from_secs(5) {
                    state.store(TIMEOUT, Ordering::Relaxed);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}
```

Also add the missing imports at top of `src/ingest/thread.rs`:
```rust
use std::sync::{Arc, Mutex};
```
(The existing file already imports `Arc`; add `Mutex` to the import.)

- [ ] **Step 5: Run tests**

```bash
cargo test ingest 2>&1 | tail -10
cargo test 2>&1 | tail -5
```

Expected: all existing ingest tests pass plus the two new ones.

- [ ] **Step 6: Commit**

```bash
git add src/ingest/mod.rs src/ingest/thread.rs src/main.rs
git commit -m "feat: ingest thread pushes RecordMsg to queue when recording; IngestHandle exposes record_sender and schema_bytes"
```

---

### Task 6: App record/replay UI

**Files:**
- Modify: `src/app.rs`
- Modify: `src/main.rs` (update DataVisApp::new call to pass IngestHandle)

**Interfaces:**
- Consumes:
  - `start_recording(dir, registry, schema_bytes, receiver) -> anyhow::Result<RecordHandle>` from Task 3
  - `PlaybackStore::load(path, registry, schema) -> anyhow::Result<Arc<PlaybackStore>>` from Task 4
  - `IngestHandle::record_sender`, `IngestHandle::schema_bytes` from Task 5
  - `RecordHandle`, `record_channel()` from Task 1
  - `rfd::FileDialog` for file picker
  - `ChannelStore` as trait object: `Arc<dyn ChannelStore>`

**Behaviour:**
- `AppMode::Live`: toolbar shows `"● Rec"` button and `"Open recording"` button
- `AppMode::Replay(ReplayState)`: toolbar shows play/pause, scrub slider, speed combo, close button
- Record start: create queue → install sender in `IngestHandle::record_sender` → call `start_recording` → `AppMode::Live` keeps existing store
- Record stop: remove sender from `IngestHandle::record_sender` → drop `RecordHandle`
- Open recording: `rfd::FileDialog::new().add_filter("mcap", &["mcap"]).pick_file()` → `PlaybackStore::load` → swap store → enter `AppMode::Replay`
- Close replay: swap store back to `live_store` → `AppMode::Live`
- Playback clock: in `DataVisApp::update()`, before rendering, advance `position_ns` by `delta * speed` if playing; clamp to `[start_ns, start_ns + duration_ns]`; pause at end

Note: `DataVisApp::new` signature changes. Update `src/main.rs` accordingly.

- [ ] **Step 1: Write failing tests**

Add to `src/app.rs` test module (append to existing `mod tests { ... }`):

```rust
#[test]
fn app_mode_transitions_compile() {
    // Checks that AppMode, ReplayState types exist and are constructible.
    // Full UI tests require eframe harness; this just verifies the types.
    let _live = AppMode::Live;
}
```

- [ ] **Step 2: Run to see failure**

```bash
cargo test app_mode_transitions_compile 2>&1 | tail -5
```

Expected: compile error (`AppMode` not defined).

- [ ] **Step 3: Rewrite `src/app.rs`**

Replace the file entirely. Preserve all existing logic (toolbar, add_panel_window, on_exit) and add new fields and modes.

```rust
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui;

use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
use crate::record::playback::PlaybackStore;
use crate::record::{start_recording, RecordHandle};
use crate::store::ChannelStore;
use crate::viz::PanelRegistry;
use crate::workspace::Workspace;

#[derive(Default)]
struct AddPanelDialog {
    open: bool,
    panel_type: String,
    selected: Vec<String>,
}

pub fn build_panel_entry(panel_type: &str, selected: &[String]) -> Option<PanelEntry> {
    let mut cfg = toml::Table::new();
    match panel_type {
        "waveform" | "log" => {
            if selected.is_empty() {
                return None;
            }
            cfg.insert(
                "channels".to_string(),
                toml::Value::Array(
                    selected.iter().map(|s| toml::Value::String(s.clone())).collect(),
                ),
            );
        }
        "xy_scatter" => {
            if selected.len() != 2 {
                return None;
            }
            cfg.insert("x_channel".to_string(), toml::Value::String(selected[0].clone()));
            cfg.insert("y_channel".to_string(), toml::Value::String(selected[1].clone()));
        }
        _ => {
            if selected.len() != 1 {
                return None;
            }
            cfg.insert("channel".to_string(), toml::Value::String(selected[0].clone()));
        }
    }
    Some(PanelEntry { panel_type: panel_type.to_string(), config: cfg })
}

struct ReplayState {
    store: Arc<PlaybackStore>,
    playing: bool,
    speed: f32,
    last_frame: Instant,
}

pub(crate) enum AppMode {
    Live,
    Replay(ReplayState),
}

pub struct DataVisApp {
    store: Arc<dyn ChannelStore>,
    live_store: Arc<dyn ChannelStore>,
    channels: ChannelRegistry,
    registry: PanelRegistry,
    workspace: Workspace,
    layout_path: PathBuf,
    add_panel: AddPanelDialog,
    new_screen_name: String,
    status: String,
    conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
    mode: AppMode,
    // Recording state
    record_handle: Option<RecordHandle>,
    record_sender_slot: Option<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
    ingest_schema_bytes: Vec<u8>,
}

impl DataVisApp {
    pub fn new(
        store: Arc<dyn ChannelStore>,
        channels: ChannelRegistry,
        registry: PanelRegistry,
        workspace: Workspace,
        layout_path: PathBuf,
        conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
        record_sender_slot: Option<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
        ingest_schema_bytes: Vec<u8>,
    ) -> Self {
        let panel_type = registry
            .type_names()
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default();
        Self {
            live_store: store.clone(),
            store,
            channels,
            registry,
            workspace,
            layout_path,
            add_panel: AddPanelDialog { panel_type, ..Default::default() },
            new_screen_name: String::new(),
            status: String::new(),
            conn_state,
            mode: AppMode::Live,
            record_handle: None,
            record_sender_slot,
            ingest_schema_bytes,
        }
    }

    fn save_layout(&mut self) {
        self.status = match self.workspace.to_config().save(&self.layout_path) {
            Ok(()) => format!("layout saved to {}", self.layout_path.display()),
            Err(e) => format!("layout save failed: {e}"),
        };
    }

    fn load_layout(&mut self) {
        match LayoutConfig::load(&self.layout_path) {
            Ok(cfg) => {
                self.workspace = Workspace::from_config(&cfg, &self.registry, &self.channels);
                self.status = format!("layout loaded from {}", self.layout_path.display());
            }
            Err(e) => self.status = format!("layout load failed: {e}"),
        }
    }

    fn start_recording(&mut self) {
        if self.record_sender_slot.is_none() {
            self.status = "Recording not available in demo mode".to_string();
            return;
        }
        let (tx, rx) = crate::record::record_channel();
        // Install sender so ingest thread starts queuing.
        if let Some(slot) = &self.record_sender_slot {
            *slot.lock().unwrap() = Some(tx);
        }
        match start_recording(
            Path::new("."),
            &self.channels,
            self.ingest_schema_bytes.clone(),
            rx,
        ) {
            Ok(handle) => {
                self.record_handle = Some(handle);
                self.status = "Recording started".to_string();
            }
            Err(e) => {
                // Remove sender since recorder won't consume it.
                if let Some(slot) = &self.record_sender_slot {
                    *slot.lock().unwrap() = None;
                }
                self.status = format!("Record failed: {e}");
            }
        }
    }

    fn stop_recording(&mut self) {
        // Remove sender first so ingest stops queuing, then drop handle to signal recorder.
        if let Some(slot) = &self.record_sender_slot {
            *slot.lock().unwrap() = None;
        }
        self.record_handle = None;
        self.status = "Recording stopped".to_string();
    }

    fn open_recording(&mut self) {
        if self.record_handle.is_some() {
            self.status = "Stop recording before opening a file".to_string();
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MCAP recording", &["mcap"])
            .pick_file()
        else {
            return;
        };

        // PlaybackStore::load needs a ProtoSchema. For replay, we reconstruct the pool
        // from the embedded MCAP schema bytes stored in the file, OR we rely on the
        // ingest schema if available. For v1: require the schema to have been loaded
        // at startup (ingest_schema_bytes non-empty). If empty (demo mode), show error.
        //
        // For demo mode or when proto schema is not available at runtime, we still
        // need schema to decode. In demo mode, we can use a dummy schema and rely on
        // the fact that PlaybackStore::load uses the TopicRouter which uses the ProtoSchema.
        // For v1: only support replay when ingest was started (schema_bytes non-empty).
        if self.ingest_schema_bytes.is_empty() {
            self.status = "Replay requires a proto schema (not available in demo mode)".to_string();
            return;
        }

        // Reconstruct ProtoSchema from schema_bytes requires writing to a temp file
        // or using the pool directly. Since we already have the bytes from startup,
        // we need the ProtoSchema. However, we only stored bytes, not the ProtoSchema.
        //
        // Solution: store the ProtoSchema in DataVisApp as well (Option<ProtoSchema>).
        // See note in step 4 below — this requires adding a field.
        //
        // For now, this placeholder will be completed in step 4.
        self.status = "Open recording: see step 4".to_string();
    }

    fn close_replay(&mut self) {
        self.store = self.live_store.clone();
        self.mode = AppMode::Live;
        self.status = "Replay closed".to_string();
    }

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Save layout").clicked() {
                        self.save_layout();
                        ui.close_menu();
                    }
                    if ui.button("Load layout").clicked() {
                        self.load_layout();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Screen selector
                ui.label("screen:");
                let mut selected = self.workspace.active.clone();
                egui::ComboBox::from_id_source("screen-select")
                    .selected_text(&selected)
                    .show_ui(ui, |ui| {
                        for name in self.workspace.screens.keys() {
                            ui.selectable_value(&mut selected, name.clone(), name);
                        }
                    });
                if selected != self.workspace.active {
                    self.workspace.active = selected;
                }
                ui.menu_button("+ screen", |ui| {
                    ui.text_edit_singleline(&mut self.new_screen_name);
                    if ui.button("Create").clicked() && !self.new_screen_name.is_empty() {
                        let name = std::mem::take(&mut self.new_screen_name);
                        self.workspace.add_screen(&name);
                        ui.close_menu();
                    }
                });
                if ui.button("+ panel").clicked() {
                    self.add_panel.open = true;
                }
                ui.separator();

                match &self.mode {
                    AppMode::Live => {
                        // Connection state
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
                        ui.separator();

                        // Record controls
                        if self.record_handle.is_none() {
                            if ui.button("Rec").clicked() {
                                self.start_recording();
                            }
                            if ui.button("Open recording").clicked() {
                                self.open_recording();
                            }
                        } else {
                            if ui.button("Stop Rec").clicked() {
                                self.stop_recording();
                            }
                            if let Some(handle) = &self.record_handle {
                                let gaps = handle.gap_count.load(Ordering::Relaxed);
                                if gaps > 0 {
                                    ui.colored_label(egui::Color32::RED, format!("{gaps} gaps"));
                                }
                                if handle.record_failed.load(Ordering::Relaxed) {
                                    ui.colored_label(egui::Color32::RED, "WRITE ERROR");
                                }
                            }
                        }
                    }
                    AppMode::Replay(_) => {
                        ui.colored_label(egui::Color32::LIGHT_BLUE, "REPLAY");
                        ui.separator();
                        // Controls rendered in advance_and_draw_replay below
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.status);
                });
            });
        });
    }

    fn replay_controls(&mut self, ctx: &egui::Context) {
        let AppMode::Replay(ref mut rs) = self.mode else { return };
        egui::TopBottomPanel::top("replay_controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let play_label = if rs.playing { "Pause" } else { "Play" };
                if ui.button(play_label).clicked() {
                    rs.playing = !rs.playing;
                }

                let pos = rs.store.position_ns.load(Ordering::Relaxed);
                let start = rs.store.start_ns;
                let dur = rs.store.duration_ns.max(1);
                let mut offset = (pos - start) as f64;
                let dur_f = dur as f64;
                if ui.add(egui::Slider::new(&mut offset, 0.0..=dur_f)
                    .text("pos")
                    .custom_formatter(|v, _| {
                        let secs = v / 1e9;
                        format!("{:.1}s", secs)
                    }))
                    .changed()
                {
                    rs.store.position_ns.store(start + offset as i64, Ordering::Relaxed);
                }

                egui::ComboBox::from_label("speed")
                    .selected_text(format!("{}x", rs.speed))
                    .show_ui(ui, |ui| {
                        for &s in &[0.25f32, 0.5, 1.0, 2.0, 4.0] {
                            ui.selectable_value(&mut rs.speed, s, format!("{s}x"));
                        }
                    });

                if ui.button("Close").clicked() {
                    // Signal close — handled after borrow ends
                }
            });
        });
    }

    fn add_panel_window(&mut self, ctx: &egui::Context) {
        if !self.add_panel.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Add panel")
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::ComboBox::from_label("type")
                    .selected_text(&self.add_panel.panel_type)
                    .show_ui(ui, |ui| {
                        for t in self.registry.type_names() {
                            ui.selectable_value(&mut self.add_panel.panel_type, t.to_string(), t);
                        }
                    });
                ui.label(match self.add_panel.panel_type.as_str() {
                    "xy_scatter" => "select exactly 2 channels (x first, then y)",
                    "waveform" | "log" => "select one or more channels",
                    _ => "select exactly 1 channel",
                });
                ui.separator();
                for id in self.channels.iter_ids() {
                    let name = self.channels.meta(id).name.clone();
                    let mut checked = self.add_panel.selected.contains(&name);
                    if ui.checkbox(&mut checked, &name).changed() {
                        if checked {
                            self.add_panel.selected.push(name);
                        } else {
                            self.add_panel.selected.retain(|n| n != &name);
                        }
                    }
                }
                ui.separator();
                let entry = build_panel_entry(&self.add_panel.panel_type, &self.add_panel.selected);
                if ui
                    .add_enabled(entry.is_some(), egui::Button::new("Add"))
                    .clicked()
                {
                    if let Some(e) = entry {
                        if let Err(err) =
                            self.workspace.add_panel(&e, &self.registry, &self.channels)
                        {
                            self.status = format!("add panel failed: {err}");
                        }
                        self.add_panel.selected.clear();
                        self.add_panel.open = false;
                    }
                }
            });
        if !open {
            self.add_panel.open = false;
        }
    }
}

impl eframe::App for DataVisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint();

        // Advance playback clock before any rendering.
        let mut close_replay = false;
        if let AppMode::Replay(ref mut rs) = self.mode {
            if rs.playing {
                let delta_ns = rs.last_frame.elapsed().as_nanos() as i64;
                let advance = (delta_ns as f64 * rs.speed as f64) as i64;
                let pos = rs.store.position_ns.load(Ordering::Relaxed);
                let end = rs.store.start_ns + rs.store.duration_ns;
                let new_pos = (pos + advance).min(end);
                rs.store.position_ns.store(new_pos, Ordering::Relaxed);
                if new_pos >= end {
                    rs.playing = false;
                }
            }
            rs.last_frame = Instant::now();
        }

        self.menu_bar(ctx);
        self.toolbar(ctx);

        // Replay close button check — we can't call self.close_replay() inside the
        // Replay arm while mutably borrowing self.mode, so we use a flag.
        // The "Close" button in replay_controls sets close_replay = true.
        // Re-implement close in the controls panel to set a local flag:
        if let AppMode::Replay(ref mut rs) = self.mode {
            egui::TopBottomPanel::top("replay_controls").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let play_label = if rs.playing { "Pause" } else { "Play" };
                    if ui.button(play_label).clicked() {
                        rs.playing = !rs.playing;
                    }

                    let pos = rs.store.position_ns.load(Ordering::Relaxed);
                    let start = rs.store.start_ns;
                    let dur = rs.store.duration_ns.max(1);
                    let mut offset = (pos - start) as f64;
                    if ui.add(egui::Slider::new(&mut offset, 0.0..=(dur as f64))
                        .text("pos")
                        .custom_formatter(|v, _| format!("{:.1}s", v / 1e9)))
                        .changed()
                    {
                        rs.store.position_ns.store(start + offset as i64, Ordering::Relaxed);
                    }

                    egui::ComboBox::from_label("speed")
                        .selected_text(format!("{}x", rs.speed))
                        .show_ui(ui, |ui| {
                            for &s in &[0.25f32, 0.5, 1.0, 2.0, 4.0] {
                                ui.selectable_value(&mut rs.speed, s, format!("{s}x"));
                            }
                        });

                    if ui.button("Close").clicked() {
                        close_replay = true;
                    }
                });
            });
        }
        if close_replay {
            self.close_replay();
        }

        self.add_panel_window(ctx);

        // Use replay store if in replay mode, live store otherwise.
        let store: &dyn ChannelStore = if let AppMode::Replay(ref rs) = self.mode {
            rs.store.as_ref()
        } else {
            self.store.as_ref()
        };

        egui::CentralPanel::default().show(ctx, |ui| {
            self.workspace.ui(ui, store);
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.workspace.to_config().save(&self.layout_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_mode_transitions_compile() {
        let _live = AppMode::Live;
    }

    #[test]
    fn build_entry_single_channel_types() {
        for t in ["numeric", "gauge", "spectrum", "state_graph"] {
            let e = build_panel_entry(t, &["a".into()]).unwrap();
            assert_eq!(e.panel_type, t);
            assert_eq!(e.config["channel"], toml::Value::String("a".into()));
            assert!(build_panel_entry(t, &[]).is_none());
            assert!(build_panel_entry(t, &["a".into(), "b".into()]).is_none());
        }
    }

    #[test]
    fn build_entry_multi_channel_types() {
        for t in ["waveform", "log"] {
            let e = build_panel_entry(t, &["a".into(), "b".into()]).unwrap();
            assert_eq!(
                e.config["channels"],
                toml::Value::Array(vec![
                    toml::Value::String("a".into()),
                    toml::Value::String("b".into())
                ])
            );
            assert!(build_panel_entry(t, &[]).is_none());
        }
    }

    #[test]
    fn build_entry_xy_needs_exactly_two() {
        let e = build_panel_entry("xy_scatter", &["x".into(), "y".into()]).unwrap();
        assert_eq!(e.config["x_channel"], toml::Value::String("x".into()));
        assert_eq!(e.config["y_channel"], toml::Value::String("y".into()));
        assert!(build_panel_entry("xy_scatter", &["x".into()]).is_none());
        assert!(build_panel_entry("xy_scatter", &["a".into(), "b".into(), "c".into()]).is_none());
    }
}
```

- [ ] **Step 4: Fix open_recording — store ProtoSchema in DataVisApp**

The `open_recording` method needs a `ProtoSchema` to call `PlaybackStore::load`. Add an `Option<crate::ingest::loader::ProtoSchema>` field to `DataVisApp`:

```rust
// In DataVisApp fields:
ingest_schema: Option<crate::ingest::loader::ProtoSchema>,
```

But `ProtoSchema` is built during `spawn_ingest` and not exported. Approach: expose a constructor `ProtoSchema::from_bytes(bytes: &[u8]) -> anyhow::Result<Self>` that rebuilds the pool from schema bytes:

In `src/ingest/loader.rs`, add after `schema_bytes()`:
```rust
pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
    use prost_reflect::prost::Message as _;
    let fds = prost_types::FileDescriptorSet::decode(bytes)
        .context("decoding FileDescriptorSet from schema bytes")?;
    let schema_bytes = bytes.to_vec();
    let pool = DescriptorPool::from_file_descriptor_set(fds)
        .context("building descriptor pool from schema bytes")?;
    Ok(Self { pool, schema_bytes })
}
```

Then add `prost_types` to `src/ingest/loader.rs` imports:
```rust
use prost_types;
```

Now `open_recording` can do:
```rust
fn open_recording(&mut self) {
    if self.record_handle.is_some() {
        self.status = "Stop recording before opening a file".to_string();
        return;
    }
    let Some(path) = rfd::FileDialog::new()
        .add_filter("MCAP recording", &["mcap"])
        .pick_file()
    else {
        return;
    };

    if self.ingest_schema_bytes.is_empty() {
        self.status = "Replay not available in demo mode (no proto schema)".to_string();
        return;
    }
    let schema = match crate::ingest::loader::ProtoSchema::from_bytes(&self.ingest_schema_bytes) {
        Ok(s) => s,
        Err(e) => {
            self.status = format!("Failed to reconstruct schema: {e}");
            return;
        }
    };
    match PlaybackStore::load(&path, &self.channels, &schema) {
        Ok(playback) => {
            self.store = playback.clone();
            self.mode = AppMode::Replay(ReplayState {
                store: playback,
                playing: false,
                speed: 1.0,
                last_frame: Instant::now(),
            });
            self.status = format!("Loaded {}", path.display());
        }
        Err(e) => {
            self.status = format!("Failed to load recording: {e}");
        }
    }
}
```

Remove the stub `open_recording` from step 3 and replace with this full implementation.

- [ ] **Step 5: Update `src/main.rs`**

`DataVisApp::new` now takes two extra params: `record_sender_slot` and `ingest_schema_bytes`. Update `main.rs`:

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

    let (conn_state, record_sender_slot, ingest_schema_bytes) = if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
        (None, None, vec![])
    } else {
        let config = IngestConfig {
            endpoint,
            proto_path: PathBuf::from(&schema_path),
        };
        match datavis::ingest::spawn_ingest(config, &channels, store.clone()) {
            Ok(handle) => {
                let schema_bytes = handle.schema_bytes.clone();
                (Some(handle.conn_state), Some(handle.record_sender), schema_bytes)
            }
            Err(e) => {
                eprintln!("ingest: failed to start ({e}); running without live data");
                (None, None, vec![])
            }
        }
    };

    let registry = PanelRegistry::with_builtins();
    let workspace = Workspace::from_config(&layout, &registry, &channels);
    let dyn_store: Arc<dyn ChannelStore> = store;
    let app = DataVisApp::new(
        dyn_store,
        channels,
        registry,
        workspace,
        layout_path,
        conn_state,
        record_sender_slot,
        ingest_schema_bytes,
    );

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

- [ ] **Step 6: Run tests and build**

```bash
cargo test 2>&1 | tail -10
cargo build 2>&1 | tail -10
```

Expected: all tests pass. If `prost_types` import fails in `loader.rs`, check that `prost_types` is accessible via the `prost-reflect` transitive dep. If not, add to `Cargo.toml`:
```toml
prost-types = "0.13"
```

- [ ] **Step 7: Smoke test**

```bash
cargo run -- --demo
```

Verify:
- App starts, panels show live demo data
- "Rec" button visible in toolbar
- "Open recording" button visible in toolbar
- Clicking "Rec" in demo mode shows "Recording not available in demo mode" in status bar (expected — no ingest_schema_bytes in demo)
- No panics

- [ ] **Step 8: Commit**

```bash
git add src/app.rs src/main.rs src/ingest/loader.rs
git commit -m "feat: AppMode Live/Replay, record/stop controls, playback clock, store swap"
```

---

## Spec Coverage Check

| Spec requirement | Task |
|-----------------|------|
| Manual Record/Stop toolbar button | Task 6 |
| MCAP file write with embedded proto schema | Task 3 |
| One MCAP channel per ZMQ topic | Task 3 |
| Raw ZMQ proto bytes stored verbatim | Task 5 (ingest push) |
| FileDescriptorSet schema bytes embedded | Tasks 2 + 3 |
| SPSC queue cap 8192, ingest never blocks | Tasks 1 + 5 |
| Gap counting on queue full | Task 3 (RecordHandle.gap_count) |
| Gap warning in UI | Task 6 |
| record_failed flag on write error | Task 3 |
| Recorder thread drains on stop | Task 3 (recorder_loop drain) |
| Flush every 1s | Task 3 |
| Full-file PlaybackStore load | Task 4 |
| Binary-search snapshot | Task 4 |
| now_ns() override returns position | Task 4 |
| latest() anchored at position_ns | Task 4 |
| Play/Pause | Task 6 |
| Scrub slider | Task 6 |
| Speed 0.25×/0.5×/1×/2×/4× | Task 6 |
| Close replay → restore live store | Task 6 |
| Playback clock in update() | Task 6 |
| rfd file open dialog | Task 6 |
| Pause at end of file | Task 6 |
| No viz panel changes | Verified: panels use store.latest() not wall clock |
| Open while recording → error message | Task 6 |
| ChannelStore::now_ns default | Task 2 |
| ProtoSchema::schema_bytes | Task 2 |
| IngestHandle.schema_bytes | Task 5 |
| IngestHandle.record_sender Arc | Task 5 |

All spec requirements covered.
