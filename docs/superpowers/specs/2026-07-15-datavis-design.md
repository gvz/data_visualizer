# Data Visualizer — Design Spec

**Date:** 2026-07-15
**Status:** Approved

## Overview

A desktop data visualization tool for engineers monitoring live test-rig data, inspired by DeweSoft X. Written in Rust. Accepts data via ZMQ + protobuf. Supports live monitoring, recording to Zarr, and replay/exploration — all within the same application.

**Target platforms:** Linux, Windows
**UI framework:** egui / eframe
**Data input:** ZMQ SUB + protobuf (schema loaded from `.proto` file at startup)

---

## Requirements

- Up to 50 channels per screen, up to 100kHz sample rate per channel (~5M samples/sec sustained)
- Screen refresh decoupled from sample rate (~60fps render)
- Typed channels: `f64`, `i64`, `bool`, `String` (log)
- Visualization types: Waveform, FFT/Spectrum, Numeric/Gauge, XY Scatter, State Graph, Log
- Configurable layouts: GUI drag-and-drop + save/load TOML
- Multiple named screens (switchable from toolbar)
- Record to Zarr, explore recorded data with the same tool
- Live + Record run concurrently; Replay is an exclusive mode
- Cursors / measurements (min/max/mean/RMS over selection)
- Per-channel engineering-unit scaling (raw → EU) on ingest
- Capture triggers (auto start/stop recording on condition), plus manual record
- Linux + Windows desktop only; no headless or remote modes

---

## Architecture

### Overview

Single process. egui runs on the **main thread** (eframe owns the event loop). Two background threads — **Ingest** and **Recorder** — feed and drain the shared `ChannelStore`. There is no separate "render thread"; rendering is the main thread's eframe update loop.

```
┌──────────────────────────────────────────────────────────────┐
│                        App Process                           │
│                                                              │
│  ┌──────────────┐    ┌─────────────────────────────────┐    │
│  │ Ingest Thread│    │         ChannelStore            │    │
│  │              │    │  ch[i] (typed SoA ring):        │    │
│  │ ZMQ SUB      │──▶│    ts:  RingBuf<i64>            │    │
│  │ (topic subs) │    │    val: RingBuf<f64|i64|u8>    │    │
│  │ proto decode │    │  ...                            │    │
│  │ EU scaling   │    │  text channels: mutex Vec       │    │
│  └───────┬──────┘    └──────────────┬──────────────────┘    │
│          │                          │                        │
│          │ (record queue, lossless) │                        │
│          ▼                          ▼                        │
│  ┌──────────────┐        ┌──────────────────────┐           │
│  │  Recorder    │        │  Main Thread (eframe) │           │
│  │  Thread      │        │  egui @ ~60fps        │           │
│  │  → zarrs +   │        │  VizPanels            │           │
│  │    sidecar   │        │  read ChannelStore    │           │
│  └──────────────┘        └──────────────────────┘           │
│                                                              │
│  Replay mode (exclusive): Replay engine reads a recording,  │
│  fills a PlaybackStore implementing the same ChannelStore    │
│  trait. Ingest/Recorder idle while replaying.                │
└──────────────────────────────────────────────────────────────┘
```

**Key invariant:** `VizPanel` only knows about the `ChannelStore` trait. Live and replay modes are transparent to the viz layer.

### Application state machine

```
        ┌────────────── Live ──────────────┐
        │            (ingesting)            │
        │   ┌──────────────────────────┐    │
        │   │  Live + Recording        │    │  (Record toggled or trigger fired)
        │   └──────────────────────────┘    │
        └───────────────┬──────────────────┘
                        │  Open recording
                        ▼
                    ┌────────┐
                    │ Replay │  (ingest + recorder idle)
                    └────────┘
                        │  Close recording
                        ▼
                     back to Live
```

- **Live** and **Live+Recording** share one live `ChannelStore` fed by ingest.
- **Replay** is exclusive: opening a recording pauses ingest and swaps the UI's store reference to a `PlaybackStore`. Closing returns to Live.

### Threading model

- **Ingest thread** — hot path, never blocks. Decodes batches, applies EU scaling, writes to typed ring buffers. When recording, also pushes to a lossless record queue.
- **Recorder thread** — drains the record queue, flushes Zarr chunks (numeric) and sidecar (text) every ~1s.
- **Main thread (eframe)** — egui update at ~60fps. Reads `snapshot`s from the store. Never blocks ingest.
- **Replay** — replaces the store reference with `PlaybackStore`. Ingest/recorder idle.

