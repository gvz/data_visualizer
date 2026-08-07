# Size-Based Recording Auto-Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a live recording's `.mcap` file reaches a size limit set in `config.toml`, the recorder finalizes it and continues into a new numbered part, so no single file grows without bound; playback stitches the parts back automatically.

**Architecture:** Only the recorder changes. A new `RecordingConfig` parses `[recording] max_file_mb`; `main.rs` converts it to `max_bytes: Option<u64>` and threads it through `DataVisApp` → `start_recording` → `spawn_recorder`. The recorder holds a `RecorderCfg` describing how to open a fresh part; after each 1-second flush it stats the file on disk and, once over the limit, finalizes the current part (summary + chunk index) and opens the next (`recording_{secs}_{part:03}.mcap`). The read path is untouched — `PlaybackStore::load_many` already stitches multiple finalized files.

**Tech Stack:** Rust, `mcap` 0.8 (`Writer`, `Summary`), `serde`/`toml`, `crossbeam-channel`, `std::fs::metadata`.

## Global Constraints

- Never add `Co-Authored-By` / Claude self-attribution to commits or PRs.
- Do not commit `config.toml` (local session state). Commit only when explicitly asked — but this plan's tasks each end with a commit as the deliverable gate, which is authorized here.
- Nix: `cache.numtide.com` is forbidden; the flake sets `extra-substituters = []`. Do not add substituters.
- Preserve today's default behavior exactly: with no `[recording]` section (or `max_file_mb` absent / `0`), `max_bytes = None` and the recorder writes exactly one file named `recording_{secs}.mcap` (no suffix).
- The rollover check runs only right after the existing 1-second flush; a part may overshoot the cap by up to ~one chunk plus one second of data (documented approximation, not a bug).
- Each produced part MUST be independently finalized (`writer.finish()` → summary + chunk index) so the lazy mmap loader can read it.
- All existing library tests stay green (`cargo test --lib`). `cargo build` and `cargo clippy --lib` clean (no new warnings) before each commit.

---

## File Structure

- `src/config/recording.rs` — NEW: `RecordingConfig` + `from_toml_str` (mirrors `script::config::ScriptsConfig` parse style).
- `src/config/mod.rs` — add `pub mod recording;` + re-export.
- `src/record/writer.rs` — rework `McapRecorder` into a `RecorderCfg` + `open(part)` + size-check rollover; thread `max_bytes` through `spawn_recorder`.
- `src/record/mod.rs` — add `max_bytes: Option<u64>` param to `start_recording`.
- `src/main.rs` — parse `RecordingConfig`, compute `max_bytes`, pass to `DataVisApp::new`.
- `src/app.rs` — `DataVisApp::new` gains a `record_max_bytes: Option<u64>` param + field; `start_recording` (the method at app.rs:380) passes it through.
- `src/config/default_config.toml` — add a commented `[recording]` example.

Build order: config parser → recorder rotation → app/main wiring + docs. Each task is independently testable.

---

## Task 1: RecordingConfig parser

**Files:**
- Create: `src/config/recording.rs`
- Modify: `src/config/mod.rs`

**Interfaces:**
- Consumes: `serde`, `toml`, `anyhow`.
- Produces:
  - `pub struct RecordingConfig { pub max_file_mb: Option<u64> }`
  - `pub fn RecordingConfig::from_toml_str(s: &str) -> anyhow::Result<RecordingConfig>` — reads the `[recording]` table; absent section or absent key → `max_file_mb: None`.
  - `pub fn RecordingConfig::max_bytes(&self) -> Option<u64>` — `max_file_mb` mapped to bytes, treating `Some(0)` as `None`.

- [ ] **Step 1: Write the failing tests**

In `src/config/recording.rs` under `#[cfg(test)] mod tests`:

```rust
use super::*;

#[test]
fn absent_section_means_no_split() {
    let c = RecordingConfig::from_toml_str("[defaults]\nmax_rate = 100\n").unwrap();
    assert_eq!(c.max_file_mb, None);
    assert_eq!(c.max_bytes(), None);
}

#[test]
fn reads_max_file_mb() {
    let c = RecordingConfig::from_toml_str("[recording]\nmax_file_mb = 512\n").unwrap();
    assert_eq!(c.max_file_mb, Some(512));
    assert_eq!(c.max_bytes(), Some(512 * 1024 * 1024));
}

#[test]
fn zero_means_no_split() {
    let c = RecordingConfig::from_toml_str("[recording]\nmax_file_mb = 0\n").unwrap();
    assert_eq!(c.max_bytes(), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::recording`
