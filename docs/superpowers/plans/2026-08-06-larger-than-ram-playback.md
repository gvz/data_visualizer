# Larger-Than-RAM MCAP Playback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `PlaybackStore`'s full-retention internals with a memory-mapped, chunk-on-demand store plus a decimated min/max envelope, so recordings larger than RAM play back through the unchanged `ChannelStore` interface.

**Architecture:** Each recording is memory-mapped (`memmap2`) and indexed by chunk time-bounds from the MCAP summary. Load makes one parallel pass that folds samples into a small per-channel min/max envelope (numeric) and full retention (text), keeping no raw numeric samples. `snapshot(window)` decodes the few chunks overlapping a narrow window (byte-budgeted LRU cache) for exact detail, or reads the envelope for near-whole-file windows. The public type stays `PlaybackStore`; only `src/record/playback.rs` internals change plus new helper modules under `src/record/lazy/`.

**Tech Stack:** Rust, `mcap` 0.8 (`Summary`, `ChunkIndex`, `stream_chunk`), `memmap2` (new), `std::thread::scope`, `prost-reflect` decode (existing `decode_batch`/`decode_message`).

## Global Constraints

- Never add `Co-Authored-By` / Claude self-attribution to commits or PRs.
- Do not commit `config.toml` (local session state); commit only when explicitly asked — but this plan's tasks each end with a commit as the deliverable gate, which is authorized here.
- Nix: `cache.numtide.com` is forbidden; the flake sets `extra-substituters = []`. Do not add substituters.
- The public store type stays named `PlaybackStore` and lives in `src/record/playback.rs`; `load(path, registry) -> anyhow::Result<Arc<Self>>` and `load_many(paths: &[&Path], registry) -> anyhow::Result<Arc<Self>>` signatures are preserved, as are the public fields `position_ns: Arc<AtomicI64>`, `duration_ns: i64`, `start_ns: i64`, so `app.rs` is not edited.
- The `ChannelStore` trait is not changed. `snapshot`, `latest`, `latest_at`, `now_ns`, `break_times`, `channel_meta` keep their current signatures and observable semantics for windows that fit the detail path.
- Preserve stitching semantics: original timestamps, `break_times` at each non-earliest file's start, order-independent.
- Defaults (exact values): `cache_bytes = 512 * 1024 * 1024`, `envelope_buckets = 16384`, `chunk_budget = 16`.
- All existing library tests must stay green (`cargo test --lib`). Run `cargo build` and `cargo clippy` clean (no new warnings) before each commit.

---

## File Structure

- `Cargo.toml` — add `memmap2` dependency.
- `src/record/lazy/mod.rs` — module root; re-exports `Envelope`, `ChunkCache`, `RecordingSource`, `DecodedChunk`, `ChunkDecodeBuf`.
- `src/record/lazy/envelope.rs` — `Envelope`: per-channel min/max buckets; builder that folds samples; window read to `ChannelSnapshot`.
- `src/record/lazy/decode_buf.rs` — `ChunkDecodeBuf` (a `ChannelStore` scratch that collects per-channel typed vecs) and `DecodedChunk` (its frozen, byte-measured form).
- `src/record/lazy/cache.rs` — `ChunkCache`: byte-budgeted LRU of `Arc<DecodedChunk>` keyed by `(file, chunk)`.
- `src/record/lazy/source.rs` — `RecordingSource`: `Mmap` + `Summary` + sorted `ChunkSpan`s; `overlapping`, `decode_chunk`, bounds.
- `src/record/playback.rs` — `PlaybackStore` internals reworked to hold `Vec<RecordingSource>`, `Envelope`, retained text, `ChunkCache`; new `load_many`; `ChannelStore` impl over the two read paths.
- `src/record/mod.rs` — add `pub mod lazy;` (or `mod lazy;`) — check current visibility style.

Build order: memmap2 dep → envelope → decode_buf → cache → source → store integration → wide-zoom path → latest_at → cleanup/docs. Each task is independently testable.

---

## Task 1: Add memmap2 dependency

**Files:**
- Modify: `Cargo.toml` (the `[dependencies]` table, currently starting line 8)

**Interfaces:**
- Consumes: nothing.
- Produces: `memmap2` crate available (`memmap2::Mmap`, `unsafe { Mmap::map(&File) }`).

- [ ] **Step 1: Add the dependency**

Add to the `[dependencies]` table in `Cargo.toml`, keeping alphabetical-ish grouping near other deps:

```toml
memmap2 = "0.9"
```

- [ ] **Step 2: Verify it resolves and builds**

Run: `cargo build`
Expected: compiles; `memmap2 v0.9.x` appears in the build. If the offline/substituter setup blocks the download, stop and report — do NOT add a substituter (Global Constraints).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add memmap2 for memory-mapped MCAP playback"
```

---

## Task 2: Envelope (per-channel min/max buckets)

**Files:**
- Create: `src/record/lazy/mod.rs`
- Create: `src/record/lazy/envelope.rs`
- Modify: `src/record/mod.rs` (add the module)

**Interfaces:**
- Consumes: `crate::types::{ChannelId, ChannelSnapshot, TimeWindow, SampleType}`.
- Produces:
  - `struct Envelope` with:
    - `fn new(nchannels: usize, start_ns: i64, duration_ns: i64, buckets: usize) -> Envelope`
    - `fn fold_numeric(&mut self, ch: usize, ts: i64, val: f64)` — updates that channel's bucket min/max.
    - `fn merge(&mut self, other: &Envelope)` — bucket-wise min/max union (same shape).
    - `fn read(&self, ch: usize, window: TimeWindow) -> Vec<(i64, f64)>` — for each non-empty bucket intersecting the window, its min and max points in timestamp order.
  - `fn bucket_of(start_ns: i64, duration_ns: i64, buckets: usize, ts: i64) -> usize` (module-private, tested via `Envelope`).

Bucket cell layout: `struct Cell { any: bool, t_min: i64, v_min: f64, t_max: i64, v_max: f64 }`, `Vec<Cell>` length `nchannels * buckets`, row-major by channel.

- [ ] **Step 1: Write the module root**

`src/record/lazy/mod.rs`:

```rust
pub mod envelope;
pub use envelope::Envelope;
```

Add to `src/record/mod.rs` (match the file's existing `mod`/`pub mod` style):

```rust
pub mod lazy;
```

- [ ] **Step 2: Write the failing tests**

In `src/record/lazy/envelope.rs` under `#[cfg(test)] mod tests`:

```rust
use super::*;
use crate::types::TimeWindow;

#[test]
fn folds_min_max_per_bucket() {
    // 4 buckets over [0, 40): each bucket is 10 wide.
    let mut e = Envelope::new(1, 0, 40, 4);
    // Bucket 0 gets samples at t=0 (v=5) and t=9 (v=1): min 1 @9, max 5 @0.
    e.fold_numeric(0, 0, 5.0);
    e.fold_numeric(0, 9, 1.0);
    // Bucket 2 gets a single sample.
    e.fold_numeric(0, 25, 7.0);
    let pts = e.read(0, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
    // Bucket 0 emits its two extremes in time order, bucket 2 emits one point.
    assert_eq!(pts, vec![(0, 5.0), (9, 1.0), (25, 7.0)]);
}

#[test]
fn read_clips_to_window_and_skips_empty() {
    let mut e = Envelope::new(1, 0, 40, 4);
    e.fold_numeric(0, 5, 1.0);   // bucket 0
    e.fold_numeric(0, 35, 2.0);  // bucket 3
    // Window covering only the last bucket.
    let pts = e.read(0, TimeWindow { start_ns: 30, end_ns: 40 });
    assert_eq!(pts, vec![(35, 2.0)]);
}

#[test]
fn merge_unions_extremes() {
    let mut a = Envelope::new(1, 0, 40, 4);
    a.fold_numeric(0, 0, 5.0);
    let mut b = Envelope::new(1, 0, 40, 4);
    b.fold_numeric(0, 1, -3.0);
    b.fold_numeric(0, 2, 9.0);
    a.merge(&b);
    let pts = a.read(0, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
    // Bucket 0 now spans min -3 @1 .. max 9 @2, emitted in time order.
    assert_eq!(pts, vec![(1, -3.0), (2, 9.0)]);
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib record::lazy::envelope`
Expected: FAIL to compile (`Envelope` not found).