---

## Data Model

### Timestamps

- Type: **`i64` nanoseconds since Unix epoch** (wall-clock, UTC).
- Source: **from the proto payload** — a designated timestamp field per sample (device clock). No fallback to arrival time in v1; publisher is expected to stamp samples.
- Batches carry **explicit per-sample timestamps** (not start-time+rate). This tolerates irregular/gappy data.

### Samples

Each channel has a fixed type declared in `channels.toml`. Ring buffers are **typed and homogeneous** (SoA), not an enum-of-variants ring — this keeps the hot path `Copy`-only and allocation-free.

```rust
// Logical value type (used at API boundaries, not stored per-slot in the enum form)
enum Sample {
    Float(f64),
    Int(i64),
    Bool(bool),   // stored as u8 in the ring
    Text(String), // NOT stored in the ring — separate path (see below)
}

enum SampleType { Float, Int, Bool, Text }
```

- **Numeric channels (Float/Int/Bool):** stored in a per-channel typed ring — parallel `RingBuf<i64>` timestamps + `RingBuf<f64|i64|u8>` values (Structure-of-Arrays). Lockless single-producer (ingest) / multi-reader (UI) with atomic head/tail.
- **Text channels:** bypass the high-perf ring entirely. Stored in a `Mutex<VecDeque<(i64, String)>>` (bounded by `max_lines`). Low rate, so mutex contention is a non-issue; keeps `String` allocation off the numeric hot path.

### Time alignment

- XY Scatter and Spectrum **assume uniform sampling and index-align** channels within the display window (align by sample index, no interpolation). This relies on a synchronous DAQ where combined channels share a rate.
- Spectrum computes FFT over the index-aligned window; if per-sample timestamps reveal large non-uniformity, the panel shows a warning banner (does not attempt resampling in v1).

### Memory budget

Worst case, live ring at full spec: `50 ch × 100kHz × 10s (default depth) = 50M samples`.
Per numeric sample: `8 B (ts) + 8 B (val) = 16 B` → **~800 MB** for 10s of history at full rate across 50 channels.
History depth is **configurable per channel** (default 10s). Ring capacity is allocated at startup from each channel's `max_rate` in `channels.toml`.

---

## Components

### 1. `ingest`

- Opens ZMQ SUB socket(s); subscribes to configured **topics** (topic-per-channel/group). Each message is a proto-encoded **batch** of samples tagged by topic.
- Loads `.proto` schema at startup via `prost-reflect` (dynamic reflection, no codegen).
- For each batch: decode → extract per-sample value + `i64` ns timestamp by proto field path → apply per-channel **EU scaling** (`raw * scale + offset`) → write to the channel's ring.
- When recording is active, also pushes each sample to the **lossless record queue** (see recorder).
- On disconnect: retries with exponential backoff, publishes connection state.

### 2. `channel_store`

Core shared state. Per-channel typed ring buffer (numeric) + text path.

```rust
trait ChannelStore: Send + Sync {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal);
    fn write_text(&self, channel: ChannelId, ts: i64, line: String);
    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot;
    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)>;
    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta;
}
```

- `ChannelSnapshot` returns SoA slices (timestamps + values) for zero-copy plotting.
- History depth configurable per channel (default 10s × `max_rate`).
- Ring buffers sized at startup from config. Overflow overwrites oldest (display ring only — recording uses the separate lossless queue, so display overwrite never loses recorded data).
- Lockless: `crossbeam` atomics for head/tail; single producer (ingest), multiple readers (UI).

### 3. `recorder`

- Control: **manual Record/Stop** (toolbar) always available; optionally **arm a capture trigger** (channel crosses a configured threshold) to auto-start, with pre/post-trigger duration.
- Reads from a **separate lossless queue** fed by ingest (SPSC, large/bounded). If the queue overflows because disk stalls, it records a **gap marker** and warns — never grows unbounded, never silently drops without flagging.
- On start: creates a recording directory:
  - Numeric channels → **Zarr arrays** via the `zarrs` crate (one value array + one timestamp array per channel).
  - Text channels → **JSONL sidecar** (`text/<channel>.jsonl`, `{ts, line}` per row) in the same recording dir.
