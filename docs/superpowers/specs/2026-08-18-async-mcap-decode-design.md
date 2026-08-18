# Async MCAP Decode — Design

**Date:** 2026-08-18
**Status:** Approved design, pending implementation plan
**Scope:** `src/record/playback.rs`, `src/record/lazy/*`, one app integration point

## Problem

Scrubbing a large MCAP recording freezes the UI even with only 1–4 panels
on screen. Root cause is architectural, not a slow function:

`PlaybackStore::snapshot` (and `latest`/`latest_at`) decode chunks
**synchronously on the render thread**. egui is immediate-mode — the frame
cannot present until every visible panel's `render` returns. When the user
drags the scrub bar, the detail window jumps to fresh chunks every frame →
full cache miss → `decompress + decode` runs inline → the whole frame
blocks. For a fat foreign channel a single miss is tens to hundreds of ms;
a few panels each missing per frame stalls the app.

Incremental speedups to the decode itself (summary caching, the flatten
fast path) reduced per-decode cost but cannot remove the freeze: any
blocking decode on the paint thread stalls the frame. The fix must change
the model.

## Principle

**The render thread must never wait for decode or I/O.** Every frame
returns immediately with the best data already in memory; missing detail is
produced off-thread and shown on a later frame.

## Chosen approach: async detail decode (Approach A)

Keep the existing decimated envelope overview as the instant coarse
fallback (already built at load). Move detail-chunk decoding to a
background worker pool. `snapshot` returns immediately: exact detail when
every needed chunk is already cached, otherwise the coarse overview for the
window plus an enqueued request for the missing chunks. When a worker fills
the cache it requests a repaint; the next frame serves exact detail.

Rejected alternatives:
- **B (multi-resolution mip pyramid):** precompute several decimated levels
  so scrubbing reads tiles with no decode. Better coarse fidelity at every
  zoom, but more load time + memory, and the deepest raw zoom still needs
  A. Deferred — a natural follow-on once A lands.
- **C (harden the sync path):** decompress-once-decode-many, right-size the
  decode buffer, prefetch neighbours. Real speedups, but they still block
  the paint thread on a cold scrub. These fold in later as pure throughput
  wins, not freeze fixes.

## Architecture

### `DecodeShared` (new, behind `Arc`)

Holds everything a decode needs, all read-only after load, so decoding is
safe from many threads:

- `sources: Vec<RecordingSource>` — mmap + summary + bindings (immutable).
- `cache: ChunkCache` — the existing byte-budgeted LRU (already
  `Mutex`-guarded; safe for concurrent worker writes + render reads).
- `metas: Vec<ChannelMeta>`.
- `in_flight: Mutex<HashSet<(usize, usize, u32)>>` — keys queued or being
  decoded, so a key is enqueued at most once.
- `waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>` — set by the app to
  `move || ctx.request_repaint()`. `None` in headless/tests (no-op).

### `PlaybackStore` (modified)

- Loses `sources`, `cache`, `metas` as direct fields; gains
  `shared: Arc<DecodeShared>`.
- Keeps `envelope`, `text`, `breaks`, `position_ns`, `duration_ns`,
  `start_ns`, `chunk_budget`.
- Gains `jobs: SyncSender<Job>` — a **bounded** channel to the workers.

`Job = (usize, usize, u32)` — the `(source, chunk, topic)` cache key.

### Worker pool

At load, spawn `min(4, available_parallelism)` threads. Each loops:

```
for key in rx {                    // blocks until a job or channel closes
    shared.decode_and_cache(key);  // decode_chunk_topic → cache.get_or_insert_with
    shared.in_flight.remove(key);
    shared.wake();                 // request_repaint if a waker is set
}
```

Workers hold `Arc<DecodeShared>` + the `Receiver`. They do **not** hold the
`PlaybackStore` or the `Sender`, so there is no `Arc` cycle.

### Lifecycle

The store owns the `Sender`. Dropping the store drops the `Sender`, closes
the channel, the worker loops end, workers exit, and the `Arc<DecodeShared>`
refcount falls to zero. No leak, no explicit shutdown flag.

### Backpressure

