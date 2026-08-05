# Larger-Than-RAM MCAP Playback — Design

**Status:** approved (2026-08-05)

**Goal:** Play back MCAP recordings whose decoded (and even compressed) size
exceeds available RAM, by memory-mapping the file, decoding chunks on demand
behind the existing window-scoped `ChannelStore` interface, and serving a
precomputed decimated envelope for zoomed-out overviews.

## Problem

`PlaybackStore` today fully materialises every recording: `std::fs::read`
pulls the whole compressed file into RAM, then every sample is decoded and
retained in per-channel `Vec`s (`Float { ts, vals }`, …). Resident memory is
therefore `O(file size)` twice over (compressed bytes + decoded samples), so a
recording larger than RAM cannot be opened.

Two facts make a lazy design cheap to slot in:

- **The read interface is already window-scoped.** `ChannelStore::snapshot`
  takes a `TimeWindow`, and every panel already requests a bounded viewport
  (`waveform.rs` fetches `[t0, end_ns]`, `win_s` seconds wide, around the
  playback clock — not the whole file).
- **The waveform already decimates** (`decimate_minmax(.., MAX_PLOT_BUCKETS)`)
  *after* fetching the window, so an overview tier only has to intervene when
  the requested window is wide enough to cover most of the file.

## Decisions (locked)

- **Always lazy, one path.** Replace eager full retention entirely. Every load
  mmaps, builds an envelope, and decodes chunks on demand. No hybrid
  small/large split — one implementation to reason about.
- **Add `memmap2`.** Memory-map each file so the OS pages compressed bytes in
  on demand; even the compressed file need not fit in RAM. One new dependency,
  accepted deliberately (the parallel-decode work stayed std-only; this cannot).
- **Single-level envelope, no LOD pyramid.** Accept blocky fidelity in the
  narrow band that is both too wide for detail-decode and thin on envelope
  buckets. A pyramid is a documented future upgrade.
- **Text/log channels retained fully in RAM at load.** Logs are low-volume, so
  this is cheap; it is the one component that stays `O(file)` in memory.
  Documented v1 limitation; future work spills them to on-demand decode.
- **No separate state-channel component.** Replay has no coded-text: a
  `text_coded` channel loads as plain `Text`, and `state_graph.rs` already
  interns `Text`/`Int`/`Bool` snapshots itself. So numeric channels
  (`Float`/`Int`/`Bool`) all use the envelope, and text stays exact.
  Consequence: a state graph of a *high-rate* `Int` channel at near-whole-file
  zoom shows approximate bands; low-rate state channels touch few chunks and
  stay in the exact detail path, so are unaffected. Documented limitation
  alongside blocky mid-zoom.

## Architecture

New module `src/record/lazy/` holds the components. The public store type keeps
its current name `PlaybackStore` (in `src/record/playback.rs`) so `app.rs` —
which holds `Arc<PlaybackStore>` and reads its `position_ns` / `start_ns` /
`duration_ns` fields directly — needs no edits. Its internals are replaced;
`load`/`load_many` signatures and the `ChannelStore` impl are preserved.

```
PlaybackStore  (src/record/playback.rs)
├── sources:   Vec<RecordingSource>   // one per stitched file
├── envelope:  Envelope               // numeric channels, decimated min/max
├── text:      TextRetention          // text/log channels, retained in full
├── cache:     ChunkCache             // byte-budgeted LRU of decoded chunks
├── metas:     Vec<ChannelMeta>
├── position_ns / start_ns / duration_ns / breaks   // as today
```

### Components

Each unit has one job and a narrow interface, so it can be tested alone.

**`RecordingSource`** — one memory-mapped file.
- Owns `mmap: memmap2::Mmap` (derefs to `&[u8]`; `mcap::Summary::stream_chunk`
  takes `&[u8]`), the `mcap::Summary`, and a chunk index
  `spans: Vec<ChunkSpan>` sorted by `start_ns`.
- `struct ChunkSpan { start_ns: i64, end_ns: i64, idx: usize }` — `idx` indexes
  `summary.chunk_indexes`.
- `overlapping(window) -> &[ChunkSpan]` — binary-search the sorted spans for
  those intersecting `[window.start_ns, window.end_ns)`.
- `decode_chunk(idx) -> DecodedChunk` — `summary.stream_chunk` the one chunk,
  run each message through the existing `decode_message` routing into a scratch
  buffer.