- Flushes chunks every ~1s.
- On flush failure: retries, warns in status bar, keeps buffering in memory.
- Directory naming: `recording_<ISO8601>/`.
- Stores channel metadata (name, type, unit, EU scale/offset, proto path, sample rate) as Zarr attributes + a `manifest.json`.
- Gap markers and trigger events stored in the manifest.

### 4. `replay`

- Opens a `recording_*/` directory (exclusive mode; pauses live ingest).
- Implements `ChannelStore` as `PlaybackStore`.
- Exposes duration, current position, playback speed (0.1x–10x), play/pause. A **playback clock** advances position at `speed × wall-time` and drives which chunk range is loaded.
- On scrub/play: loads the Zarr chunk(s) covering the requested time range into `PlaybackStore`; text from the JSONL sidecar.
- Annotations / time markers (incl. trigger events, gap markers) shown on the timeline scrubber.

### 5. `layout`

- Wraps `egui_tiles` for split panes, tabs, and drag-and-drop.
- Panel tree serializes to/from **`layout.toml`** (separate file from channel config).
- Multiple named screens; each screen is an independent layout tree.
- Right-click panel → "Add panel" → pick type → bind channels.
- Save/load layout via menu; auto-save on exit.

`layout.toml` structure:

```toml
[screens.main]
[[screens.main.panels]]
type = "waveform"
channels = ["sensor.accel.x", "sensor.accel.y"]
time_window_s = 5.0
cursors = true

[[screens.main.panels]]
type = "state_graph"
channel = "motor.state"
states = { 0 = "IDLE", 1 = "RUN", 2 = "FAULT" }

[[screens.main.panels]]
type = "log"
channels = ["system.log"]
max_lines = 500
```

### 6. `viz`

