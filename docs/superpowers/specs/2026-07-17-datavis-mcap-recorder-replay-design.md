# MCAP Recorder + Replay Design

**Date:** 2026-07-17
**Status:** Approved

## Goal

Replace the Zarr-based recorder design with MCAP. Record raw ZMQ proto bytes verbatim into a single `.mcap` file per session. Replay reads the file back, decodes via existing prost-reflect logic, and feeds a `PlaybackStore` that implements `ChannelStore` — viz panels require zero changes.

## Scope

- Manual Record/Stop (toolbar button)
- MCAP file write with embedded proto schema
- Full-recording playback: play/pause, scrub, variable speed
- Gap detection when record queue overflows

Deferred: trigger-arm capture, chunked/streaming replay for large files, CSV export.

---

## Architecture

```
Ingest thread
  recv_multipart → (topic, raw_bytes)
  │
  ├─ decode + EU scale → LiveStore  (unchanged)
  └─ if recording:
       try_send (topic, raw_bytes, log_time_ns) → SPSC queue [cap 8192]
                                                        │
                                       Recorder thread ─┘
                                       drain → MCAP writer
                                       flush every 1s
                                       finish on Stop

Replay:
  Open .mcap → read all messages → decode via decode_batch
             → fill PlaybackStore
  App swaps self.store → panels unchanged
  Playback clock in DataVisApp::update()
```

**File format:** Single `recording_<ISO8601>.mcap`. One MCAP channel per ZMQ topic. Schema = FileDescriptorSet bytes (from `protox::compile` output, same as ingest startup). Messages carry raw ZMQ proto payloads with `log_time` = i64 ns wall clock at receive time.

---

## ChannelStore Extension

Add one method to the `ChannelStore` trait with a default implementation:

```rust
fn now_ns(&self) -> i64 {
    crate::types::now_ns()
}
```

`LiveStore` inherits the default (wall clock). `PlaybackStore` overrides to return current playback position. Viz panels replace bare `crate::types::now_ns()` calls with `store.now_ns()` for window calculation — no other panel changes required.

---

## Component: Record Queue (`src/record/queue.rs`)

```rust
pub type RecordMsg = (Arc<str>, Vec<u8>, i64);  // (topic, raw_bytes, log_time_ns)

pub fn record_channel(cap: usize) -> (Sender<RecordMsg>, Receiver<RecordMsg>) {
    crossbeam_channel::bounded(cap)
}

pub const QUEUE_CAP: usize = 8192;
```

The ingest thread holds `Option<Sender<RecordMsg>>`. When recording starts, the app places a new `Sender` into `Arc<Mutex<Option<Sender<RecordMsg>>>>` shared with the ingest thread. The ingest thread checks the Option on each received message:

```rust
if let Some(sender) = &*record_sender.lock().unwrap() {
    let _ = sender.try_send((topic.into(), data.clone(), log_time_ns));
    // try_send returns Err on full queue → gap (counted below)
}
```

Queue full → `gap_count.fetch_add(1, Relaxed)`. Message is dropped. Ingest never blocks.

---

## Component: MCAP Writer (`src/record/writer.rs`)

**Crate:** `mcap = "0.8"`

```rust
pub struct McapRecorder {
    writer: mcap::Writer<BufWriter<File>>,
    channel_ids: HashMap<String, u16>,  // topic → mcap channel id
    last_flush: Instant,
}
```

**Start recording:**
1. Create file `recording_<chrono::Local::now().format("%Y%m%dT%H%M%S")>.mcap`
2. Embed proto schema: read `.proto` FileDescriptorSet bytes (same bytes produced by `protox::compile` at ingest startup — pass them through to the recorder)
3. For each unique topic in `ChannelRegistry`: `writer.add_channel(topic, schema_id, "protobuf", &HashMap::new())`
4. Spawn recorder thread with `Receiver<RecordMsg>`

**Per message:**
```rust
writer.write_to_known_channel(
    channel_ids[&topic],
    log_time_ns as u64,
    log_time_ns as u64,
    sequence,
    &raw_bytes,
)?;
if last_flush.elapsed() >= Duration::from_secs(1) {
    writer.flush()?;
    last_flush = Instant::now();
}
```