- Unchunked / unfinalised files (no summary): fall back to one `ChunkSpan`
  covering the whole file decoded via `MessageStream` (same fallback shape as
  today's linear scan).

**`DecodedChunk`** — the decoded samples of one chunk, all channels it touched,
as per-channel typed `Vec<(i64, V)>`. Produced by `decode_chunk`, cached by
value behind `Arc`.

**`ChunkCache`** — LRU keyed by `(file: usize, chunk: usize)` →
`Arc<DecodedChunk>`, evicting oldest once total retained bytes exceed
`cache_bytes` (default 512 MB). A decoded chunk's byte cost is estimated from
its sample counts. Bounds worst-case resident RAM independent of file size.

**`Envelope`** — numeric (`Float`/`Int`/`Bool`) channels only. Per channel, `B`
fixed-width min/max buckets spanning `[start_ns, start_ns + duration_ns]`:

```
struct Bucket { t_min: i64, v_min: f64, t_max: i64, v_max: f64, any: bool }
```

`B` default 16384 (≈ 384 KB/channel). Bucket for a sample at `ts` is
`((ts - start_ns) * B / (duration_ns + 1))`, clamped to `0..B`. Built once at
load; `any=false` buckets (no sample) are skipped when read.

**`TextRetention`** — text/log channels retained fully: `Vec<(i64, String)>`
per channel (today's `PlaybackChannel::Text`). `O(file)` memory for these
channels only; accepted v1 limitation.

## Data flow

### Load (`PlaybackStore::load_many(paths, registry)`)

1. For each path: `mmap` the file; read `Summary`; if present, build `spans`
   from `chunk_indexes` (each `ChunkIndex` carries `message_start_time` /
   `message_end_time`) and `(min,max)` bounds from `summary.stats`
   (`message_start_time`/`message_end_time`) — no decode. Files without a
   summary get bounds/spans from a message scan (existing `time_bounds`
   fallback).
2. Register channels for every file first (unchanged `plan_file` +
   `add_dynamic`), so the store's slot set covers all files before it is built.
3. Compute `start_ns` / `duration_ns` / `breaks` from the per-file bounds —
   identical order-independent logic to today (`mins.sort(); skip(1)`), no
   decode required.
4. **One parallel pass** over all chunks of all files (reuse today's
   `thread::scope` per-chunk split): each worker decodes its chunk range into a
   thread-local partial `Envelope` + partial text, **retaining no raw numeric
   samples**. Merge partials: envelope buckets by min/max, text by time-ordered
   merge. Load stays `O(file)` time, drops to `O(buckets + text)` memory.

### Read `snapshot(channel, window)`

Numeric channel:
1. Gather `overlapping(window)` spans across all sources; sum their chunk count.
2. **Detail path** (sum ≤ `chunk_budget`, default 16): for each overlapping
   chunk, fetch/decode via `ChunkCache`, take this channel's samples, merge
   across chunks/files, `partition_point`-slice to the window → exact raw
   `ChannelSnapshot` (same bytes today's store returns).
3. **Overview path** (sum > `chunk_budget`): read this channel's envelope
   buckets whose time range intersects the window; emit, per non-empty bucket,
   its min and max points in timestamp order → `ChannelSnapshot` of decimated
   min/max. `decimate_minmax` in the waveform then further reduces to
   `MAX_PLOT_BUCKETS` for drawing.

Text channel: `partition_point`-slice the retained `Vec` (as today, exact at
all zooms). `Int`/`Bool` shown as a state graph follow the numeric paths above.

### Read `latest_at(channel, end_ns)` / `latest`

Numeric: locate the source+chunk whose span contains `end_ns` (or the last span
ending ≤ `end_ns`), decode it via the cache, return the last sample with
`ts ≤ end_ns`. Exact, no full retention. Text: `partition_point` the retained
`Vec`. `latest()` uses `position_ns` as `end_ns`, matching today's behaviour.

## Memory model

Resident RAM = envelope (`B × numeric channels`) + retained text
+ `ChunkCache` cap + OS-paged mmap working set. **All independent of total file
size** except retained text (documented limitation). Tunable knobs:
`cache_bytes` (512 MB), `envelope_buckets` (16384), `chunk_budget` (16).

## Error handling

- Missing/short summary → per-file message-scan fallback for bounds and a
  single whole-file span (no chunk-level laziness for that file, but it still
  loads).
- Decode errors inside a chunk → same as today: `decode_batch` logs and skips
  the bad message; the rest of the chunk still decodes.
- mmap failure → propagate `anyhow::Error` with the file path context, exactly
  like today's `fs::read` error.
- Type conflicts on reopen (a topic re-registered with a different type) →
  unchanged `plan_file` behaviour: skip, do not corrupt.

## What does not change

- The `ChannelStore` trait and every viz panel — the interface is already
  window-scoped and decimation-aware.
- Stitching semantics: original timestamps preserved, `break_times` at each
  non-earliest file start, line/state gaps at breaks.
- `app.rs` load wiring beyond the concrete store type returned by
  `load_many`.
- The parallel per-chunk decode structure (`thread::scope`, per-worker
  buffers) — repurposed from "merge raw samples" to "merge partial
  envelope/spans/text".

## Testing

Reuse the existing `write_test_mcap` / `write_mqtt_mcap` harness.

- **Envelope correctness:** known samples → assert per-bucket min/max and their
  timestamps.
- **Detail-path exactness:** narrow window on a small file → snapshot equals the
  eager store's bytes for the same window.
- **Overview-path shape:** wide window over many chunks → snapshot is the
  envelope min/max sequence, monotonic in time.
- **Path switch:** a window straddling `chunk_budget` picks detail below, envelope
  above.
- **`latest_at` chunk selection:** positions in different chunks return the
  correct last-≤ sample without decoding unrelated chunks.
- **Cache bound:** decode `N+1` distinct chunks with a cap holding `N`; assert
  retained bytes ≤ cap and the evicted chunk re-decodes correctly.
- **Stitching/breaks preserved:** the two existing `load_many` tests
  (timestamps merged, break order-independent) pass against the lazy store.

## Future work (out of scope)

- Multi-level LOD pyramid for smooth fidelity at every zoom.
- Lazy (spill-to-decode) text/log channels.
- Prefetch of neighbouring chunks on scrub to hide decode latency.