One module per panel type. Panels are constructed through a **registry/factory** (fixes the fact that a `where Self: Sized` deserialize can't be called on a trait object):

```rust
trait VizPanel {
    fn title(&self) -> &str;
    fn accepted_types(&self) -> &[SampleType];
    fn config_ui(&mut self, ui: &mut egui::Ui);
    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore);
    fn serialize(&self) -> toml::Value;
}

// Factory keyed by the `type` string in layout.toml.
type PanelCtor = fn(toml::Value, &ChannelRegistry) -> Result<Box<dyn VizPanel>>;
struct PanelRegistry { ctors: HashMap<&'static str, PanelCtor> }
```

- The `PanelRegistry` maps `type` → constructor. Load resolves the panel's `channels`/`channel` names → `ChannelId` via the `ChannelRegistry` (built from `channels.toml`), so panels never parse channel names themselves.

**Panel types:**

| Type | Accepted sample types | Notes |
|---|---|---|
| `Waveform` | Float, Int, Bool | Scrolling time-series, configurable window, cursors + measurements |
| `Spectrum` | Float, Int | FFT via `rustfft`, configurable window fn; warns on non-uniform ts |
| `Numeric` | Float, Int, Bool | Large number display with unit label |
| `Gauge` | Float, Int | Arc/bar gauge, configurable min/max |
| `XYScatter` | Float, Int | Two channels as X/Y (index-aligned), Lissajous-style |
| `StateGraph` | Bool, Int | Grafana-style colored bands over time |
| `Log` | Text | Scrolling timestamped log, filterable |

**Cursors / measurements:** Waveform (and XY/Spectrum where meaningful) support a cursor and a selection region; the panel computes min/max/mean/RMS over the selection and displays them.

If a channel of the wrong type is bound to a panel, the panel renders an inline error message — no crash.

### 7. `app`

- eframe top-level window; egui update loop on the main thread.
- Menu bar: File (save/load layout, open recording), View (screens), Help.
- Toolbar: screen selector, Record/Stop, trigger-arm control, connection status, replay controls (when in replay mode).
- Status bar: connection state, sample rate, record duration, error/gap counter.
- Hosts layout engine + `PanelRegistry`; passes the active `ChannelStore` (live or playback) down to panels.

---

## Data Flow

### Live mode (with optional recording)

```
ZMQ batch message arrives (per topic)
  → prost-reflect decode → per-sample (value, i64 ns ts)
  → apply EU scaling (raw*scale + offset)
  → write to ChannelStore (typed ring / text path)
  → [if recording] push sample to lossless record queue
  → [recorder thread] drain queue → flush Zarr chunk + JSONL sidecar every ~1s
  → main thread @ ~60fps:
      each VizPanel calls store.snapshot(channel, window)
      → renders frame via egui
```

### Replay mode (exclusive)

```
User opens recording_* dir
  → ingest paused; UI store reference swapped to PlaybackStore
  → playback clock advances position (speed × wall-time)
  → replay engine loads Zarr chunk(s) + JSONL for the time range
  → fills PlaybackStore (implements ChannelStore)
  → main thread render loop unchanged — reads PlaybackStore as normal
User closes recording → back to Live
```

---

## Config Files

Two separate TOML files with different lifecycles.

### `channels.toml` (stable; loaded at startup)

Maps proto fields to named channels, declares type, units, EU scaling, buffer sizing, and ZMQ topic.

```toml
[channels."sensor.acceleration.x"]
topic     = "accel"
proto_path = "AccelBatch.samples.x"
ts_path    = "AccelBatch.samples.t_ns"
type      = "float"
unit      = "m/s²"
color     = "#ff0000"
max_rate  = 100000        # Hz, used for ring sizing
history_s = 10.0
eu_scale  = 1.0
eu_offset = 0.0

[channels."motor.state"]
topic     = "status"
proto_path = "StatusBatch.samples.state"
ts_path    = "StatusBatch.samples.t_ns"
type      = "int"
unit      = ""
color     = "#0000ff"
max_rate  = 1000
history_s = 30.0

[channels."system.log"]
topic     = "log"
proto_path = "LogBatch.samples.message"
ts_path    = "LogBatch.samples.t_ns"
type      = "text"
max_lines = 500
```

### `layout.toml` (changes often; loaded/saved via UI, auto-saved on exit)

Screens + panels (see `layout` component above).

---

## Error Handling

| Scenario | Behavior |
|---|---|
| ZMQ disconnect | Ingest retries with backoff; status bar shows DISCONNECTED |
| Proto decode failure | Log to Log panel + status, skip batch, increment error counter |
| Unknown proto field / topic in channel map | Warn at startup, skip that channel |
| Missing timestamp field in payload | Warn at startup; batch samples for that channel dropped + counted |
| Record queue overflow (disk stall) | Write gap marker to manifest, warn in status bar, resume when drained |
| Zarr write failure | Warn in status bar, keep buffering in memory, retry next flush |
| Wrong sample type bound to panel | Panel shows inline error, rest of UI unaffected |
| Non-uniform timestamps in Spectrum/XY window | Panel shows warning banner; still index-aligns |
| Recording corrupt/missing arrays | Replay shows error dialog, returns to Live mode |

---

## Testing Strategy

- **`channel_store`** — unit tests: synthetic samples, concurrent reader/writer, verify no data loss or torn reads on the lockless ring; text path bounds.
- **`ingest`** — feed pre-encoded proto batches via ZMQ loopback; verify topic routing, field extraction, EU scaling, ts mapping.
- **`recorder` + `replay`** — round-trip: record N samples across all types (numeric + text), replay, verify values/timestamps match; verify gap markers on forced queue overflow.
- **`viz`** — egui headless rendering smoke tests per panel type; type-mismatch error rendering; cursor measurement math (min/max/mean/RMS) against known inputs.
- **`layout`** — serialize/deserialize round-trip via `PanelRegistry`; channel-name resolution.
- **Integration** — ZMQ mock publisher → full pipeline → verify frame renders without panic at sustained 100kHz; record+replay concurrency with live.

---

## Key Dependencies

| Crate | Purpose |
|---|---|
| `egui` + `eframe` | UI framework (main-thread update loop) |
| `egui_tiles` | Dockable panel layout |
| `zmq` | ZMQ subscriber |
| `prost-reflect` | Dynamic protobuf decoding from .proto schema |
| `zarrs` | Zarr storage for numeric recordings |
| `rustfft` | FFT for spectrum panels |
| `crossbeam` | Lockless data structures for ring buffers + record queue |
| `toml` | Layout and channel config serialization |
| `serde` | Serialization support |

---

## Out of Scope (v1)

- CSV export from replay (deferred)
- Remote/headless mode
- Plugin system for user-defined viz types
- Cloud storage for recordings
- Multi-process or multi-machine setups
- Complex number / I/Q channel types
- Interpolating resample for cross-channel alignment (index-align only in v1)
- Arrival-time timestamp fallback (payload timestamp required)