Expected: FAIL to compile (`RecordingConfig` not found).

- [ ] **Step 3: Implement `RecordingConfig`**

`src/config/recording.rs`:

```rust
use anyhow::Context;
use serde::Deserialize;

/// The `[recording]` section of config.toml. Controls size-based auto-split of
/// live recordings. Absent section / absent key / `0` all mean "single file".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecordingConfig {
    pub max_file_mb: Option<u64>,
}

#[derive(Deserialize)]
struct DocWrapper {
    recording: Option<RawRecording>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecording {
    max_file_mb: Option<u64>,
}

impl RecordingConfig {
    /// Parse the `[recording]` table out of a full config.toml. An absent
    /// section or absent key yields `max_file_mb: None`.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let doc: DocWrapper = toml::from_str(s).context("parsing [recording]")?;
        Ok(match doc.recording {
            None => RecordingConfig::default(),
            Some(raw) => RecordingConfig { max_file_mb: raw.max_file_mb },
        })
    }

    /// Size limit in bytes, or `None` for no split. `Some(0)` is treated as
    /// `None` so a user can disable splitting without deleting the key.
    pub fn max_bytes(&self) -> Option<u64> {
        match self.max_file_mb {
            Some(mb) if mb > 0 => Some(mb * 1024 * 1024),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    // (tests from Step 1)
}
```

Add to `src/config/mod.rs` (match existing `pub mod` / `pub use` style near lines 1-5):