- [ ] **Step 4: Implement `Envelope`**

```rust
use crate::types::TimeWindow;

#[derive(Clone, Copy)]
struct Cell {
    any: bool,
    t_min: i64,
    v_min: f64,
    t_max: i64,
    v_max: f64,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { any: false, t_min: 0, v_min: 0.0, t_max: 0, v_max: 0.0 }
    }
}

pub struct Envelope {
    nchannels: usize,
    start_ns: i64,
    duration_ns: i64,
    buckets: usize,
    cells: Vec<Cell>,
}

fn bucket_of(start_ns: i64, duration_ns: i64, buckets: usize, ts: i64) -> usize {
    if buckets == 0 {
        return 0;
    }
    let span = (duration_ns as i128) + 1; // inclusive of the end sample
    let off = (ts as i128 - start_ns as i128).clamp(0, span - 1);
    ((off * buckets as i128) / span) as usize
}

impl Envelope {
    pub fn new(nchannels: usize, start_ns: i64, duration_ns: i64, buckets: usize) -> Self {
        let buckets = buckets.max(1);
        Self {
            nchannels,
            start_ns,
            duration_ns,
            buckets,
            cells: vec![Cell::default(); nchannels * buckets],
        }
    }

    #[inline]
    fn idx(&self, ch: usize, b: usize) -> usize {
        ch * self.buckets + b
    }

    pub fn fold_numeric(&mut self, ch: usize, ts: i64, val: f64) {
        if ch >= self.nchannels {
            return;
        }
        let b = bucket_of(self.start_ns, self.duration_ns, self.buckets, ts);
        let cell = &mut self.cells[self.idx(ch, b)];
        if !cell.any {
            *cell = Cell { any: true, t_min: ts, v_min: val, t_max: ts, v_max: val };
        } else {
            if val < cell.v_min {
                cell.v_min = val;
                cell.t_min = ts;
            }
            if val > cell.v_max {
                cell.v_max = val;
                cell.t_max = ts;
            }
        }
    }

    pub fn merge(&mut self, other: &Envelope) {
        for i in 0..self.cells.len().min(other.cells.len()) {
            let o = other.cells[i];
            if !o.any {
                continue;
            }
            let c = &mut self.cells[i];
            if !c.any {
                *c = o;
            } else {
                if o.v_min < c.v_min {
                    c.v_min = o.v_min;
                    c.t_min = o.t_min;
                }
                if o.v_max > c.v_max {
                    c.v_max = o.v_max;
                    c.t_max = o.t_max;
                }
            }
        }
    }

    pub fn read(&self, ch: usize, window: TimeWindow) -> Vec<(i64, f64)> {
        let mut out = Vec::new();
        if ch >= self.nchannels {
            return out;
        }
        for b in 0..self.buckets {
            let cell = self.cells[self.idx(ch, b)];
            if !cell.any {
                continue;
            }
            // Emit the two extremes in timestamp order; clip each to the window.
            let mut pair = if cell.t_min <= cell.t_max {
                [(cell.t_min, cell.v_min), (cell.t_max, cell.v_max)]
            } else {
                [(cell.t_max, cell.v_max), (cell.t_min, cell.v_min)]
            };
            for (t, v) in pair.drain(..) {
                if t >= window.start_ns && t < window.end_ns {
                    // Deduplicate a single-sample bucket (t_min == t_max, v equal).
                    if out.last() != Some(&(t, v)) {
                        out.push((t, v));
                    }
                }
            }
        }
        out
    }
}
```

Note the dedup: a single-sample bucket has `t_min == t_max` and equal values, so the second push is suppressed — matching the tests (`(25, 7.0)` appears once).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib record::lazy::envelope`
Expected: PASS (3 tests).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/record/mod.rs src/record/lazy/mod.rs src/record/lazy/envelope.rs
git commit -m "feat: per-channel min/max envelope for playback overview"
```

---

## Task 3: ChunkDecodeBuf + DecodedChunk

**Files:**
- Create: `src/record/lazy/decode_buf.rs`
- Modify: `src/record/lazy/mod.rs` (export)

