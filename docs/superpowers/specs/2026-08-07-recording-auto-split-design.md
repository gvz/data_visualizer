# Size-Based Recording Auto-Split — Design

**Status:** approved (2026-08-07)

**Goal:** When a live recording grows past a size limit set in `config.toml`,
the recorder automatically finalizes the current `.mcap` file and continues
into a new one, so no single recording file grows without bound. Playback
stitches the parts back into one timeline (already supported).

## Problem

The recorder writes one `.mcap` file per session
(`recording_{secs}.mcap`) with no upper bound; a long session produces a
single enormous file. The user wants a configurable size cap that splits a
session across several files. Playback already stitches multiple files onto
one timeline order-independently (`PlaybackStore::load_many`), and each
finalized file carries its own summary + chunk index (needed by the lazy,
memory-mapped loader), so split parts replay seamlessly.

## Decisions (locked)

- **Split by on-disk file size.** Roll over when the actual `.mcap` on disk
  reaches the configured limit (post-compression). Matches the user's
  "file bigger than X" intuition; measured with `fs::metadata(path).len()`
  right after the recorder's existing 1-second flush.
- **Sequential part suffix.** Parts of one session share its start timestamp
  and are numbered: `recording_{secs}_000.mcap`, `_001`, `_002`, …
  The suffix is used **only when a size limit is configured**; with no limit
  the filename stays `recording_{secs}.mcap` (today's behavior, unchanged).
- **Config lives in a new `[recording]` section**, parsed by a small
  `RecordingConfig` in the same style as the other independent config
  parsers (`BridgeConfig`, `ScriptsConfig`) that each read `config.toml`.
- **Rollover is approximate.** It fires at the first flush past the limit, so
  a part may overshoot the cap by up to roughly one chunk plus one second of
  data. Acceptable for "keep files under ~X".
- **Playback is untouched.** The user multi-selects the parts in the open
  dialog. Auto-expanding a session's sibling parts from a single pick is a
  documented future nicety, out of scope here.

## Configuration

New section in `config.toml`:

```toml
[recording]
max_file_mb = 512   # roll over when the .mcap on disk reaches this many MB.
                    # Omit the section, or set 0, to keep a single file.
```

```rust
pub struct RecordingConfig {
    pub max_file_mb: Option<u64>, // None or Some(0) => no split
}
impl RecordingConfig {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self>;
}
```

Loaded in `main.rs` alongside the other parsers and handed to the app, which
converts it to `max_bytes: Option<u64>` (`mb * 1024 * 1024`, `0 => None`) and
passes it into `start_recording`.

## Architecture

Only the recorder changes; the read path does not.

```
app.rs::start_recording
  └── record::start_recording(output_dir, registry, schema, rx, max_bytes)
        └── writer::spawn_recorder(..., max_bytes)
              └── recorder thread
                    ├── RecorderCfg { output_dir, session_secs, schema_bytes,
                    │                  zmq_topics, max_bytes }
                    ├── McapRecorder (one open part)  ── rolls over ──┐
                    └────────────────────────────────────────────────┘
```

### `RecorderCfg` (new)

Holds everything needed to open a fresh part without borrowing the registry
into the thread:

```rust
struct RecorderCfg {
    output_dir: PathBuf,
    session_secs: u64,
    schema_bytes: Arc<[u8]>,   // shared ZMQ/Proto schema
    zmq_topics: Vec<String>,   // distinct topics to pre-seed per part
    max_bytes: Option<u64>,    // None => never rotate, single un-suffixed file
}
```

`zmq_topics` is precomputed once from the registry in `spawn_recorder`
(the current `McapRecorder::new` loop that reads `registry.config(id).topic`),
so the thread never needs the registry.

### `McapRecorder` (reworked)

- `fn open(cfg: &RecorderCfg, part: u32) -> anyhow::Result<Self>` — builds the
  path (`recording_{secs}_{part:03}.mcap` when `max_bytes.is_some()`, else
  `recording_{secs}.mcap`), creates the writer, pre-seeds the ZMQ channels
  from `cfg.zmq_topics` with the shared schema, and starts with an empty
  `dynamic_channel_ids` map. Keeps `path` for size checks.
- The existing per-write flush block gains a size check: after
  `writer.flush()`, if `cfg.max_bytes` is `Some(limit)` and
  `fs::metadata(&self.path).len() >= limit`, signal a rollover to the loop.
- Dynamic (MQTT) channels re-register on first sight in each new part (each
  `.mcap` is self-contained), so the reset map is correct.

Rollover in the recorder loop: `recorder.finish()` the current part (writes
summary + chunk index), `part += 1`, `recorder = McapRecorder::open(&cfg,
part)`. The stop path finishes the final part exactly as today.

## Data flow (record)

1. `spawn_recorder` builds `RecorderCfg` (precomputes `zmq_topics`), opens
   part 0, spawns the thread.
2. Each queued frame is written as today. After a flush, the size check may
   flag a rollover.
3. On rollover: finish current part → open next part → continue writing the
   same frame stream into it.
4. On stop / channel close: drain remaining frames, finish the last part.

## Error handling

- `metadata()` error on the size check → skip the check this time, keep
  recording (do not abort a session over a transient stat failure).
- `finish()` / `open()` error during rollover → propagate out of the loop,
  setting the existing `record_failed` flag, exactly like a write error today.
- No config / `max_file_mb` absent or `0` → `max_bytes = None`: one
  un-suffixed file, byte-for-byte today's behavior.

## What does not change

- Message write path (`write_msg`, `write_dynamic`, `write_to_channel`), the
  1-second flush cadence, sequence numbering, and the queue.
- Playback: `PlaybackStore::load_many` stitching, the mmap/lazy loader, and
  every viz panel.
- The three existing `writer.rs` tests (they call `start_recording` with no
  limit → single un-suffixed file) stay green.

## Testing

Reuse the `writer.rs` test harness (`start_recording` + `record_channel`).

- **Rotation by size:** `spawn_recorder` (via `start_recording`) with a tiny
  `max_bytes`; feed enough messages to exceed it several times; assert
  multiple `recording_{secs}_NNN.mcap` files exist, numbered from `000`.
- **Each part is finalized:** every produced file reads back through
  `mcap::Summary::read` with a non-empty chunk index (self-contained).
- **Stitched round-trip:** `PlaybackStore::load_many` over all parts returns
  the full, timestamp-ordered message set — no gaps, no dupes.
- **No-limit unchanged:** with `max_bytes = None`, exactly one
  `recording_{secs}.mcap` is written (existing tests cover this).
- **Config parse:** `RecordingConfig::from_toml_str` reads `max_file_mb`,
  and treats a missing section and `0` as "no split".

## Future work (out of scope)

- Auto-expanding a session's sibling parts when the user opens one part.
- Time-based or message-count rotation in addition to size.
- Configurable output directory (currently `.`).