```rust
pub mod recording;
pub use recording::RecordingConfig;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config::recording`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/config/recording.rs src/config/mod.rs
git commit -m "feat: parse [recording] max_file_mb from config"
```

---

## Task 2: Recorder rotation by size

**Files:**
- Modify: `src/record/writer.rs`
- Modify: `src/record/mod.rs`

**Interfaces:**
- Consumes: `RecordMsg`, `ChannelRegistry`, `mcap::Writer`, `std::fs::metadata`.
- Produces:
  - `record::start_recording(output_dir, registry, schema_bytes: Vec<u8>, receiver, max_bytes: Option<u64>) -> anyhow::Result<RecordHandle>` (new trailing param).
  - `writer::spawn_recorder(..., max_bytes: Option<u64>)` (new trailing param).
  - Internal `RecorderCfg` + reworked `McapRecorder::open(cfg, part)` with size-triggered rollover producing `recording_{secs}_{part:03}.mcap` parts when `max_bytes.is_some()`.

**Note on the existing tests:** `writer.rs`'s three `start_recording` call sites (`roundtrip_write_read`, `unknown_topic_messages_are_skipped`, `dynamic_proto_registers_per_topic_schemas`, `dynamic_proto_topic_colliding_with_registry_gets_own_schema`) must be updated to pass `None` as the new last arg. Likewise the caller in `src/app.rs` (Task 3). With `None`, filenames and behavior are unchanged, so those tests still assert exactly one `recording_{secs}.mcap`.

- [ ] **Step 1: Write the failing tests**

Add to `writer.rs` `mod tests` (reuse `minimal_registry`, `record_channel`, `start_recording`):

```rust
#[test]
fn rotates_into_numbered_parts_when_over_limit() {
    use crate::record::playback::PlaybackStore;
    let dir = tempfile::tempdir().unwrap();
    let registry = minimal_registry();
    let (tx, rx) = record_channel();
    // Tiny 8 KiB cap forces several rollovers over a few thousand messages.
    let handle = start_recording(dir.path(), &registry, vec![], rx, Some(8 * 1024)).unwrap();

    let n = 4000i64;
    for i in 0..n {
        // 64-byte payloads so total >> cap after compression.
        tx.try_send(RecordMsg::Proto { topic: Arc::from("accel"), data: vec![(i % 256) as u8; 64], ts: i + 1 })
            .unwrap();
    }
    drop(tx);
    drop(handle);
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Collect parts, sorted by name → they are recording_{secs}_000.mcap, _001, …
    let mut parts: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "mcap").unwrap_or(false))
        .collect();
    parts.sort();
    assert!(parts.len() >= 2, "expected multiple parts, got {}", parts.len());
    // First part is suffix _000.
    assert!(parts[0].file_name().unwrap().to_str().unwrap().contains("_000.mcap"));
    // Every part is finalized (summary readable, non-empty chunk index).
    for p in &parts {
        let bytes = std::fs::read(p).unwrap();
        let summary = mcap::Summary::read(&bytes).unwrap().expect("finalized summary");
        assert!(!summary.chunk_indexes.is_empty(), "part {p:?} has no chunk index");
    }

    // Stitched round-trip: load_many over all parts returns every message.
    let refs: Vec<&std::path::Path> = parts.iter().map(|p| p.as_path()).collect();
    let store = PlaybackStore::load_many(&refs, &registry).unwrap();
    let id = registry.id("x").unwrap();
    use crate::store::ChannelStore;
    use crate::types::TimeWindow;
    let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
    // The channel type is float; every message decodes as one sample. Assert the
    // overall count is preserved and timestamps are ordered.
    let snap = store.snapshot(id, all);
    let len = snap.len();
    assert_eq!(len, n as usize, "stitched parts must contain all messages");
}
```

Note: `minimal_registry` maps channel `"x"` to topic `"accel"` with `proto_path = "M.v"`, `ts_path = "M.t"`. The raw 64-byte payloads are not valid protobuf for that schema, so decoded playback samples would be zero — DO NOT rely on decoding here. Instead assert on the **finalized files + chunk indexes** and the **stitched message count via `MessageStream`**, not on `PlaybackStore` decoding. Replace the `load_many`/`snapshot` block above with a direct raw-message count that does not depend on the registry schema:

```rust
    // Stitched round-trip at the MCAP layer: every written message survives,
    // across all parts, in timestamp order.
    let mut all_ts: Vec<u64> = Vec::new();
    for p in &parts {
        let bytes = std::fs::read(p).unwrap();
        for m in mcap::MessageStream::new(&bytes).unwrap() {
            all_ts.push(m.unwrap().log_time);
        }
    }
    assert_eq!(all_ts.len(), n as usize, "all messages must be preserved across parts");
    all_ts.sort_unstable();
    assert_eq!(all_ts.first(), Some(&1));
    assert_eq!(all_ts.last(), Some(&(n as u64)));
```

Use ONLY the raw-message-count version (drop the `PlaybackStore` block); it is schema-independent and deterministic.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib record::writer::tests::rotates_into_numbered_parts_when_over_limit`
Expected: FAIL to compile (`start_recording` takes 4 args, not 5).

- [ ] **Step 3: Update `start_recording` (record/mod.rs)**

In `src/record/mod.rs`, add the trailing param and forward it:

```rust
pub fn start_recording(
    output_dir: &std::path::Path,
    registry: &crate::config::ChannelRegistry,
    schema_bytes: Vec<u8>,
    receiver: crossbeam_channel::Receiver<RecordMsg>,
    max_bytes: Option<u64>,
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
        max_bytes,
    )?;
    Ok(RecordHandle { gap_count, record_failed, stop_tx })
}
```

- [ ] **Step 4: Rework `writer.rs`**

Replace `McapRecorder::new`, `recorder_thread_fn`, `recorder_loop`, and `spawn_recorder` with the rotation-aware versions. Add `use std::path::PathBuf;` and `use std::collections::HashSet;` (HashSet already imported).

`RecorderCfg` + reworked struct fields:

```rust
/// Everything needed to open a fresh recording part without borrowing the
/// registry into the recorder thread.
struct RecorderCfg {
    output_dir: PathBuf,
    session_secs: u64,
    schema_bytes: Vec<u8>,
    /// Distinct ZMQ/Proto topics to pre-seed as channels in every part.
    zmq_topics: Vec<String>,
    /// On-disk size limit; `None` = never rotate, single un-suffixed file.
    max_bytes: Option<u64>,
}

impl RecorderCfg {
    fn part_path(&self, part: u32) -> PathBuf {
        let name = if self.max_bytes.is_some() {
            format!("recording_{}_{:03}.mcap", self.session_secs, part)
        } else {
            format!("recording_{}.mcap", self.session_secs)
        };
        self.output_dir.join(name)
    }
}
```