**Interfaces:**
- Consumes: `crate::store::ChannelStore`; `crate::types::{ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, TimeWindow}`; `crate::config::ChannelRegistry`.
- Produces:
  - `struct ChunkDecodeBuf` implementing `ChannelStore` — one typed `Vec` per channel that `write_numeric`/`write_text` push into (mirrors today's `PlaybackChannel`).
    - `fn new(registry: &ChannelRegistry) -> ChunkDecodeBuf`
    - `fn freeze(self) -> DecodedChunk`
  - `struct DecodedChunk` — the frozen per-channel typed vectors plus a byte estimate.
    - `enum ChanSamples { Float { ts: Vec<i64>, vals: Vec<f64> }, Int { ts: Vec<i64>, vals: Vec<i64> }, Bool { ts: Vec<i64>, vals: Vec<u8> }, Text { lines: Vec<(i64, String)> } }`
    - `channels: Vec<ChanSamples>`
    - `fn bytes(&self) -> usize` — retained-size estimate for the cache budget.
    - `fn window(&self, ch: usize, window: TimeWindow) -> ChannelSnapshot` — `partition_point`-sliced snapshot for one channel (assumes each chunk's samples are already time-sorted, which they are within a chunk).

This is the reusable scratch that lets `decode_batch`/`decode_message` (unchanged) decode a chunk. `ChanSamples`/`ChannelSnapshot` slicing logic is the same `partition_point` code as today's `PlaybackStore::snapshot`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::store::ChannelStore;
    use crate::types::{ChannelId, NumericVal, TimeWindow};

    fn reg() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(r#"
[channels."a.f"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap()
    }

    #[test]
    fn buf_collects_then_freezes_and_windows() {
        let r = reg();
        let buf = ChunkDecodeBuf::new(&r);
        let id = r.id("a.f").unwrap();
        buf.write_numeric(id, 10, NumericVal::Float(1.0));
        buf.write_numeric(id, 20, NumericVal::Float(2.0));
        buf.write_numeric(id, 30, NumericVal::Float(3.0));
        let chunk = buf.freeze();
        assert!(chunk.bytes() > 0);
        match chunk.window(id.0 as usize, TimeWindow { start_ns: 15, end_ns: 30 }) {
            crate::types::ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![20]);
                assert_eq!(vals, vec![2.0]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib record::lazy::decode_buf`
Expected: FAIL to compile (`ChunkDecodeBuf` not found).

- [ ] **Step 3: Implement `ChunkDecodeBuf` and `DecodedChunk`**

```rust
use std::sync::Mutex;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{
    ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, SampleType, TimeWindow,
};

pub enum ChanSamples {
    Float { ts: Vec<i64>, vals: Vec<f64> },
    Int { ts: Vec<i64>, vals: Vec<i64> },
    Bool { ts: Vec<i64>, vals: Vec<u8> },
    Text { lines: Vec<(i64, String)> },
}

impl ChanSamples {
    fn for_type(t: SampleType) -> Self {
        match t {
            SampleType::Float => ChanSamples::Float { ts: vec![], vals: vec![] },
            SampleType::Int => ChanSamples::Int { ts: vec![], vals: vec![] },
            SampleType::Bool => ChanSamples::Bool { ts: vec![], vals: vec![] },
            SampleType::Text => ChanSamples::Text { lines: vec![] },
        }
    }
}

/// A `ChannelStore` scratch that simply collects every write into per-channel
/// typed vectors, so the existing `decode_batch`/`decode_message` path can
/// decode one chunk without a full store. Interior mutability (Mutex per
/// channel) because `ChannelStore` writes take `&self`.
pub struct ChunkDecodeBuf {
    channels: Vec<Mutex<ChanSamples>>,
    metas: Vec<ChannelMeta>,
}

impl ChunkDecodeBuf {
    pub fn new(registry: &ChannelRegistry) -> Self {
        Self {
            channels: registry
                .iter_ids()
                .map(|id| Mutex::new(ChanSamples::for_type(registry.meta(id).sample_type)))
                .collect(),
            metas: registry.iter_ids().map(|id| registry.meta(id).clone()).collect(),
        }
    }

    pub fn freeze(self) -> DecodedChunk {
        let channels: Vec<ChanSamples> =
            self.channels.into_iter().map(|m| m.into_inner().unwrap()).collect();
        let mut bytes = 0usize;
        for c in &channels {
            bytes += match c {
                ChanSamples::Float { ts, .. } => ts.len() * (8 + 8),
                ChanSamples::Int { ts, .. } => ts.len() * (8 + 8),
                ChanSamples::Bool { ts, .. } => ts.len() * (8 + 1),
                ChanSamples::Text { lines } => {
                    lines.iter().map(|(_, s)| 8 + s.len() + 24).sum::<usize>()
                }
            };
        }
        DecodedChunk { channels, bytes }
    }
}

pub struct DecodedChunk {
    pub channels: Vec<ChanSamples>,
    bytes: usize,
}

impl DecodedChunk {
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn window(&self, ch: usize, window: TimeWindow) -> ChannelSnapshot {
        let Some(c) = self.channels.get(ch) else {
            return ChannelSnapshot::Float { ts: Vec::new(), vals: Vec::new() };
        };
        match c {
            ChanSamples::Float { ts, vals } => {
                let (s, e) = clip(ts, window);
                ChannelSnapshot::Float { ts: ts[s..e].to_vec(), vals: vals[s..e].to_vec() }
            }
            ChanSamples::Int { ts, vals } => {
                let (s, e) = clip(ts, window);
                ChannelSnapshot::Int { ts: ts[s..e].to_vec(), vals: vals[s..e].to_vec() }
            }
            ChanSamples::Bool { ts, vals } => {
                let (s, e) = clip(ts, window);
                ChannelSnapshot::Bool { ts: ts[s..e].to_vec(), vals: vals[s..e].to_vec() }
            }
            ChanSamples::Text { lines } => {
                let s = lines.partition_point(|(t, _)| *t < window.start_ns);
                let e = lines.partition_point(|(t, _)| *t < window.end_ns);
                ChannelSnapshot::Text { lines: lines[s..e].to_vec() }
            }
        }
    }
}

fn clip(ts: &[i64], window: TimeWindow) -> (usize, usize) {
    let s = ts.partition_point(|&t| t < window.start_ns);
    let e = ts.partition_point(|&t| t < window.end_ns);
    (s, e)
}

impl ChannelStore for ChunkDecodeBuf {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal) {
        let mut c = self.channels[channel.0 as usize].lock().unwrap();
        match (&mut *c, val) {
            (ChanSamples::Float { ts: tv, vals }, NumericVal::Float(v)) => { tv.push(ts); vals.push(v); }
            (ChanSamples::Int { ts: tv, vals }, NumericVal::Int(v)) => { tv.push(ts); vals.push(v); }
            (ChanSamples::Bool { ts: tv, vals }, NumericVal::Bool(v)) => { tv.push(ts); vals.push(v as u8); }
            _ => {}
        }
    }

    fn write_text(&self, channel: ChannelId, ts: i64, line: String) {
        let mut c = self.channels[channel.0 as usize].lock().unwrap();
        if let ChanSamples::Text { lines } = &mut *c {
            lines.push((ts, line));
        }
    }

    fn snapshot(&self, _channel: ChannelId, _window: TimeWindow) -> ChannelSnapshot {
        ChannelSnapshot::Float { ts: Vec::new(), vals: Vec::new() }
    }

    fn latest(&self, _channel: ChannelId) -> Option<(i64, Sample)> {
        None
    }

    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta {
        &self.metas[channel.0 as usize]
    }
}
```

Add to `src/record/lazy/mod.rs`:

```rust
pub mod decode_buf;
pub use decode_buf::{ChanSamples, ChunkDecodeBuf, DecodedChunk};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib record::lazy::decode_buf`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/record/lazy/mod.rs src/record/lazy/decode_buf.rs
git commit -m "feat: chunk decode scratch buffer and frozen decoded chunk"
```

---

## Task 4: ChunkCache (byte-budgeted LRU)

**Files:**
- Create: `src/record/lazy/cache.rs`
- Modify: `src/record/lazy/mod.rs` (export)

**Interfaces:**
- Consumes: `DecodedChunk` (Task 3).
- Produces:
  - `struct ChunkCache` with:
    - `fn new(cap_bytes: usize) -> ChunkCache`
    - `fn get_or_insert_with(&self, key: (usize, usize), make: impl FnOnce() -> DecodedChunk) -> Arc<DecodedChunk>` — returns cached or builds, inserts, and evicts oldest until `retained_bytes <= cap_bytes` (never evicts the just-inserted entry).
    - `fn retained_bytes(&self) -> usize` (test hook).

Interior mutability via `Mutex` (reads take `&self`). Recency tracked by a monotonic tick per access.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::lazy::decode_buf::{ChanSamples, DecodedChunk};

    // A decoded chunk of a chosen byte weight: N float samples ≈ N*16 bytes.
    fn chunk(n: usize) -> DecodedChunk {
        // Build via ChunkDecodeBuf so bytes() matches production accounting.
        use crate::config::ChannelRegistry;
        use crate::store::ChannelStore;
        use crate::types::{ChannelId, NumericVal};
        let r = ChannelRegistry::from_toml_str(r#"
[channels."a.f"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap();
        let buf = crate::record::lazy::decode_buf::ChunkDecodeBuf::new(&r);
        let id = r.id("a.f").unwrap();
        for i in 0..n as i64 {
            buf.write_numeric(id, i, NumericVal::Float(i as f64));
        }
        buf.freeze()
    }

    #[test]
    fn caches_and_reuses() {
        let cache = ChunkCache::new(10 * 1024 * 1024);
        let mut built = 0;
        let a = cache.get_or_insert_with((0, 0), || { built += 1; chunk(100) });
        let b = cache.get_or_insert_with((0, 0), || { built += 1; chunk(100) });
        assert_eq!(built, 1, "second get must hit the cache");
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn evicts_to_stay_under_cap() {
        // Cap holds ~one 1000-sample chunk (16KB). Insert three distinct chunks.
        let cap = chunk(1000).bytes() + 8;
        let cache = ChunkCache::new(cap);
        let _ = cache.get_or_insert_with((0, 0), || chunk(1000));
        let _ = cache.get_or_insert_with((0, 1), || chunk(1000));
        let _ = cache.get_or_insert_with((0, 2), || chunk(1000));
        assert!(cache.retained_bytes() <= cap, "retained {} > cap {}", cache.retained_bytes(), cap);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib record::lazy::cache`
Expected: FAIL to compile (`ChunkCache` not found).

- [ ] **Step 3: Implement `ChunkCache`**

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::decode_buf::DecodedChunk;

struct Entry {
    chunk: Arc<DecodedChunk>,
    bytes: usize,
    last_used: u64,
}

struct Inner {
    map: HashMap<(usize, usize), Entry>,
    tick: u64,
    retained: usize,
}

pub struct ChunkCache {
    cap_bytes: usize,
    inner: Mutex<Inner>,
}

impl ChunkCache {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            cap_bytes,
            inner: Mutex::new(Inner { map: HashMap::new(), tick: 0, retained: 0 }),
        }
    }

    pub fn retained_bytes(&self) -> usize {
        self.inner.lock().unwrap().retained
    }

    pub fn get_or_insert_with(
        &self,
        key: (usize, usize),
        make: impl FnOnce() -> DecodedChunk,
    ) -> Arc<DecodedChunk> {
        let mut inner = self.inner.lock().unwrap();
        inner.tick += 1;
        let tick = inner.tick;
        if let Some(e) = inner.map.get_mut(&key) {
            e.last_used = tick;
            return e.chunk.clone();
        }
        let chunk = Arc::new(make());
        let bytes = chunk.bytes();
        inner.retained += bytes;
        inner.map.insert(key, Entry { chunk: chunk.clone(), bytes, last_used: tick });
        // Evict least-recently-used until under cap, but never the entry we
        // just inserted (its last_used == tick is the maximum).
        while inner.retained > self.cap_bytes {
            let Some((&victim, _)) = inner
                .map
                .iter()
                .filter(|(&k, _)| k != key)
                .min_by_key(|(_, e)| e.last_used)
            else {
                break; // only the fresh entry remains
            };
            if let Some(e) = inner.map.remove(&victim) {
                inner.retained -= e.bytes;
            }
        }
        chunk
    }
}
```

Add to `src/record/lazy/mod.rs`:

```rust
pub mod cache;
pub use cache::ChunkCache;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib record::lazy::cache`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/record/lazy/mod.rs src/record/lazy/cache.rs
git commit -m "feat: byte-budgeted LRU cache for decoded chunks"
```

---

## Task 5: RecordingSource (mmap + chunk index + decode)

**Files:**
- Create: `src/record/lazy/source.rs`
- Modify: `src/record/lazy/mod.rs` (export)

**Interfaces:**
- Consumes: `memmap2::Mmap`; `mcap::{Summary, MessageStream}`; `ChunkDecodeBuf`/`DecodedChunk` (Task 3); the routing types `TopicRouter`, `ChannelBinding` and `decode_batch` (from `crate::ingest`); `ChannelRegistry`.
- Produces:
  - `struct ChunkSpan { pub start_ns: i64, pub end_ns: i64, pub idx: usize }`
  - `struct RecordingSource` with:
    - `fn open(path: &Path, registry: &ChannelRegistry) -> anyhow::Result<RecordingSource>` — mmap, read summary, build sorted spans, build routing (reuse the existing `plan_file` logic, which is moved here or called), compute `bounds`.
    - `pub bounds: Option<(i64, i64)>`
    - `fn overlapping(&self, window: TimeWindow) -> Vec<usize>` — indices into `spans` whose `[start,end]` intersect `[window.start, window.end)`.
    - `fn spans(&self) -> &[ChunkSpan]`
    - `fn decode_chunk(&self, span_idx: usize, registry: &ChannelRegistry) -> DecodedChunk` — decode exactly the one chunk (or the whole file, for the no-summary fallback) into a `ChunkDecodeBuf`, freeze.
    - `fn decode_all(&self, registry: &ChannelRegistry, sink: &dyn ChannelStore)` — stream every message through routing into `sink` (used by the envelope-building load pass; parallelizable per-span by the caller).

**Note on `plan_file`:** the current `PlaybackStore::plan_file` (playback.rs:330) builds `(TopicRouter, HashMap<String, Vec<ChannelBinding>>)` and registers reconstructed channels. Move this function (and its helpers `first_message_name`, `value_sample_type`) into `source.rs` as free functions or `RecordingSource` methods so both `RecordingSource::open` and the store can use them. Keep behaviour identical (verified by the existing playback tests that exercise MQTT reconstruction and type-conflict skip).

- [ ] **Step 1: Write the failing test**

Reuse the existing `write_test_mcap` helper pattern (copy a minimal version into this test module, or make the playback one `pub(crate)` — prefer a local minimal copy to keep the task self-contained). Test:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // ... local write_test_mcap producing a chunked, finalised MCAP ...

    #[test]
    fn open_indexes_chunks_and_decodes_one() {
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.mcap");
        write_test_mcap(&path, &schema, &[
            (1_000_000_000, 1.0),
            (2_000_000_000, 2.0),
            (3_000_000_000, 3.0),
        ]);
        let src = RecordingSource::open(&path, &registry).unwrap();
        assert_eq!(src.bounds, Some((1_000_000_000, 3_000_000_000)));
        assert!(!src.spans().is_empty());
        // Decode the first span; the accel.x channel must carry its samples.
        let id = registry.id("accel.x").unwrap();
        let chunk = src.decode_chunk(0, &registry);
        let snap = chunk.window(id.0 as usize, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
        match snap {
            crate::types::ChannelSnapshot::Float { ts, .. } => assert!(!ts.is_empty()),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn overlapping_selects_intersecting_spans() {
        // A file whose messages land in distinct chunks (write enough to force
        // multiple chunks, or assert the single-span case intersects correctly).
        let (schema, _d, registry) = make_proto_and_registry();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.mcap");
        write_test_mcap(&path, &schema, &[(1_000_000_000, 1.0), (5_000_000_000, 2.0)]);
        let src = RecordingSource::open(&path, &registry).unwrap();
        // Window before all data → no spans.
        assert!(src.overlapping(TimeWindow { start_ns: 0, end_ns: 500_000_000 }).is_empty());
        // Window covering the data → at least one span.
        assert!(!src.overlapping(TimeWindow { start_ns: 0, end_ns: i64::MAX }).is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib record::lazy::source`
Expected: FAIL to compile (`RecordingSource` not found).

- [ ] **Step 3: Implement `RecordingSource`**

Key points for the implementer:

```rust
use std::fs::File;
use std::path::Path;

use anyhow::Context;
use memmap2::Mmap;

use crate::config::ChannelRegistry;
use crate::ingest::decode::decode_batch;
use crate::ingest::router::{ChannelBinding, TopicRouter};
use crate::store::ChannelStore;
use crate::types::TimeWindow;
use std::collections::HashMap;

use super::decode_buf::{ChunkDecodeBuf, DecodedChunk};

pub struct ChunkSpan {
    pub start_ns: i64,
    pub end_ns: i64,
    pub idx: usize,
}

pub struct RecordingSource {
    mmap: Mmap,
    router: TopicRouter,
    reconstructed: HashMap<String, Vec<ChannelBinding>>,
    spans: Vec<ChunkSpan>,
    pub bounds: Option<(i64, i64)>,
}

impl RecordingSource {
    pub fn open(path: &Path, registry: &ChannelRegistry) -> anyhow::Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        // Safety: the file is opened read-only and not mutated for the mmap's
        // lifetime; MCAP playback is read-only.
        let mmap = unsafe { Mmap::map(&file) }
            .with_context(|| format!("mmapping {}", path.display()))?;
        let bytes: &[u8] = &mmap;

        let (router, reconstructed) = plan_file(bytes, registry)?; // moved from playback.rs

        let mut spans = Vec::new();
        if let Some(summary) = mcap::Summary::read(bytes).context("reading MCAP summary")? {
            for (i, ci) in summary.chunk_indexes.iter().enumerate() {
                spans.push(ChunkSpan {
                    start_ns: ci.message_start_time as i64,
                    end_ns: ci.message_end_time as i64,
                    idx: i,
                });
            }
        }
        spans.sort_by_key(|s| s.start_ns);

        let bounds = time_bounds(bytes)?; // moved from playback.rs

        // Fallback: no chunk index (unsummarised file) → one whole-file span so
        // decode_chunk(0) linear-scans the file.
        if spans.is_empty() {
            if let Some((mn, mx)) = bounds {
                spans.push(ChunkSpan { start_ns: mn, end_ns: mx, idx: usize::MAX });
            }
        }

        Ok(Self { mmap, router, reconstructed, spans, bounds })
    }

    pub fn spans(&self) -> &[ChunkSpan] {
        &self.spans
    }

    pub fn overlapping(&self, window: TimeWindow) -> Vec<usize> {
        // spans sorted by start_ns; a span intersects [start,end) iff
        // span.start < window.end && span.end >= window.start.
        self.spans
            .iter()
            .enumerate()
            .filter(|(_, s)| s.start_ns < window.end_ns && s.end_ns >= window.start_ns)
            .map(|(i, _)| i)
            .collect()
    }

    fn decode_into(&self, span: &ChunkSpan, sink: &dyn ChannelStore) -> anyhow::Result<()> {
        let bytes: &[u8] = &self.mmap;
        if span.idx == usize::MAX {
            // Whole-file linear scan (no summary).
            for message in mcap::MessageStream::new(bytes).context("opening MCAP message stream")? {
                let msg = message.context("reading MCAP message")?;
                decode_message(&msg, &self.router, &self.reconstructed, sink);
            }
            return Ok(());
        }
        let summary = mcap::Summary::read(bytes)
            .context("reading MCAP summary")?
            .context("MCAP summary missing")?;
        let index = &summary.chunk_indexes[span.idx];
        for message in summary.stream_chunk(bytes, index).context("streaming MCAP chunk")? {
            let msg = message.context("reading MCAP message")?;
            decode_message(&msg, &self.router, &self.reconstructed, sink);
        }
        Ok(())
    }

    pub fn decode_chunk(&self, span_idx: usize, registry: &ChannelRegistry) -> DecodedChunk {
        let buf = ChunkDecodeBuf::new(registry);
        if let Some(span) = self.spans.get(span_idx) {
            let _ = self.decode_into(span, &buf); // decode errors already logged per-message
        }
        buf.freeze()
    }

    /// Stream every span's messages into `sink`. `decode_message` writes through
    /// `&sink`, so a caller can hand a shared envelope-building sink.
    pub fn decode_all(&self, sink: &dyn ChannelStore) -> anyhow::Result<()> {
        for span in &self.spans {
            self.decode_into(span, sink)?;
        }
        Ok(())
    }
}
```

Move `plan_file`, `first_message_name`, `value_sample_type`, `time_bounds`, and `decode_message` out of `playback.rs` into `source.rs` (make `decode_message` and `plan_file` `pub(crate)` so the store can still call them). Update `playback.rs` imports accordingly in Task 6.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib record::lazy::source`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/record/lazy/mod.rs src/record/lazy/source.rs src/record/playback.rs
git commit -m "feat: memory-mapped recording source with chunk index and on-demand decode"
```

---

## Task 6: Rework PlaybackStore onto the lazy components (detail path)

**Files:**
- Modify: `src/record/playback.rs` (replace the internals; keep the public surface)

**Interfaces:**
- Consumes: `RecordingSource`, `Envelope`, `ChunkCache`, `DecodedChunk`, `ChanSamples` (Tasks 2–5).
- Produces: the reworked `PlaybackStore`:
  - fields: `sources: Vec<RecordingSource>`, `envelope: Envelope`, `text: Vec<Option<Vec<(i64,String)>>>` (retained text per channel, `None` for non-text), `cache: ChunkCache`, `metas: Vec<ChannelMeta>`, `position_ns: Arc<AtomicI64>`, `duration_ns: i64`, `start_ns: i64`, `breaks: Vec<i64>`, and the config knobs `chunk_budget: usize`, plus the registry snapshot needed to build `ChunkDecodeBuf` on demand (store an `Arc<ChannelRegistry>` or a cloned minimal channel-type table — check whether `ChannelRegistry` is `Clone`/shareable; if not, store the per-channel `SampleType` vec and a small constructor).
  - `load` / `load_many` unchanged signatures returning `Arc<Self>`.
  - `ChannelStore` impl: `snapshot` (detail path only in this task — wide-zoom in Task 7), `latest`/`latest_at` (Task 8 refines; here provide a correct-but-simple version), `channel_meta`, `now_ns`, `break_times`.

**Registry-on-demand note:** `snapshot`/`latest` must build a `ChunkDecodeBuf` (needs a `ChannelRegistry`). `ChannelRegistry` is currently passed by `&` into `load_many`. Store what `ChunkDecodeBuf::new` needs. Simplest: give `ChunkDecodeBuf` a second constructor `from_metas(metas: &[ChannelMeta]) -> ChunkDecodeBuf` and keep `metas` in the store (already needed for `channel_meta`). Update Task 3's `ChunkDecodeBuf` to expose `from_metas` — do this as part of THIS task if not already present, and add a unit test for it in `decode_buf.rs`.

- [ ] **Step 1: Add `ChunkDecodeBuf::from_metas` (+ test) in decode_buf.rs**

```rust
impl ChunkDecodeBuf {
    pub fn from_metas(metas: &[ChannelMeta]) -> Self {
        Self {
            channels: metas.iter().map(|m| Mutex::new(ChanSamples::for_type(m.sample_type))).collect(),
            metas: metas.to_vec(),
        }
    }
}
```

Test:

```rust
#[test]
fn from_metas_matches_registry_shape() {
    use crate::types::{ChannelMeta, SampleType, ChannelId, NumericVal};
    let metas = vec![ChannelMeta {
        name: "x".into(), sample_type: SampleType::Float, unit: String::new(),
        color: "#fff".into(), max_rate: 1, history_s: 1.0, max_lines: 1, text_coded: false,
    }];
    let buf = ChunkDecodeBuf::from_metas(&metas);
    buf.write_numeric(ChannelId(0), 1, NumericVal::Float(2.0));
    let c = buf.freeze();
    assert!(c.bytes() > 0);
}
```

Run: `cargo test --lib record::lazy::decode_buf` → PASS.

- [ ] **Step 2: Write the failing store test (detail path exactness)**

Replace/keep the existing playback tests. Add — reusing the file's existing `write_test_mcap`:

```rust
#[test]
fn detail_snapshot_matches_exact_samples() {
    let (schema, _d, registry) = make_proto_and_registry();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.mcap");
    write_test_mcap(&path, &schema, &[
        (1_000_000_000, 1.0),
        (2_000_000_000, 2.0),
        (3_000_000_000, 3.0),
    ]);
    let store = PlaybackStore::load(&path, &registry).unwrap();
    let id = registry.id("accel.x").unwrap();
    // Narrow window (few chunks) → exact raw samples.
    let snap = store.snapshot(id, TimeWindow { start_ns: 1_000_000_000, end_ns: 3_000_000_000 });
    match snap {
        ChannelSnapshot::Float { ts, vals } => {
            assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
            assert!((vals[0] - 1.0).abs() < 1e-9 && (vals[1] - 2.0).abs() < 1e-9);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

The existing tests `load_and_snapshot_returns_data_in_window`, `latest_returns_sample_at_or_before_position`, `duration_and_start_ns_computed_from_data`, `load_many_preserves_timestamps_merges_channels_and_records_break`, `load_many_break_is_independent_of_file_order`, the MQTT reconstruction and type-conflict tests MUST still pass unchanged — they define the preserved behaviour.

- [ ] **Step 3: Run to verify current tests still compile against the new struct**

Run: `cargo test --lib record::playback`
Expected: FAIL (struct not yet reworked). This step confirms the test set is the target.

- [ ] **Step 4: Rework the struct and `load_many`**

`load_many` new shape:

```rust
pub fn load_many(paths: &[&Path], registry: &ChannelRegistry) -> anyhow::Result<Arc<Self>> {
    anyhow::ensure!(!paths.is_empty(), "no recording files to load");

    // Open (mmap + index + routing) every file first, registering channels.
    let mut sources = Vec::with_capacity(paths.len());
    for path in paths {
        sources.push(RecordingSource::open(path, registry)?);
    }

    // Timeline + breaks from per-file bounds (no decode).
    let mut mins: Vec<i64> = sources.iter().filter_map(|s| s.bounds.map(|(mn, _)| mn)).collect();
    let global_min = mins.iter().copied().min();
    let global_max = sources.iter().filter_map(|s| s.bounds.map(|(_, mx)| mx)).max();
    mins.sort_unstable();
    let breaks: Vec<i64> = mins.into_iter().skip(1).collect();

    let metas: Vec<ChannelMeta> = registry.iter_ids().map(|id| registry.meta(id).clone()).collect();
    let (start_ns, duration_ns) = match (global_min, global_max) {
        (Some(mn), Some(mx)) if mn <= mx => (mn, mx - mn),
        _ => (0, 0),
    };

    // One parallel pass builds the envelope + retained text; retains no numeric.
    let nchannels = metas.len();
    let mut envelope = Envelope::new(nchannels, start_ns, duration_ns, ENVELOPE_BUCKETS);
    let mut text: Vec<Option<Vec<(i64, String)>>> = metas
        .iter()
        .map(|m| (m.sample_type == SampleType::Text).then(Vec::new))
        .collect();
    // ... parallel per-span decode into thread-local EnvelopeSink, then merge
    //     (see Task 7 for the sink; in THIS task a simple sequential build is
    //     acceptable, parallelised in Task 7). ...

    Ok(Arc::new(Self {
        sources, envelope, text,
        cache: ChunkCache::new(CACHE_BYTES),
        metas,
        position_ns: Arc::new(AtomicI64::new(start_ns)),
        duration_ns, start_ns, breaks,
        chunk_budget: CHUNK_BUDGET,
    }))
}
```

Constants near the top of the file:

```rust
const ENVELOPE_BUCKETS: usize = 16384;
const CACHE_BYTES: usize = 512 * 1024 * 1024;
const CHUNK_BUDGET: usize = 16;
```

`snapshot` detail path (numeric) and text path:

```rust
fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot {
    let ch = channel.0 as usize;
    // Text: retained full, exact slice.
    if let Some(Some(lines)) = self.text.get(ch) {
        let s = lines.partition_point(|(t, _)| *t < window.start_ns);
        let e = lines.partition_point(|(t, _)| *t < window.end_ns);
        return ChannelSnapshot::Text { lines: lines[s..e].to_vec() };
    }
    // Numeric: gather overlapping chunks across sources.
    let mut overlaps: Vec<(usize, usize)> = Vec::new(); // (source, span_idx)
    for (si, src) in self.sources.iter().enumerate() {
        for span_idx in src.overlapping(window) {
            overlaps.push((si, span_idx));
        }
    }
    if overlaps.len() > self.chunk_budget {
        return self.snapshot_overview(ch, window); // Task 7
    }
    // Detail: decode each overlapping chunk (cached), window-slice, concat.
    let mut acc = SnapshotAcc::new();
    for (si, span_idx) in overlaps {
        let chunk = self.cache.get_or_insert_with((si, span_idx), || {
            self.sources[si].decode_chunk(span_idx, /* registry via metas */ &self.metas)
        });
        acc.extend(chunk.window(ch, window));
    }
    acc.into_snapshot()
}
```

Where `decode_chunk` is adjusted (Task 5 signature) to take `&[ChannelMeta]` and build the buf via `ChunkDecodeBuf::from_metas` — update Task 5's `decode_chunk` signature to `decode_chunk(&self, span_idx: usize, metas: &[ChannelMeta]) -> DecodedChunk` accordingly. `SnapshotAcc` is a small helper that concatenates same-variant `ChannelSnapshot`s (Float/Int/Bool/Text) preserving order; since spans are decoded in overlap order and each chunk is internally sorted and chunks are time-disjoint, concatenation yields sorted output. Implement `SnapshotAcc` inline in playback.rs with a unit test.

- [ ] **Step 5: Implement `snapshot_overview` as a temporary stub delegating to envelope-less empty**

To keep this task self-contained and green, implement `snapshot_overview` minimally as: read the envelope (Task 2 `envelope.read`) and wrap as `ChannelSnapshot::Float`/matching the channel's numeric type. (Task 7 makes the parallel builder fill the envelope; if the envelope is empty because this task builds it sequentially, that's fine — the sequential build in Step 4 already fills it.)

```rust
fn snapshot_overview(&self, ch: usize, window: TimeWindow) -> ChannelSnapshot {
    let pts = self.envelope.read(ch, window);
    let ts: Vec<i64> = pts.iter().map(|(t, _)| *t).collect();
    match self.metas[ch].sample_type {
        SampleType::Int => ChannelSnapshot::Int { ts, vals: pts.iter().map(|(_, v)| *v as i64).collect() },
        SampleType::Bool => ChannelSnapshot::Bool { ts, vals: pts.iter().map(|(_, v)| (*v != 0.0) as u8).collect() },
        _ => ChannelSnapshot::Float { ts, vals: pts.iter().map(|(_, v)| *v).collect() },
    }
}
```

- [ ] **Step 6: Run the full playback test module**

Run: `cargo test --lib record::playback`
Expected: PASS — all preserved tests plus `detail_snapshot_matches_exact_samples`.

- [ ] **Step 7: Full build + clippy + whole suite**

Run: `cargo build && cargo clippy --all-targets && cargo test --lib`
Expected: clean, all green (313+ tests).

- [ ] **Step 8: Commit**

```bash
git add src/record/playback.rs src/record/lazy/decode_buf.rs src/record/lazy/source.rs
git commit -m "feat: rework PlaybackStore onto mmap + on-demand chunk decode"
```

---

## Task 7: Parallel envelope build + wide-zoom overview path

**Files:**
- Modify: `src/record/playback.rs` (parallelise the load pass)
- Modify: `src/record/lazy/envelope.rs` (add an `EnvelopeSink: ChannelStore` adapter, or fold via `ChunkDecodeBuf` per span)

**Interfaces:**
- Consumes: `Envelope::{new, fold_numeric, merge}` (Task 2), `RecordingSource::spans/decode_into` (Task 5), `thread::scope`.
- Produces: a parallel builder that produces the same `Envelope` + retained text as the sequential version, and a verified wide-zoom `snapshot` overview.

- [ ] **Step 1: Write the failing test (overview path triggers past budget)**

```rust
#[test]
fn wide_window_uses_envelope_overview() {
    let (schema, _d, registry) = make_proto_and_registry();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.mcap");
    // Write enough messages across enough chunks that a whole-file window
    // exceeds CHUNK_BUDGET spans. (write_test_mcap writes one message per call;
    // ensure the writer chunks — mcap::Writer chunks by default. Write, say,
    // 5000 samples so multiple chunks form.)
    let msgs: Vec<(i64, f32)> = (0..5000).map(|i| (i as i64 * 1_000_000 + 1, (i % 7) as f32)).collect();
    write_test_mcap(&path, &schema, &msgs);
    let store = PlaybackStore::load(&path, &registry).unwrap();
    let id = registry.id("accel.x").unwrap();
    let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
    let snap = store.snapshot(id, all);
    // Overview returns far fewer points than 5000 (bucketed min/max), but not empty.
    match snap {
        ChannelSnapshot::Float { ts, .. } => {
            assert!(!ts.is_empty());
            assert!(ts.len() < 5000, "overview must be decimated, got {}", ts.len());
            assert!(ts.windows(2).all(|w| w[0] <= w[1]), "overview must be time-ordered");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

If `write_test_mcap` does not produce multiple chunks even at 5000 messages, adjust the helper to flush chunks (e.g. call `writer.flush()`/rely on `mcap::Writer`'s default chunk size) — document the exact mechanism used; do not assert on an unchunked file.

- [ ] **Step 2: Run to verify it fails or is flaky on the sequential build**

Run: `cargo test --lib record::playback::tests::wide_window_uses_envelope_overview`
Expected: passes only if the sequential envelope from Task 6 is correct; if Task 6 left the envelope empty on the parallel path, it FAILS (empty overview). Either way this test now pins the behaviour.

- [ ] **Step 3: Implement the parallel envelope build**

Mirror the existing `thread::scope` per-chunk split (playback.rs old lines 201–258), but each worker folds into a thread-local `Envelope` (and thread-local text vecs), then merge:

```rust
let nthreads = std::thread::available_parallelism().map_or(1, |n| n.get());
// Flatten all (source, span_idx) pairs, split across workers.
let jobs: Vec<(usize, usize)> = self.sources.iter().enumerate()
    .flat_map(|(si, s)| (0..s.spans().len()).map(move |k| (si, k)))
    .collect();
let workers = nthreads.min(jobs.len().max(1));
let partials: Vec<(Envelope, Vec<Option<Vec<(i64,String)>>>)> = std::thread::scope(|scope| {
    let handles: Vec<_> = (0..workers).map(|w| {
        let lo = w * jobs.len() / workers;
        let hi = (w + 1) * jobs.len() / workers;
        let sources = &sources; let metas = &metas;
        scope.spawn(move || {
            let mut env = Envelope::new(nchannels, start_ns, duration_ns, ENVELOPE_BUCKETS);
            let mut txt: Vec<Option<Vec<(i64,String)>>> =
                metas.iter().map(|m| (m.sample_type == SampleType::Text).then(Vec::new)).collect();
            for &(si, k) in &jobs[lo..hi] {
                let buf = ChunkDecodeBuf::from_metas(metas);
                let _ = sources[si].decode_into(&sources[si].spans()[k], &buf);
                let chunk = buf.freeze();
                for (ch, cs) in chunk.channels.iter().enumerate() {
                    match cs {
                        ChanSamples::Float { ts, vals } =>
                            for (t, v) in ts.iter().zip(vals) { env.fold_numeric(ch, *t, *v); },
                        ChanSamples::Int { ts, vals } =>
                            for (t, v) in ts.iter().zip(vals) { env.fold_numeric(ch, *t, *v as f64); },
                        ChanSamples::Bool { ts, vals } =>
                            for (t, v) in ts.iter().zip(vals) { env.fold_numeric(ch, *t, *v as f64); },
                        ChanSamples::Text { lines } =>
                            if let Some(Some(dst)) = txt.get_mut(ch) { dst.extend(lines.iter().cloned()); },
                    }
                }
            }
            (env, txt)
        })
    }).collect();
    handles.into_iter().map(|h| h.join().expect("envelope worker panicked")).collect()
});
// Merge partial envelopes and text.
for (penv, ptxt) in partials {
    envelope.merge(&penv);
    for (ch, slot) in ptxt.into_iter().enumerate() {
        if let (Some(dst), Some(src)) = (text.get_mut(ch).and_then(|o| o.as_mut()), slot) {
            dst.extend(src);
        }
    }
}
// Text must end sorted by time (chunks are time-ordered per file, but files
// may interleave): sort each retained text channel.
for slot in text.iter_mut().flatten() {
    slot.sort_by_key(|(t, _)| *t);
}
```

`decode_into` must be `pub(crate)` and `spans()` public on `RecordingSource` (Task 5 already exposes `spans()`; make `decode_into` `pub(crate)`).

- [ ] **Step 4: Run the overview + full playback tests**

Run: `cargo test --lib record::playback`
Expected: PASS including `wide_window_uses_envelope_overview`.

- [ ] **Step 5: Commit**

```bash
git add src/record/playback.rs src/record/lazy/source.rs src/record/lazy/envelope.rs
git commit -m "feat: parallel envelope build and wide-zoom overview snapshot"
```

---

## Task 8: latest / latest_at via targeted chunk decode

**Files:**
- Modify: `src/record/playback.rs`

**Interfaces:**
- Consumes: `RecordingSource::{spans, overlapping, decode_chunk}`, `ChunkCache`, retained text.
- Produces: `latest`/`latest_at` that decode only the chunk containing the position, matching today's "last sample at or before position" semantics.

- [ ] **Step 1: Confirm the existing latest test is the target**

`latest_returns_sample_at_or_before_position` (already in the file) drives `position_ns` to four values and asserts the last-≤ sample. Keep it. Add a cross-chunk test:

```rust
#[test]
fn latest_at_decodes_correct_chunk_across_span_boundary() {
    let (schema, _d, registry) = make_proto_and_registry();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("l.mcap");
    let msgs: Vec<(i64, f32)> = (0..4000).map(|i| (i as i64 + 1, i as f32)).collect();
    write_test_mcap(&path, &schema, &msgs);
    let store = PlaybackStore::load(&path, &registry).unwrap();
    let id = registry.id("accel.x").unwrap();
    // A position deep in the file returns the exact last sample ≤ it.
    let (t, s) = store.latest_at(id, 3500).unwrap();
    assert_eq!(t, 3500);
    match s { Sample::Float(v) => assert!((v - 3499.0).abs() < 1e-6), other => panic!("{other:?}") }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib record::playback::tests::latest_at_decodes_correct_chunk_across_span_boundary`
Expected: FAIL (default `latest_at` from Task 6 may scan wrong or return None).

- [ ] **Step 3: Implement `latest` / `latest_at`**

```rust
fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)> {
    self.latest_at(channel, self.position_ns.load(Ordering::Relaxed))
}
```

Override `latest_at` in the impl (the trait provides a default, but we want targeted decode):

```rust
fn latest_at(&self, channel: ChannelId, end_ns: i64) -> Option<(i64, Sample)> {
    let ch = channel.0 as usize;
    if let Some(Some(lines)) = self.text.get(ch) {
        let idx = lines.partition_point(|(t, _)| *t <= end_ns);
        return (idx > 0).then(|| (lines[idx-1].0, Sample::Text(lines[idx-1].1.clone())));
    }
    // Numeric: find, across sources, the span with the greatest start_ns whose
    // start_ns <= end_ns; decode it and take the last sample <= end_ns. If that
    // chunk has none <= end_ns (position between chunks), fall back to the prior
    // chunk. Simplest correct approach: scan candidate spans from latest to
    // earliest until a sample <= end_ns is found.
    let mut candidates: Vec<(usize, usize, i64)> = Vec::new(); // (src, span_idx, start_ns)
    for (si, src) in self.sources.iter().enumerate() {
        for (k, span) in src.spans().iter().enumerate() {
            if span.start_ns <= end_ns {
                candidates.push((si, k, span.start_ns));
            }
        }
    }
    candidates.sort_by_key(|(_, _, s)| *s);
    for (si, k, _) in candidates.into_iter().rev() {
        let chunk = self.cache.get_or_insert_with((si, k), || self.sources[si].decode_chunk(k, &self.metas));
        if let Some(hit) = last_le(&chunk, ch, end_ns) {
            return Some(hit);
        }
    }
    None
}
```

`last_le(chunk, ch, end_ns)` reads `chunk.channels[ch]`, `partition_point(|t| t <= end_ns)`, returns the last sample as the right `Sample` variant, or `None` if the chunk has no sample ≤ `end_ns`. Implement it in playback.rs (or as a `DecodedChunk::last_le` method with its own unit test in decode_buf.rs — preferred, since it belongs to `DecodedChunk`).

- [ ] **Step 4: Run latest tests + full suite**

Run: `cargo test --lib record::playback && cargo test --lib`
Expected: PASS, all green.

- [ ] **Step 5: Commit**

```bash
git add src/record/playback.rs src/record/lazy/decode_buf.rs
git commit -m "feat: latest/latest_at via targeted chunk decode"
```

---

## Task 9: Cache-bound integration test, cleanup, docs

**Files:**
- Modify: `src/record/playback.rs` (expose a test hook for retained cache bytes if needed; final tidy)
- Modify: `README`/crate docs if they describe playback memory behaviour (check `src/lib.rs` crate-level docs and `src/record/mod.rs`).

**Interfaces:**
- Consumes: everything above.
- Produces: an end-to-end memory-bound test and updated docs. No new public API beyond a `#[cfg(test)]` accessor for `cache.retained_bytes()` if a store-level assertion is wanted.

- [ ] **Step 1: Write the cache-bound integration test**

Set a tiny cache to force eviction and assert bytes stay bounded while reads remain correct. Since `CACHE_BYTES` is a const, add a `#[cfg(test)]` constructor `load_many_with_caps(paths, registry, cache_bytes, chunk_budget)` used only by tests, or make the three consts overridable via a private `Config` the public `load_many` fills with defaults. Prefer a small private `struct Caps { cache_bytes, envelope_buckets, chunk_budget }` with a `Default`, and a `pub(crate) fn load_many_with(paths, registry, caps)`.

```rust
#[test]
fn cache_stays_within_cap_while_scrubbing() {
    let (schema, _d, registry) = make_proto_and_registry();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scrub.mcap");
    let msgs: Vec<(i64, f32)> = (0..8000).map(|i| (i as i64 + 1, i as f32)).collect();
    write_test_mcap(&path, &schema, &msgs);
    let caps = Caps { cache_bytes: 64 * 1024, envelope_buckets: 1024, chunk_budget: 4 };
    let store = PlaybackStore::load_many_with(&[&path], &registry, caps).unwrap();
    let id = registry.id("accel.x").unwrap();
    // Scrub many narrow windows across the file (forces many distinct chunks).
    for start in (0..8000i64).step_by(200) {
        let w = TimeWindow { start_ns: start, end_ns: start + 100 };
        let _ = store.snapshot(id, w);
        assert!(store.cache_retained_bytes() <= 64 * 1024,
            "cache exceeded cap: {}", store.cache_retained_bytes());
    }
}
```

Add `pub(crate) fn cache_retained_bytes(&self) -> usize { self.cache.retained_bytes() }`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib record::playback::tests::cache_stays_within_cap_while_scrubbing`
Expected: FAIL to compile (`Caps`/`load_many_with`/`cache_retained_bytes` not present).

- [ ] **Step 3: Implement `Caps` + `load_many_with`; route `load_many` through it**

```rust
pub(crate) struct Caps {
    pub cache_bytes: usize,
    pub envelope_buckets: usize,
    pub chunk_budget: usize,
}
impl Default for Caps {
    fn default() -> Self {
        Caps { cache_bytes: CACHE_BYTES, envelope_buckets: ENVELOPE_BUCKETS, chunk_budget: CHUNK_BUDGET }
    }
}
pub fn load_many(paths: &[&Path], registry: &ChannelRegistry) -> anyhow::Result<Arc<Self>> {
    Self::load_many_with(paths, registry, Caps::default())
}
pub(crate) fn load_many_with(paths: &[&Path], registry: &ChannelRegistry, caps: Caps) -> anyhow::Result<Arc<Self>> {
    // ... body from Tasks 6/7, using caps.* instead of the bare consts ...
}
```

- [ ] **Step 4: Run the bound test + full suite**

Run: `cargo test --lib record::playback::tests::cache_stays_within_cap_while_scrubbing && cargo test --lib`
Expected: PASS, all green.

- [ ] **Step 5: Update docs**

In the crate-level / `src/record/mod.rs` docs, add a short paragraph: playback is now memory-mapped and decodes chunks on demand; resident memory is bounded by the chunk cache (512 MB default), the min/max envelope (16384 buckets/channel), and retained text; recordings larger than RAM are supported, with the caveats that text/log channels are retained in full and very-wide zooms serve a decimated min/max overview. Keep it to the style/length of the existing module docs (check `git show 1273851 -- src` for the doc conventions).

- [ ] **Step 6: Manual run check (drive the app)**

Run the app on a real (large, if available) recording and confirm playback + scrub + zoom-out render without OOM. Use the project's run path (check for a `run` skill / `cargo run` entry). Report what was observed. If no large file is available, load a normal recording and confirm no regression in waveform/state/log panels.

- [ ] **Step 7: Commit**

```bash
git add src/record/playback.rs src/record/mod.rs
git commit -m "test: cache memory-bound integration test; docs for lazy playback"
```

---

## Self-Review Notes (author)

- **Spec coverage:** mmap (Task 1,5) · chunk index/overlap (5) · envelope build+read (2,7) · detail decode+cache (3,4,6) · wide-zoom overview (7) · latest_at targeted decode (8) · text retained (6) · stitching/breaks preserved (6, existing tests) · memory bound (9) · knobs (6,9). No StateSpans (dropped per approved decision). All spec sections map to a task.
- **Type consistency:** `ChunkSpan`, `ChanSamples`, `DecodedChunk`, `ChunkDecodeBuf::{new,from_metas,freeze}`, `Envelope::{new,fold_numeric,merge,read}`, `ChunkCache::{new,get_or_insert_with,retained_bytes}`, `RecordingSource::{open,spans,overlapping,decode_chunk(&self,usize,&[ChannelMeta]),decode_into(pub(crate)),decode_all}` used consistently across tasks. `decode_chunk` signature settled as `(&self, span_idx, metas: &[ChannelMeta])` (noted in Tasks 5 and 6).
- **Preserved public surface:** `PlaybackStore::{load,load_many}`, fields `position_ns/duration_ns/start_ns`, `ChannelStore` impl — `app.rs` untouched.
- **Known limitations (documented, not bugs):** blocky mid-zoom (single-level envelope); approximate state bands for high-rate Int at whole-file zoom; text/log retained O(file).
