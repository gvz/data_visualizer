# Async MCAP Decode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `PlaybackStore` reads non-blocking so scrubbing a large MCAP never freezes the UI — detail chunks decode on background workers; the render thread returns instantly with cached detail or the coarse envelope overview.

**Architecture:** Move decode state into an `Arc<DecodeShared>` (sources + cache + metas + in-flight set + repaint waker) shared by the store and a small worker pool. `snapshot`/`latest_at` probe the cache without decoding; on a miss they enqueue the chunk and return the overview (coarse), sharpening on a later repaint after a worker fills the cache. See `docs/superpowers/specs/2026-08-18-async-mcap-decode-design.md`.

**Tech Stack:** Rust, `crossbeam_channel` (already a dependency) for the bounded MPMC job queue, `std::thread` workers, existing `ChunkCache`/`RecordingSource::decode_chunk_topic`, egui `request_repaint` as the waker.

## Global Constraints

- Never add a Co-Authored-By / self-attribution line to any commit or PR.
- Never push to a remote without explicit approval; local commits are fine.
- Never stage or commit `config.toml` (it stays dirty/untracked).
- No new external crates. Use `crossbeam_channel` (already used in `app.rs`) and `std`.
- Both test suites stay green after every task: `cargo test --no-default-features --lib` and `cargo test --lib`. Baseline before this plan: 404 (no-default) / 412 (default).
- The synchronous detail result (when every needed chunk is cached) must remain byte-identical to today's output — existing playback tests must not change their assertions.
- Tests must be deterministic: no `sleep`. Drive worker decode to completion via the test hook, or use a zero-worker store and drain the queue on the test thread.

---

### Task 1: Non-inserting cache lookup `ChunkCache::get`

**Files:**
- Modify: `src/record/lazy/cache.rs`

**Interfaces:**
- Produces: `ChunkCache::get(&self, key: (usize, usize, u32)) -> Option<Arc<DecodedChunk>>` — returns the cached chunk (updating LRU recency) or `None`, never decoding.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/record/lazy/cache.rs`:

```rust
#[test]
fn get_returns_none_on_miss_and_hit_after_insert() {
    let cache = ChunkCache::new(10 * 1024 * 1024);
    assert!(cache.get((0, 0, 0)).is_none(), "miss must be None");
    let a = cache.get_or_insert_with((0, 0, 0), || chunk(50));
    let g = cache.get((0, 0, 0)).expect("must hit after insert");
    assert!(Arc::ptr_eq(&a, &g));
}
```

- [ ] **Step 2: Run it to see it fail**

Run: `cargo test --no-default-features --lib cache::tests::get_returns_none_on_miss_and_hit_after_insert`
Expected: FAIL — no method named `get`.

- [ ] **Step 3: Implement `get`**

Add to `impl ChunkCache`:

```rust
/// Non-inserting lookup for the render thread: returns the cached chunk and
/// bumps its LRU recency, or `None` without decoding. The async detail path
/// uses this so a cache miss never blocks on a decode.
pub fn get(&self, key: (usize, usize, u32)) -> Option<Arc<DecodedChunk>> {
    let mut inner = self.inner.lock().unwrap();
    inner.tick += 1;
    let tick = inner.tick;
    let e = inner.map.get_mut(&key)?;
    e.last_used = tick;
    Some(e.chunk.clone())
}
```

- [ ] **Step 4: Run the test to see it pass**

Run: `cargo test --no-default-features --lib cache::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/record/lazy/cache.rs
git commit -m "feat(cache): add non-inserting ChunkCache::get for the async detail path"
```

---

### Task 2: `DecodeShared` + background worker pool

**Files:**
- Create: `src/record/lazy/decode_service.rs`
- Modify: `src/record/lazy/mod.rs` (add `pub mod decode_service;` and re-exports)

**Interfaces:**
- Consumes: `RecordingSource` (Task nothing new), `ChunkCache::get`/`get_or_insert_with` (Task 1), `RecordingSource::decode_chunk_topic`.
- Produces:
  - `type Job = (usize, usize, u32);` (source, chunk, topic_key — the cache key).
  - `struct DecodeShared { sources: Vec<RecordingSource>, cache: ChunkCache, metas: Vec<ChannelMeta>, in_flight: Mutex<HashSet<Job>>, waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>, active: Arc<AtomicUsize> }`
  - `impl DecodeShared`: `cached(&self, key: Job) -> Option<Arc<DecodedChunk>>`, `decode_and_cache(&self, key: Job)`, `set_waker(&self, f: Box<dyn Fn() + Send + Sync>)`, `wake(&self)`.
  - `fn spawn_decode_workers(shared: Arc<DecodeShared>, count: usize) -> crossbeam_channel::Sender<Job>` — creates a bounded channel (capacity 64), spawns `count` detached workers, returns the `Sender`. When the returned `Sender` (and all clones) drop, workers exit.

- [ ] **Step 1: Write the file with its unit tests**

Create `src/record/lazy/decode_service.rs`:

```rust
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{bounded, Sender};