Add `path: PathBuf`, `max_bytes: Option<u64>`, `over_limit: bool` to `McapRecorder` and replace `new` with `open`:

```rust
struct McapRecorder {
    writer: mcap::Writer<'static, BufWriter<File>>,
    channel_ids: HashMap<String, u16>,
    dynamic_channel_ids: HashMap<String, u16>,
    last_flush: Instant,
    sequence: u32,
    path: PathBuf,
    max_bytes: Option<u64>,
    /// Set true by the flush block once the file on disk reaches `max_bytes`.
    over_limit: bool,
}

impl McapRecorder {
    fn open(cfg: &RecorderCfg, part: u32) -> anyhow::Result<Self> {
        let path = cfg.part_path(part);
        let file = BufWriter::new(File::create(&path)?);
        let mut writer = mcap::Writer::new(file)?;

        let schema = Arc::new(mcap::Schema {
            name: "protobuf".to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(cfg.schema_bytes.clone()),
        });

        let mut channel_ids = HashMap::new();
        for topic in &cfg.zmq_topics {
            let channel = mcap::Channel {
                topic: topic.clone(),
                schema: Some(schema.clone()),
                message_encoding: "protobuf".to_string(),
                metadata: BTreeMap::new(),
            };
            let channel_id = writer.add_channel(&channel)?;
            channel_ids.insert(topic.clone(), channel_id);
        }

        Ok(Self {
            writer,
            channel_ids,
            dynamic_channel_ids: HashMap::new(),
            last_flush: Instant::now(),
            sequence: 0,
            path,
            max_bytes: cfg.max_bytes,
            over_limit: false,
        })
    }

    // write_to_channel, write_msg, write_dynamic: UNCHANGED except the flush
    // block in write_to_channel gains the size check below.

    fn finish(mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        self.writer.finish()?;
        Ok(())
    }
}
```

The flush block inside `write_to_channel` becomes:

```rust
        if self.last_flush.elapsed() >= Duration::from_secs(1) {
            self.writer.flush()?;
            self.last_flush = Instant::now();
            if let Some(limit) = self.max_bytes {
                // A stat failure just skips the check — never abort a session.
                if std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) >= limit {
                    self.over_limit = true;
                }
            }
        }
```

Rotation-aware loop + thread fn:

```rust
fn recorder_thread_fn(
    cfg: RecorderCfg,
    recorder: McapRecorder,
    record_rx: Receiver<RecordMsg>,
    stop_rx: crossbeam_channel::Receiver<()>,
    record_failed: Arc<AtomicBool>,
) {
    if let Err(e) = recorder_loop(cfg, recorder, record_rx, stop_rx) {
        eprintln!("recorder: write error: {e}");
        record_failed.store(true, Ordering::Relaxed);
    }
}

/// Finalize the current part and open the next one.
fn rotate(recorder: McapRecorder, cfg: &RecorderCfg, part: u32) -> anyhow::Result<McapRecorder> {
    recorder.finish()?;
    McapRecorder::open(cfg, part)
}

fn recorder_loop(
    cfg: RecorderCfg,
    mut recorder: McapRecorder,
    record_rx: Receiver<RecordMsg>,
    stop_rx: crossbeam_channel::Receiver<()>,
) -> anyhow::Result<()> {
    let mut part: u32 = 0;
    loop {
        crossbeam_channel::select! {
            recv(record_rx) -> result => match result {
                Ok(msg) => {
                    write_record(&mut recorder, msg)?;
                    if recorder.over_limit {
                        part += 1;
                        recorder = rotate(recorder, &cfg, part)?;
                    }
                }
                Err(_) => break,
            },
            recv(stop_rx) -> _ => break,
        }
    }
    // Drain any messages that arrived before stop.
    while let Ok(msg) = record_rx.try_recv() {
        write_record(&mut recorder, msg)?;
        if recorder.over_limit {
            part += 1;
            recorder = rotate(recorder, &cfg, part)?;
        }
    }
    recorder.finish()
}
```