**Stop:** drain remaining messages from queue, `writer.finish()`.

**Error:** MCAP write failure → `eprintln!`, break loop, signal app via `Arc<AtomicBool>` `record_failed` flag. Status bar shows error.

---

## Component: RecordHandle (`src/record/mod.rs`)

```rust
pub struct RecordHandle {
    pub gap_count: Arc<AtomicU64>,
    pub record_failed: Arc<AtomicBool>,
    stop_tx: crossbeam_channel::Sender<()>,  // drop or send () to stop
}

impl Drop for RecordHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
    }
}

pub fn start_recording(
    output_dir: &Path,
    registry: &ChannelRegistry,
    schema_bytes: Vec<u8>,       // FileDescriptorSet bytes from ProtoSchema
    receiver: Receiver<RecordMsg>,
) -> anyhow::Result<RecordHandle>
```

`spawn_ingest` is updated to accept `Arc<Mutex<Option<Sender<RecordMsg>>>>` and populate it on each recv.

---

## Component: PlaybackStore (`src/record/playback.rs`)

Implements `ChannelStore`. Loaded once from a `.mcap` file at open time (full-file load, v1 limitation — document for large recordings).

```rust
pub struct PlaybackStore {
    channels: Vec<PlaybackChannel>,
    metas: Vec<ChannelMeta>,
    position_ns: Arc<AtomicI64>,
    pub duration_ns: i64,
    pub start_ns: i64,
}

enum PlaybackChannel {
    Float { ts: Vec<i64>, vals: Vec<f64> },
    Int   { ts: Vec<i64>, vals: Vec<i64> },
    Bool  { ts: Vec<i64>, vals: Vec<u8>  },
    Text  { lines: Vec<(i64, String)>    },
}
```

**Load from MCAP:**
1. Read all messages via `mcap::LinearReader`
2. For each message: look up `channel.topic` in `ChannelRegistry`, get bindings via `TopicRouter`, call `decode_batch` → writes into `PlaybackStore` via `write_numeric`/`write_text`
3. Sort each channel's data by timestamp
4. Set `start_ns` = min timestamp, `duration_ns` = max − min

**`snapshot(channel, window)`:** binary-search `ts` for `[window.start_ns, window.end_ns)`, return slice as `ChannelSnapshot`.

**`now_ns()`:** `self.position_ns.load(Relaxed)`

**`latest(channel)`:** last element in sorted data.

---

## Component: Playback Clock (in `src/app.rs`)

`AppMode` enum:
```rust
enum AppMode {
    Live,
    Replay(ReplayState),
}

struct ReplayState {
    store: Arc<PlaybackStore>,
    position_ns: Arc<AtomicI64>,
    playing: bool,
    speed: f32,                  // 0.25 / 0.5 / 1.0 / 2.0 / 4.0
    last_frame: Instant,
    file_path: PathBuf,
}
```

In `DataVisApp::update()`, before rendering:
```rust
if let AppMode::Replay(ref mut rs) = self.mode {
    if rs.playing {
        let delta_ns = rs.last_frame.elapsed().as_nanos() as i64;
        let advance = (delta_ns as f64 * rs.speed as f64) as i64;
        let pos = rs.position_ns.load(Relaxed);
        let store = &rs.store;
        let new_pos = (pos + advance).min(store.start_ns + store.duration_ns);
        rs.position_ns.store(new_pos, Relaxed);
        if new_pos >= store.start_ns + store.duration_ns {
            rs.playing = false;  // reached end
        }
    }
    rs.last_frame = Instant::now();
}
```

The app passes the replay store to `workspace.ui()` instead of the live store when in replay mode.

---

## UI Changes (`src/app.rs`)

**Toolbar — Live mode additions:**
- `"● Rec"` button → starts recording, becomes `"■ Stop"` while recording
- Gap warning: `"⚠ {n} gaps"` label in red when `gap_count > 0`
- `"Open recording"` button → `rfd::FileDialog::new().pick_file()` → load MCAP → enter replay mode