use super::cache::ChunkCache;
use super::decode_buf::DecodedChunk;
use super::source::RecordingSource;
use crate::types::ChannelMeta;

/// A decode request: the `(source, chunk, topic_key)` cache key.
pub type Job = (usize, usize, u32);

/// Bounded job-queue capacity. Small so fast scrubbing that outpaces decode
/// drops stale requests (via `try_send`) instead of growing an unbounded
/// backlog; still-visible windows are re-requested next frame.
const QUEUE_CAP: usize = 64;

/// Read-only decode state shared by the store and the worker pool. Every field
/// a decode reads (mmap, parsed summary, bindings) is immutable after `open`,
/// so decoding is safe from any number of threads.
pub struct DecodeShared {
    pub sources: Vec<RecordingSource>,
    pub cache: ChunkCache,
    pub metas: Vec<ChannelMeta>,
    /// Keys queued or being decoded, so a key is enqueued at most once.
    in_flight: Mutex<HashSet<Job>>,
    /// Set by the app to `move || ctx.request_repaint()`. `None` in headless
    /// contexts and tests (wake is then a no-op).
    waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Live worker count, for the lifecycle test to observe shutdown.
    active: Arc<AtomicUsize>,
}

impl DecodeShared {
    pub fn new(sources: Vec<RecordingSource>, cache: ChunkCache, metas: Vec<ChannelMeta>) -> Self {
        Self {
            sources,
            cache,
            metas,
            in_flight: Mutex::new(HashSet::new()),
            waker: Mutex::new(None),
            active: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Cached chunk without decoding (render-thread probe).
    pub fn cached(&self, key: Job) -> Option<Arc<DecodedChunk>> {
        self.cache.get(key)
    }

    /// Decode one chunk (if not already cached), clear its in-flight mark, and
    /// wake the UI. Runs on a worker thread.
    pub fn decode_and_cache(&self, key: Job) {
        let (si, span_idx, topic_key) = key;
        self.cache.get_or_insert_with(key, || {
            self.sources[si].decode_chunk_topic(span_idx, topic_key, &self.metas)
        });
        self.in_flight.lock().unwrap().remove(&key);
        self.wake();
    }

    /// Reserve `key` for decode if it is neither cached nor already in flight.
    /// Returns true iff the caller should now `try_send` it. Locks the cache
    /// first, releases it, then locks `in_flight` — the SAME order as
    /// `decode_and_cache` (cache → in_flight) so the two never deadlock.
    pub fn reserve(&self, key: Job) -> bool {
        if self.cache.get(key).is_some() {
            return false;
        }
        let mut inflight = self.in_flight.lock().unwrap();
        if inflight.contains(&key) {
            return false;
        }
        inflight.insert(key);
        true
    }

    /// Undo a `reserve` whose `try_send` failed (queue full), so the key is
    /// retried on a later frame.
    pub fn unreserve(&self, key: Job) {
        self.in_flight.lock().unwrap().remove(&key);
    }

    pub fn set_waker(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.waker.lock().unwrap() = Some(f);
    }

    pub fn wake(&self) {
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w();
        }
    }

    #[cfg(test)]
    pub fn active_workers(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

/// Spawn `count` detached workers draining a bounded job queue. Returns the
/// `Sender`; dropping every `Sender` closes the channel and each worker's
/// `recv` returns `Err`, ending the loop. No `Arc` cycle: workers hold
/// `Arc<DecodeShared>` and the `Receiver`, never the store or the `Sender`.
pub fn spawn_decode_workers(shared: Arc<DecodeShared>, count: usize) -> Sender<Job> {
    let (tx, rx) = bounded::<Job>(QUEUE_CAP);
    for _ in 0..count {
        let shared = shared.clone();
        let rx = rx.clone();
        shared.active.fetch_add(1, Ordering::SeqCst);
        let active = shared.active.clone();
        std::thread::spawn(move || {
            while let Ok(key) = rx.recv() {
                shared.decode_and_cache(key);
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_decodes_missing_chunk_and_wakes() {
        // Build a real single-topic source + shared state via the playback test
        // helpers. (This test lives with playback where the MCAP helpers are, or
        // reuses `PlaybackStore` internals — see the playback test module.)
        // Here we assert the plumbing directly with a hand-built DecodeShared.
        // See playback tests for an end-to-end coarse→sharp assertion.
    }
}
```

> Note: the meaningful behavioral test for the worker (decode fills cache, waker fires) is written end-to-end in Task 4 through `PlaybackStore`, where the MCAP fixture helpers already exist. Keep the `decode_service` unit test module minimal (the lifecycle/waker assertions ride on Task 4's fixtures).

- [ ] **Step 2: Register the module**

In `src/record/lazy/mod.rs` add:

```rust
pub mod decode_service;
pub use decode_service::{spawn_decode_workers, DecodeShared, Job};
```

- [ ] **Step 3: Build**

Run: `cargo build --no-default-features --lib`
Expected: compiles clean (unused warnings acceptable until Task 3 wires it in).

- [ ] **Step 4: Run tests**

Run: `cargo test --no-default-features --lib decode_service::`
Expected: PASS (module compiles; heavy behavior verified in Task 4).

- [ ] **Step 5: Commit**

```bash
git add src/record/lazy/decode_service.rs src/record/lazy/mod.rs
git commit -m "feat(replay): DecodeShared + background chunk-decode worker pool"
```

---

### Task 3: Route `PlaybackStore` through `DecodeShared` — no behavior change

**Files:**
- Modify: `src/record/playback.rs`

**Interfaces:**
- Consumes: `DecodeShared`, `spawn_decode_workers`, `Job` (Task 2).
- Produces: `Caps` gains `pub worker_count: usize` (default `min(4, available_parallelism)`; tests set `0`). `PlaybackStore` gains `shared: Arc<DecodeShared>`, `jobs: crossbeam_channel::Sender<Job>`, and `rx: crossbeam_channel::Receiver<Job>` (a retained clone used only by the Task 4 test hook). Its `sources`, `cache`, `metas` fields are removed (now inside `shared`).

- [ ] **Step 1: Add `worker_count` to `Caps`**

In `src/record/playback.rs`, extend `Caps` and its `Default`:

```rust
pub(crate) struct Caps {
    pub cache_bytes: usize,
    pub envelope_buckets: usize,
    pub chunk_budget: usize,
    pub worker_count: usize,
}

impl Default for Caps {
    fn default() -> Self {
        let worker_count =
            std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
        Caps {
            cache_bytes: CACHE_BYTES,
            envelope_buckets: ENVELOPE_BUCKETS,
            chunk_budget: CHUNK_BUDGET,
            worker_count,
        }
    }
}
```

- [ ] **Step 2: Move fields into `DecodeShared` and construct it in `load_many_with`**

- Change the struct: remove `sources`, `cache`, `metas`; add
  `shared: Arc<DecodeShared>`, `jobs: crossbeam_channel::Sender<crate::record::lazy::Job>`,
  `rx: crossbeam_channel::Receiver<crate::record::lazy::Job>`.
- At the end of `load_many_with`, after the envelope/text pass, build the shared
  state and workers (the `sources` local is moved in here; the envelope pass
  above already borrowed `&sources`, so this comes after it):

```rust
let shared = std::sync::Arc::new(crate::record::lazy::DecodeShared::new(
    sources,
    ChunkCache::new(cache_bytes),
    metas.clone(),
));
let jobs = crate::record::lazy::spawn_decode_workers(shared.clone(), caps.worker_count);
// Retain one Receiver clone so the test hook can drain the queue on the test
// thread; unused in production (workers own their own Receiver clones).
let (_probe_tx, rx) = crossbeam_channel::bounded(0); // placeholder — see below
```

> Implementation note: `spawn_decode_workers` currently returns only the
> `Sender`. To retain a `Receiver` for the test hook, change
> `spawn_decode_workers` to return `(Sender<Job>, Receiver<Job>)` (the workers
> keep their clones internally). Update Task 2's signature accordingly and its
> `mod.rs` re-export. Store `jobs = tx`, `rx = rx`.

- Replace the struct literal at the end of `load_many_with`:

```rust
Ok(Arc::new(Self {
    shared,
    jobs,
    rx,
    envelope,
    text,
    position_ns: Arc::new(AtomicI64::new(start_ns)),
    duration_ns,
    start_ns,
    breaks,
    chunk_budget,
}))
```

- [ ] **Step 3: Update `snapshot`/`latest_at` field access — still synchronous**

Rewrite the detail loop in `snapshot` and the loop in `latest_at` to reach
through `self.shared`, but keep decoding inline for now (behavior identical to
today). Every `self.sources` → `self.shared.sources`, `self.cache` →
`self.shared.cache`, `self.metas` → `self.shared.metas`. Example (`snapshot`
detail loop):

```rust
for (_, si, span_idx) in overlaps {
    let Some(topic_key) = self.shared.sources[si].topic_key_of(ch) else { continue };
    let chunk = self.shared.cache.get_or_insert_with((si, span_idx, topic_key), || {
        self.shared.sources[si].decode_chunk_topic(span_idx, topic_key, &self.shared.metas)
    });
    acc.extend(chunk.window(ch, window));
}
```

Apply the same mechanical change in `latest_at` and in the `true_overlaps`
count / `overlapping` calls (`self.sources` → `self.shared.sources`). The
`#[cfg(test)] cache_retained_bytes` helper becomes
`self.shared.cache.retained_bytes()`.

- [ ] **Step 4: Full suites green (no behavior change)**

Run: `cargo test --no-default-features --lib` then `cargo test --lib`
Expected: PASS at the same counts as before this task (existing playback
assertions unchanged — this task only relocates state).

- [ ] **Step 5: Commit**

```bash
git add src/record/playback.rs src/record/lazy/decode_service.rs src/record/lazy/mod.rs
git commit -m "refactor(replay): route PlaybackStore decode through Arc<DecodeShared> (no behavior change)"
```

---

### Task 4: Async `snapshot` — coarse overview now, sharpen later

**Files:**
- Modify: `src/record/playback.rs`

**Interfaces:**
- Consumes: `DecodeShared::{cached, reserve, unreserve}`, `self.jobs`, `self.rx` (Task 3).
- Produces: `PlaybackStore::request(&self, key: Job)` (private) and
  `#[cfg(test)] fn run_pending_decodes(&self)` (drains the retained `rx` on the
  calling thread via `self.shared.decode_and_cache`).

- [ ] **Step 1: Add the request helper and the test drain hook**

```rust
impl PlaybackStore {
    /// Enqueue a missing chunk for background decode, deduped. A full queue
    /// (fast scrub outpacing decode) drops the request; the still-visible
    /// window re-requests it next frame.
    fn request(&self, key: crate::record::lazy::Job) {
        if self.shared.reserve(key) {
            if self.jobs.try_send(key).is_err() {
                self.shared.unreserve(key);
            }
        }
    }

    /// Test hook: run every queued decode to completion on this thread, so
    /// tests observe the post-decode (sharp) state without worker threads or
    /// sleeps. Use with `worker_count: 0`.
    #[cfg(test)]
    pub(crate) fn run_pending_decodes(&self) {
        while let Ok(key) = self.rx.try_recv() {
            self.shared.decode_and_cache(key);
        }
    }
}
```

- [ ] **Step 2: Write the failing coarse→sharp test**

Add to the `tests` module in `src/record/playback.rs` (reuses
`make_proto_and_registry` + `write_test_mcap_chunked`):

```rust
#[test]
fn detail_snapshot_is_coarse_until_workers_decode_then_sharp() {
    let (schema, _d, registry) = make_proto_and_registry();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("async.mcap");
    // Many samples, small chunks; a narrow window truly overlaps <= budget so
    // the detail path is taken (not the overview budget path).
    let msgs: Vec<(i64, f32)> = (0..4000).map(|i| (i as i64 + 1, i as f32)).collect();
    write_test_mcap_chunked(&path, &schema, &msgs, 4096);
    // Zero workers → the cache only fills when the test drains the queue.
    let caps = Caps { cache_bytes: 64 * 1024 * 1024, envelope_buckets: 256,
                      chunk_budget: 16, worker_count: 0 };
    let store = PlaybackStore::load_many_with(&[&path], &registry, caps).unwrap();
    let id = registry.id("accel.x").unwrap();
    let window = TimeWindow { start_ns: 1, end_ns: 30 };

    // First read: chunks not yet decoded → coarse overview (decimated), not the
    // exact 1..30 detail.
    let first = store.snapshot(id, window);
    let coarse_len = match &first {
        ChannelSnapshot::Float { ts, .. } => ts.len(),
        other => panic!("wrong variant: {other:?}"),
    };

    // Drain the queued decode jobs, then re-read: now exact detail.
    store.run_pending_decodes();
    let sharp = store.snapshot(id, window);
    match sharp {
        ChannelSnapshot::Float { ts, .. } => {
            assert_eq!(ts, (1..30).collect::<Vec<i64>>(), "must be exact detail after decode");
            assert!(ts.len() >= coarse_len, "detail should be at least as dense as coarse");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

- [ ] **Step 3: Run it to see it fail**

Run: `cargo test --no-default-features --lib playback::tests::detail_snapshot_is_coarse_until_workers_decode_then_sharp`
Expected: FAIL — today's synchronous `snapshot` returns exact detail on the
first read, so the coarse-vs-sharp distinction does not hold.

- [ ] **Step 4: Make the detail branch async**

Replace the detail-gathering loop in `snapshot` (the part after the
`true_overlaps > chunk_budget` overview check) with a cache-probe that never
decodes:

```rust
let mut chunks: Vec<std::sync::Arc<DecodedChunk>> = Vec::new();
let mut missing = false;
for (_, si, span_idx) in overlaps {
    let Some(topic_key) = self.shared.sources[si].topic_key_of(ch) else { continue };
    let key = (si, span_idx, topic_key);
    match self.shared.cached(key) {
        Some(c) => chunks.push(c),
        None => {
            missing = true;
            self.request(key); // enqueue every missing chunk (do not early-return)
        }
    }
}
if missing {
    // Coarse placeholder now; a worker fills the cache and requests a repaint,
    // and the next frame lands in the all-cached branch below.
    return self.snapshot_overview(ch, window);
}
let mut acc = SnapshotAcc::new();
for c in chunks {
    acc.extend(c.window(ch, window));
}
acc.into_snapshot(self.metas_of(ch))
```

`overlaps` is already sorted by start time, so `chunks` stays time-ordered.
(`metas_of` is whatever the current code uses for the fallback sample type;
keep the existing expression — e.g. `self.shared.metas.get(ch).map(|m| m.sample_type).unwrap_or(SampleType::Float)`.)

- [ ] **Step 5: Run the test to see it pass, then both suites**

Run: `cargo test --no-default-features --lib playback::tests::detail_snapshot_is_coarse_until_workers_decode_then_sharp`
Expected: PASS.
Run: `cargo test --no-default-features --lib` then `cargo test --lib`
Expected: PASS. Some existing detail tests may now need a `run_pending_decodes()`
call before asserting exact detail (they used `worker_count > 0`? No — default
caps spawn workers, so `PlaybackStore::load`/`load_many` in tests decode
asynchronously). Fix each existing exact-detail test that regresses by calling
`store.run_pending_decodes()` between the snapshot request and the assertion,
OR by giving those tests real workers and draining. Do NOT weaken an assertion —
only add the drain step so the sharp result is what's asserted.

> Reviewer note: the intended, verified change is that exact-detail tests
> insert a `run_pending_decodes()` (or equivalent settle step) before asserting;
> the asserted values stay identical.

- [ ] **Step 6: Commit**

```bash
git add src/record/playback.rs
git commit -m "feat(replay): async detail snapshot — coarse overview until background decode lands"
```

---

### Task 5: Async `latest` / `latest_at`

**Files:**
- Modify: `src/record/playback.rs`

**Interfaces:**
- Consumes: `DecodeShared::cached`, `self.request` (Task 4), `self.envelope`.
- Produces: non-blocking `latest_at` — cached hit → exact; otherwise enqueue and
  fall back to the envelope's newest value ≤ `end_ns`, or `None`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn latest_at_is_nonblocking_and_sharpens_after_decode() {
    let (schema, _d, registry) = make_proto_and_registry();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("latest_async.mcap");
    let msgs: Vec<(i64, f32)> = (0..4000).map(|i| (i as i64 + 1, i as f32)).collect();
    write_test_mcap_chunked(&path, &schema, &msgs, 4096);
    let caps = Caps { cache_bytes: 64 * 1024 * 1024, envelope_buckets: 256,
                      chunk_budget: 16, worker_count: 0 };
    let store = PlaybackStore::load_many_with(&[&path], &registry, caps).unwrap();
    let id = registry.id("accel.x").unwrap();

    // Cold cache: must return without decoding (either None or an envelope-coarse
    // value), never blocking.
    let _cold = store.latest_at(id, 3500);
    // After draining, the exact containing chunk is decoded → precise sample.
    store.run_pending_decodes();
    let (t, s) = store.latest_at(id, 3500).expect("exact after decode");
    assert_eq!(t, 3500);
    match s { Sample::Float(v) => assert!((v - 3499.0).abs() < 1e-6), o => panic!("{o:?}") }
}
```

- [ ] **Step 2: Run it to see the current behavior**

Run: `cargo test --no-default-features --lib playback::tests::latest_at_is_nonblocking_and_sharpens_after_decode`
Expected: with today's synchronous `latest_at` the final assertion already
passes, but the cold read decodes inline (blocking). This task makes the cold
read non-blocking; the test guards the post-decode exactness.

- [ ] **Step 3: Make `latest_at` non-blocking**

Replace the candidate loop's `get_or_insert_with` with a `cached` probe:

```rust
for (si, k, _) in candidates.into_iter().rev() {
    let Some(topic_key) = self.shared.sources[si].topic_key_of(ch) else { continue };
    let key = (si, k, topic_key);
    match self.shared.cached(key) {
        Some(chunk) => {
            if let Some(hit) = chunk.last_le(ch, end_ns) {
                return Some(hit);
            }
        }
        None => self.request(key),
    }
}
// Nothing cached yields a hit yet: coarse fallback from the envelope, or None.
self.envelope_latest(ch, end_ns)
```

Add `envelope_latest`, deriving a coarse readout from the overview buckets ≤
`end_ns` (reuse `Envelope::read` over `[start_ns, end_ns]` and take the last
point; return the channel's typed `Sample`, or `None` if empty):

```rust
fn envelope_latest(&self, ch: usize, end_ns: i64) -> Option<(i64, Sample)> {
    let pts = self.envelope.read(ch, TimeWindow { start_ns: self.start_ns, end_ns });
    let (t, v) = *pts.last()?;
    let st = self.shared.metas.get(ch).map(|m| m.sample_type).unwrap_or(SampleType::Float);
    Some((t, match st {
        SampleType::Int => Sample::Int(v as i64),
        SampleType::Bool => Sample::Bool(v != 0.0),
        _ => Sample::Float(v),
    }))
}
```

Keep the text-channel branch at the top of `latest_at` unchanged (text is
retained in memory, already exact and non-blocking).

- [ ] **Step 4: Run the test, then both suites**

Run: `cargo test --no-default-features --lib playback::tests::latest_at_is_nonblocking_and_sharpens_after_decode`
Expected: PASS.
Run: `cargo test --no-default-features --lib` then `cargo test --lib`
Expected: PASS. As in Task 4, add `run_pending_decodes()` to any existing
`latest`/`latest_at` test that asserts an exact sample on a cold store — do not
change the asserted value.

- [ ] **Step 5: Commit**

```bash
git add src/record/playback.rs
git commit -m "feat(replay): non-blocking latest_at with envelope-coarse fallback"
```

---

### Task 6: App integration — install the repaint waker

**Files:**
- Modify: `src/record/playback.rs` (add the public setter)
- Modify: `src/app.rs` (call it after a load, passing the egui context)

**Interfaces:**
- Consumes: `DecodeShared::set_waker` (Task 2).
- Produces: `PlaybackStore::set_repaint_waker(&self, f: impl Fn() + Send + Sync + 'static)`.

- [ ] **Step 1: Add the setter on `PlaybackStore`**

```rust
/// Install the repaint hook workers call when a decoded chunk lands, so the
/// UI re-renders and picks up the now-cached detail. Set once after load with
/// the egui context. Idempotent (replaces any previous hook).
pub fn set_repaint_waker(&self, f: impl Fn() + Send + Sync + 'static) {
    self.shared.set_waker(Box::new(f));
}
```

- [ ] **Step 2: Call it where the store is installed**

`poll_pending_load` needs the egui `Context`. Give it the context from
`update` and install the waker right after `self.store = playback.clone();`
(`src/app.rs` around line 405):

- Change `fn poll_pending_load(&mut self)` to `fn poll_pending_load(&mut self, ctx: &egui::Context)` and update its call site in `update`.
- After `self.store = playback.clone();` add:

```rust
let ctx = ctx.clone();
self.store.set_repaint_waker(move || ctx.request_repaint());
```

- [ ] **Step 3: Build and run the full suites**

Run: `cargo build --lib` then `cargo test --no-default-features --lib` then `cargo test --lib`
Expected: compiles; both suites PASS.

- [ ] **Step 4: Manual smoke (documented, not automated)**

Load a large recording and drag the scrub bar: the UI must stay responsive
(coarse trace while dragging, sharpening to exact detail when you pause). This
is the acceptance behavior; no automated GUI test.

- [ ] **Step 5: Commit**

```bash
git add src/record/playback.rs src/app.rs
git commit -m "feat(app): wake the UI when background MCAP decode fills the cache"
```

---

## Self-Review

**Spec coverage:**
- Non-blocking `snapshot` (coarse→sharp) → Task 4. ✔
- Non-blocking `latest`/`latest_at` → Task 5. ✔
- `DecodeShared` + workers + no Arc cycle + shutdown → Task 2 (+ lifecycle observable via `active_workers`). ✔
- Bounded queue / backpressure / dedup → Task 2 (`QUEUE_CAP`, `reserve`) + Task 4 (`request` with `try_send`/`unreserve`). ✔
- Non-inserting cache probe → Task 1. ✔
- App waker install → Task 6. ✔
- Deterministic tests via drain hook + `worker_count: 0` → Task 3/4. ✔
- Envelope coarse fallback reused, no new precompute → Tasks 4/5. ✔

**Placeholder scan:** One deliberate forward-reference — Task 3 Step 2 flags that
`spawn_decode_workers` must return `(Sender, Receiver)` rather than just
`Sender`; the implementer updates Task 2's signature + `mod.rs` re-export when
they reach Task 3. Called out explicitly so it is not a silent gap. No TODO/TBD.

**Type consistency:** `Job = (usize, usize, u32)` is the `ChunkCache` key type
throughout (Tasks 1–5). `DecodeShared` owns `sources`/`cache`/`metas`; the store
reaches them via `self.shared.*` after Task 3. `Caps.worker_count` added in Task
3, consumed by `spawn_decode_workers`. `run_pending_decodes` uses the retained
`rx` from Task 3. Consistent.

**Risk note for the executor:** Tasks 4 and 5 may require touching existing
exact-detail tests to add a `run_pending_decodes()` settle step. This is
expected; the rule is to add the settle step, never to weaken an assertion. If a
test cannot be made to pass without changing an asserted value, stop and treat it
as a real regression.