`spawn_recorder` precomputes `zmq_topics`, opens part 0, spawns the thread:

```rust
pub(super) fn spawn_recorder(
    output_dir: &Path,
    registry: &ChannelRegistry,
    schema_bytes: &[u8],
    receiver: Receiver<RecordMsg>,
    _gap_count: Arc<AtomicU64>,
    record_failed: Arc<AtomicBool>,
    max_bytes: Option<u64>,
) -> anyhow::Result<crossbeam_channel::Sender<()>> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Distinct ZMQ/Proto topics to pre-seed in every part (MQTT topics register
    // dynamically on first sight, per part).
    let mut seen: HashSet<String> = HashSet::new();
    let mut zmq_topics: Vec<String> = Vec::new();
    for id in registry.iter_ids() {
        if let Some(topic) = registry.config(id).topic.clone() {
            if seen.insert(topic.clone()) {
                zmq_topics.push(topic);
            }
        }
    }

    let cfg = RecorderCfg {
        output_dir: output_dir.to_path_buf(),
        session_secs: secs,
        schema_bytes: schema_bytes.to_vec(),
        zmq_topics,
        max_bytes,
    };
    let recorder = McapRecorder::open(&cfg, 0)?;
    let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let rf = record_failed.clone();
    std::thread::spawn(move || recorder_thread_fn(cfg, recorder, receiver, stop_rx, rf));
    Ok(stop_tx)
}
```

- [ ] **Step 5: Update the existing `writer.rs` tests to pass `None`**

In each of `roundtrip_write_read`, `unknown_topic_messages_are_skipped`, `dynamic_proto_registers_per_topic_schemas`, `dynamic_proto_topic_colliding_with_registry_gets_own_schema`, change the `start_recording(dir.path(), &registry, <schema>, rx)` call to `start_recording(dir.path(), &registry, <schema>, rx, None)`.

- [ ] **Step 6: Run the writer tests**

Run: `cargo test --lib record::writer`
Expected: PASS — the four existing tests plus `rotates_into_numbered_parts_when_over_limit`.

- [ ] **Step 7: Full build + clippy + whole suite**