**Toolbar — Replay mode (replaces record controls):**
- `"▶"` / `"⏸"` play/pause toggle
- `egui::Slider` mapping `[0..=duration_ns]` to position
- Speed `ComboBox`: 0.25×, 0.5×, 1×, 2×, 4×
- Current position label: `format_time_of_day(position_ns)` (reuse from `viz::common`)
- `"✕ Close"` button → restore `self.store` to live store, restore `AppMode::Live`

**File open crate:** `rfd = "0.14"`

---

## Main.rs / spawn_ingest changes

`spawn_ingest` gains `record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>` parameter. `IngestHandle` exposes a clone of this Arc so the app can install a sender when recording starts.

`ProtoSchema` bytes (FileDescriptorSet) are preserved after ingest startup and passed to `start_recording` — stored in `IngestHandle` as `pub schema_bytes: Vec<u8>`.

---

## Error Handling

| Situation | Behavior |
|-----------|----------|
| Queue full (disk too slow) | Drop message, increment gap counter, never block ingest |
| MCAP write failure | Stop recording, set `record_failed`, show error in status bar |
| MCAP open/corrupt on replay | Stay in Live mode, show error in `self.status` |
| Replay reaches end of file | Pause at last frame |
| Open recording while recording | Disallow — "Stop recording first" status message |

---

## Testing

| Test | Location | What it verifies |
|------|----------|------------------|
| Queue drops on full, increments gap counter | `record/queue.rs` | `try_send` returns Err, counter += 1 |
| McapRecorder writes and reader sees same bytes | `record/writer.rs` | round-trip: write N `(topic, bytes)`, read via mcap::LinearReader, compare |
| PlaybackStore snapshot at position | `record/playback.rs` | binary-search returns correct window slice |
| PlaybackStore now_ns returns position not wall | `record/playback.rs` | set position_ns, call now_ns(), assert equal |
| Full round-trip: proto encode → record → PlaybackStore → snapshot | `record/playback.rs` | EU-scaled values survive the full pipeline |
| Gap in MCAP doesn't crash PlaybackStore | `record/playback.rs` | sparse data loads, snapshot returns available subset |

Manual smoke test: run `--demo`, record ~2s, open file, play, scrub, close — verify panels show replay data.

---

## New Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `mcap` | 0.8 | MCAP file read/write |
| `rfd` | 0.14 | Native file open dialog |
| `crossbeam-channel` | 0.5 | SPSC record queue |

`crossbeam-channel` may already be in the transitive dep tree; add explicitly to `Cargo.toml`.

---

## File Map

| File | Action |
|------|--------|
| `src/record/mod.rs` | New — `RecordHandle`, `start_recording`, `RecordMsg` type |
| `src/record/queue.rs` | New — `record_channel`, `QUEUE_CAP` |
| `src/record/writer.rs` | New — `McapRecorder`, recorder thread |
| `src/record/playback.rs` | New — `PlaybackStore`, load from MCAP |
| `src/ingest/loader.rs` | Modify — add `pub fn schema_bytes(&self) -> Vec<u8>` to `ProtoSchema` (encode pool back to FileDescriptorSet bytes for embedding in MCAP) |
| `src/store/mod.rs` | Modify — add `fn now_ns()` with default impl |
| `src/store/live.rs` | Modify — panels updated to call `store.now_ns()` |
| `src/ingest/mod.rs` | Modify — `IngestHandle` gains `record_sender` Arc + `schema_bytes`; `spawn_ingest` updated |
| `src/ingest/thread.rs` | Modify — `run_loop` checks record sender, pushes `RecordMsg` |
| `src/app.rs` | Modify — `AppMode`, record/replay toolbar, playback clock, store swap |
| `src/main.rs` | Modify — pass `record_sender` Arc through to ingest |
| `src/lib.rs` | Modify — `pub mod record` |
| `viz/waveform.rs` + all panels | Modify — replace `now_ns()` with `store.now_ns()` |
| `Cargo.toml` | Modify — add mcap, rfd, crossbeam-channel |