The job channel is bounded (e.g. capacity 64). `snapshot` uses
`try_send`: if the queue is full, the missing key is simply not enqueued
this frame (the frame stays coarse) and is retried next frame while still
visible. Fast scrubbing that outpaces decode drops stale work naturally and
caps queue memory.

## Data flow — `snapshot(channel, window)`

1. **Text channel** → in-memory slice from `self.text`. Instant. Unchanged.
2. **`true_overlaps > chunk_budget`** → `snapshot_overview` (envelope).
   Instant. Unchanged.
3. **Detail branch:**
   - Collect overlapping `(src, chunk, topic)` keys (existing logic).
   - Probe the cache for each key (a read-only `get` that does not decode).
   - **All present** → build exact detail exactly as today.
   - **Any missing** → for each missing key: if not already in `in_flight`,
     insert it and `try_send`. Return `snapshot_overview(ch, window)` as the
     placeholder for this frame.

When the last missing chunk lands in the cache, the worker's repaint causes
a frame where all keys are present → exact detail. Result: coarse while
scrubbing, sharp when settled. Placeholder decision (confirmed): **coarse
overview, then sharpen.**

### Cache probe without decode

`ChunkCache` needs a non-inserting lookup, e.g.
`fn get(&self, key) -> Option<Arc<DecodedChunk>>` that returns the entry
(touching LRU recency) or `None`. `snapshot` uses `get`; workers use the
existing `get_or_insert_with` to decode-and-store.

## `latest` / `latest_at`

Same rule — never block. Walk candidate spans newest-first as today, but
use the non-inserting cache `get`:

- First cached chunk holding a sample ≤ `end_ns` → return it.
- For a candidate whose chunk is **not** cached → enqueue it (dedup via
  `in_flight`, `try_send`) and continue scanning cached candidates.
- If nothing cached yields a hit → return the coarse fallback: the
  envelope's latest value ≤ `end_ns` for the channel, or `None` if the
  envelope has nothing. The exact readout appears once the worker fills the
  chunk and a repaint occurs.

## App integration

One new call: after the store is loaded and the app has an `egui::Context`
(available in `update`), install the waker:

```rust
store.set_repaint_waker({
    let ctx = ctx.clone();
    move || ctx.request_repaint()
});
```

Idempotent; setting it again replaces the closure. Until it is set, workers
skip the wake (only relevant before the first frame).

## Concurrency safety

- `RecordingSource` is read-only after `open` (mmap, parsed summary,
  bindings) → `Sync`; the load pass already decodes across threads with
  shared `&sources`, so concurrent detail decode is the same guarantee.
- `ChunkCache` is `Mutex`-guarded; concurrent worker inserts and render
  reads are already safe.
- `in_flight` and `waker` are `Mutex`-guarded.

## Testing

- **Existing determinism** holds: when all chunks are cached the synchronous
  detail path is byte-identical to today. Existing playback tests exercise
  this (small files decode fully into cache on first read).
- **Coarse-then-sharp:** with a store whose cache starts empty, a detail
  `snapshot` on a big-enough file returns the overview variant/shape; after
  draining the worker queue to completion (test hook) and re-snapshotting,
  it returns exact detail equal to the synchronous result.
- **Dedup:** two `snapshot`s of the same missing window enqueue the key
  once (assert `in_flight`/queue depth).
- **Backpressure:** filling the bounded queue makes further `try_send`
  no-ops without blocking or panicking.
- **Lifecycle:** dropping the store terminates workers (join returns).
- **Waker:** a fake waker counts repaint requests; asserts it fires after a
  worker completes.

Tests must be deterministic — expose a test hook to run the worker queue to
quiescence synchronously (e.g. a `drain_decode_queue_for_test`) rather than
sleeping.

## Known limitations (acceptable for A; addressed by B later)

- The coarse fallback is the single-level envelope. At a very tight zoom the
  window may cover only a couple of envelope buckets, so the placeholder is
  quite coarse until detail arrives. A multi-resolution pyramid (Approach B)
  would make the intermediate-zoom placeholder finer; deferred.
- Brief coarse→sharp popping when scrubbing stops. Intended trade-off.

## Out of scope

- Multi-resolution pyramid (B).
- Decompress-once-decode-many and buffer right-sizing (C) — separate
  throughput work, folds in independently.
- Prefetching neighbour chunks in the scroll direction — a later refinement
  on top of this queue.