Run: `cargo build && cargo clippy --lib --all-targets && cargo test --lib`
Expected: clean; all green. (Note: `src/app.rs` still calls the 4-arg `start_recording` — it is fixed in Task 3. If `cargo build` fails ONLY on that call site, that is expected here; proceed to Task 3 and re-run the full gate there. `cargo test --lib` of `record::writer` still passes because the lib test target compiles the whole crate — so if app.rs breaks the build, do Step 5/6 by temporarily fixing app.rs's call to pass `None` as part of THIS task to keep the crate compiling, then complete the wiring in Task 3.)

To keep the crate compiling at the end of this task, also apply the one-line app.rs fix now: in `src/app.rs` `start_recording` method, change the `start_recording(Path::new("."), &self.channels, self.ingest_schema_bytes.clone(), rx)` call to pass a trailing `self.record_max_bytes` — but that field does not exist yet. Simplest: pass `None` here now, and Task 3 replaces `None` with the real field. Update the call to:

```rust
        match start_recording(
            Path::new("."),
            &self.channels,
            self.ingest_schema_bytes.clone(),
            rx,
            None, // replaced with self.record_max_bytes in the config-wiring task
        ) {
```

- [ ] **Step 8: Commit**

```bash
git add src/record/writer.rs src/record/mod.rs src/app.rs
git commit -m "feat: auto-split recordings into numbered parts over a size limit"
```

---

## Task 3: Wire config → app → recorder + docs

**Files:**
- Modify: `src/main.rs`
- Modify: `src/app.rs`
- Modify: `src/config/default_config.toml`

**Interfaces:**
- Consumes: `RecordingConfig` (Task 1), the `max_bytes` param on `start_recording` (Task 2).
- Produces: `DataVisApp::new(..., record_max_bytes: Option<u64>)` with a `record_max_bytes: Option<u64>` field, used by the `start_recording` method.

- [ ] **Step 1: Add the field + constructor param to `DataVisApp`**

In `src/app.rs`, add a field to the struct (near the other recording-related fields such as `record_handle` / `record_sender_slots`):

```rust
    /// On-disk size cap for auto-split, from `[recording] max_file_mb`.
    record_max_bytes: Option<u64>,
```

Add the trailing parameter to `DataVisApp::new` (after `script_metas: SharedMetas,`):

```rust
        record_max_bytes: Option<u64>,
```

Initialize the field in the struct literal returned by `new` (add `record_max_bytes,`).

- [ ] **Step 2: Use the field in the `start_recording` method**

Replace the `None` placeholder from Task 2 Step 7 with the field:

```rust
        match start_recording(
            Path::new("."),
            &self.channels,
            self.ingest_schema_bytes.clone(),
            rx,
            self.record_max_bytes,
        ) {
```

- [ ] **Step 3: Parse and pass the config in `main.rs`**

After the `scripts_cfg` block (around main.rs:96) and before `DataVisApp::new` is called, parse the recording config from the same config text:

```rust
    let record_max_bytes = datavis::config::RecordingConfig::from_toml_str(
        &std::fs::read_to_string(&layout_path).unwrap_or_default(),
    )
    .map(|c| c.max_bytes())
    .unwrap_or(None);
```

Pass `record_max_bytes` as the new trailing argument to `DataVisApp::new(...)` (after `script_metas`).

- [ ] **Step 4: Build to verify the wiring compiles**

Run: `cargo build`
Expected: compiles clean (the app.rs call site now uses `self.record_max_bytes`; `DataVisApp::new` has the new param supplied by main.rs).

- [ ] **Step 5: Document the option in `default_config.toml`**

Add a commented example section to `src/config/default_config.toml` (so a freshly-generated config shows the knob). Place it near other top-level sections:

```toml
# Auto-split live recordings once the .mcap file on disk reaches this many MB.
# Parts are named recording_<secs>_000.mcap, _001, ... and replay stitched
# together. Omit the section or set 0 to keep a single file.
# [recording]
# max_file_mb = 512
```

- [ ] **Step 6: Full build + clippy + whole suite**

Run: `cargo build && cargo clippy --lib --all-targets && cargo test --lib`
Expected: clean; all green (existing count + Task 1's 3 + Task 2's 1 new tests).

- [ ] **Step 7: Manual run check (drive the recorder)**

Build and run the app; with a `config.toml` containing `[recording] max_file_mb = 1` and a live source (or the `--demo` flag), start recording, let it run past ~1 MB, stop, and confirm multiple `recording_<secs>_NNN.mcap` files appear in the working directory and that opening them all in the replay picker stitches one timeline. If no live source is available, note that the rotation logic is covered by the Task 2 integration test and report that.

- [ ] **Step 8: Commit**

```bash
git add src/main.rs src/app.rs src/config/default_config.toml
git commit -m "feat: wire [recording] max_file_mb through app to the recorder"
```

---

## Self-Review Notes (author)

- **Spec coverage:** config parse (Task 1) · on-disk size rollover + numbered parts + finalized-per-part (Task 2) · no-limit single-file preserved (Task 2 Steps 5-6, existing tests) · stitched round-trip (Task 2 test) · config→app→recorder wiring + docs (Task 3). All spec sections map to a task.
- **Type consistency:** `RecordingConfig::{from_toml_str, max_bytes}`, `start_recording(.., max_bytes: Option<u64>)`, `spawn_recorder(.., max_bytes)`, `RecorderCfg { output_dir, session_secs, schema_bytes, zmq_topics, max_bytes }`, `RecorderCfg::part_path(part)`, `McapRecorder::{open, over_limit, path, max_bytes}`, `DataVisApp::new(.., record_max_bytes)` used consistently across tasks.
- **Ordering caveat:** Task 2 changes `start_recording`'s arity, which breaks the app.rs call site; Task 2 Step 7 patches app.rs to pass `None` so the crate keeps compiling and its own tests stay green, then Task 3 replaces `None` with the real field. Called out explicitly so a task-scoped reviewer is not surprised.
- **Known approximation (documented, not a bug):** rollover fires at the first flush past the limit, so a part can overshoot by ~one chunk + one second of data.
