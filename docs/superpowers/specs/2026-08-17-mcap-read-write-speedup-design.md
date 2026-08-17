# MCAP Read/Write Speedup Design

## Goal

Make MCAP replay (load + scrub) fast, and stop new recordings from being slow
to read in the first place. Two independent, backward-compatible changes:

- **Read-side direct decode** — replace `prost-reflect` dynamic decoding on the
  read/ingest hot path with a hand-rolled protobuf field scan. Speeds up every
  existing recording and the live ingest path.
- **Record-side batching** — coalesce many single-sample MQTT/script frames
  into one MCAP message per topic, so new recordings carry ~1000× fewer
  messages and read fast even through the reflection path.

Builds on the lazy MCAP playback store (`src/record/lazy/`, `playback.rs`).

## Background: the measured problem

Profiled with `examples/mcap_profile.rs` against real recordings. For
`recording_1785913919.mcap` (156 MiB, 15 channels, 16.4 s, **465 chunks**):

| Phase | Wall | Notes |
|-------|------|-------|
| `PlaybackStore::load` | 1.4 s | parallel envelope pass, ~15 s thread-time |
| Full-timeline scrub (400 reads) | 4.7 s | ~12 ms/read |

The file holds **7.23M MCAP messages for 8.26M samples — ~1 message per
sample.** MQTT ingest and script outputs each write one MCAP message per
sample; only the ZMQ path (`AccelBatch`) is batched. All read cost scales with
message count, and message count == sample count.

Scrub time splits (per full sweep), measured by instrumenting `decode_into` /
`decode_batch`:

| Cost | Time | Share |
|------|------|-------|
| mcap decompress + message-stream iteration | 2.9 s | 62% |
| per-sample field extraction (dynamic `Value` lookups/clones) | 0.95 s | 20% |
| `DynamicMessage::decode` (prost-reflect) | 0.8 s | 17% |
| `Summary::read` | 0.08 s | 2% |

`Summary::read` was the initial suspect (re-parsed per chunk) but is
negligible. The 62% mcap-iteration cost is **only** removable by writing fewer,
bigger messages (Part B). The 37% decode+extract cost is removable on existing
files by a direct decoder (Part A).

## Goals / non-goals

**Goals**
- Cut decode+extract cost on the read/ingest path with a field-number decoder,
  falling back to reflection for shapes it can't represent.
- Batch new MQTT/script recordings so message count drops ~256–1000×.
- Read old (flat, per-sample) and new (batched) files identically — full
  backward compatibility, no migration.
- Preserve exact per-sample timestamps and values through both changes.

**Non-goals**
- Changing the ZMQ/Proto batched path (already batched).
- Changing the on-wire protobuf field layout of a sample (`t_ns=1`, `value=2`).
- A new file format or MCAP version. Batching uses a standard repeated field.
- Resampling, interpolation, or any change to what samples a read returns.

## Part A — Read-side direct decode

### Where it runs

`src/ingest/decode.rs::decode_batch(data, bindings, store)` is the single
decode entry point for both replay (`decode_message` → `decode_batch`) and live
ingest. Today it does, per call:

1. `DynamicMessage::decode(msg_desc.clone(), data)` — allocates a reflection
   tree, one `DynamicMessage` per repeated sample.
2. Per binding, per sample: `get_named_field` → `descriptor().get_field_by_name(name)`
   (a string-keyed lookup **per sample**) then `.into_owned()` (clones the `Value`).

### The decoder

Precompute a decode plan per `ChannelBinding` once (at binding-build time in the
router / reconstructed-binding path), holding field **numbers** and wire types
resolved from the descriptor:

```rust
/// Field-number decode plan for the two supported shapes. `None` (unsupported
/// shape/kind) makes decode_batch fall back to the reflection path.
pub struct FastPlan {
    /// Repeated container field number for the batch shape; None for flat.
    repeated_field: Option<u32>,
    ts_field: u32,
    val_field: u32,
    val_kind: ValKind,   // F64 | F32 | I64 | I32 | U64 | U32 | Bool | String
    sample_type: SampleType,
    eu_scale: f64,
    eu_offset: f64,
    id: ChannelId,
}
```

Decode is a standard protobuf wire scan:

- **Flat** (`repeated_field == None`): scan `data`'s top-level fields; capture
  `ts_field` (varint) and `val_field` (per `val_kind` wire type); on
  end-of-buffer emit one sample.
- **Batch** (`repeated_field == Some(f)`): scan top-level for tag `(f<<3)|2`
  (length-delimited); for each element sub-slice, scan for `ts_field` and
  `val_field`; emit one sample per element. Unknown fields are skipped by wire
  type (varint: read varint; 64-bit: +8; 32-bit: +4; length-delimited: read len,
  skip len).

Value conversion mirrors `extract_numeric` exactly: read the raw scalar per
`val_kind`, apply `raw * eu_scale + eu_offset`, cast per `sample_type`
(Float/Int/Bool), or read the string for Text. Timestamp uses `resolve_ts`
(non-positive → `now_ns()`), unchanged.

### Fallback

`FastPlan::build(binding)` returns `None` when the binding's `val_path` depth is
not 1 or 2, when a field number can't be resolved, or when `val_kind` is a type
the scanner doesn't handle. `decode_batch` then takes today's reflection path
verbatim. This keeps arbitrary ZMQ schemas (nested paths, group/message value
fields) correct.

### Correctness

A roundtrip test encodes messages of every `SampleType` (flat and batch shapes,
with non-default `eu_scale`/`eu_offset`, negative/zero/positive `t_ns`) and
asserts the direct decoder writes byte-identical samples to the reflection
decoder. A malformed-bytes test asserts no panic and no writes past a truncated
field (the scanner bounds-checks every read).

## Part B — Record-side batching

### Batch schema

`DynamicProtoRegistry::build_entry` generates, per topic, a **batch** file
descriptor instead of a flat one:

```proto
package mqtt;
message <Name> {                       // <Name> = sanitized topic (as today)
  repeated Sample samples = 1;
  message Sample {
    int64  t_ns  = 1;
    <type> value = 2;                  // Double | Int64 | Bool | String
  }
}
```

The inner `Sample` has the **same field numbers** as today's flat message, so a
`Sample` and today's flat top-level message encode to identical bytes.

### Producer change (minimal)

`encode()` builds and returns the bytes of a single **`Sample`** message (same
`set_field` calls, against the `Sample` descriptor) rather than the top-level
message — identical output bytes to today. `record_frame`/`record_numeric`
return `(batch_schema_bytes, sample_bytes, sample_type)`. Producers
(`ingest/scalar.rs`, `script/runner.rs`) send `RecordMsg::DynamicProto`
carrying the batch schema and the `Sample` bytes, unchanged in shape otherwise.

### Recorder buffering + flush

`McapRecorder` gains a per-topic buffer:

```rust
struct DynBuf {
    channel_id: u16,          // added on first sight, as today
    schema_bytes: Arc<[u8]>,  // the batch schema
    samples: Vec<u8>,         // concatenated: repeated tag+len+sample_bytes
    count: usize,
    first_flush_deadline: Instant,  // set when the buffer goes from empty→1
    last_log_time: i64,       // newest sample ts buffered
}
```

`write_dynamic(topic, schema, sample_bytes, ts)` appends to the topic's buffer:
`samples.extend([0x0A])`, `samples.extend(varint(sample_bytes.len()))`,
`samples.extend(sample_bytes)` — the wire encoding of one `repeated Sample`
element (field 1, wire type 2). No decode/re-encode. `count += 1`;
`last_log_time = ts`.

Flush a topic when `count >= 1024` **or** `now >= first_flush_deadline`
(deadline = first-buffered-time + 500 ms). A flush writes one MCAP message to
the topic's channel: `data = samples`, `log_time = last_log_time`, then clears
the buffer. `count == 0` topics are skipped.

The recorder loop's `select!` gains a `recv(ticker)` arm (a
`crossbeam_channel::tick(100ms)`); each tick scans buffers and flushes any past
their deadline or over size. `finish()` and `rotate()` flush **all** buffers
first, so no clean-exit data is lost. Only an actual crash loses at most one
buffer per topic (≤1024 samples / ≤500 ms), per the chosen policy.

The ZMQ `RecordMsg::Proto` path is unchanged — it writes through immediately as
today.

### Read-side backward compatibility

`plan_file` (`src/record/lazy/source.rs`) reconstructs bindings for topics with
no registry entry. Today it requires top-level `t_ns` + `value`. Generalize the
shape detection:

- Top-level has `t_ns` and `value` → **flat** (old files):
  `val_path=["value"]`, `ts_path=["t_ns"]` (unchanged).
- Top-level has a single repeated message field whose submessage has `t_ns` +
  `value` → **batch** (new files): `val_path=["<repeated>","value"]`,
  `ts_path=["<repeated>","t_ns"]`, `sample_type` from the submessage's `value`
  kind.

The existing 2-deep decode path (`decode_batch_channel`) already handles the
batch binding; Part A's `FastPlan` accelerates it.

### Timeline correctness under batching

A batch MCAP message carries one `log_time` (the batch's last sample ts). The
first batch in a chunk therefore stamps the chunk-index **start** bound up to
one batch span (≤500 ms) *after* the chunk's true earliest sample; the **end**
bound stays accurate (the last batch's log_time is the true max). To guarantee
no in-window chunk is skipped, `RecordingSource::overlapping(window)` pads only
the coarsened **start** comparison by `SPAN_PAD_NS`:

```rust
// span may hold in-window samples iff (start - pad) < window.end && end >= window.start
s.start_ns.saturating_sub(SPAN_PAD_NS) < window.end_ns && s.end_ns >= window.start_ns
```

`SPAN_PAD_NS = 500_000_000` (exactly the max batch span). It must not exceed the
existing `overlapping_selects_intersecting_spans` test's 5e8 gap (a larger pad
would over-select there), and the max batch span is 500 ms, so 500 ms is both
the floor for correctness and the ceiling for that test. Padding only widens
**chunk selection**; the per-sample clip in `DecodedChunk::window` still returns
exactly the samples inside the true `window`. Worst case decodes one extra
neighbouring chunk at a boundary. Old files (per-sample log_time) are unaffected.
`latest_at`'s candidate scan (`span.start_ns <= end_ns`) is already conservative
and needs no change.

## File structure

- `src/ingest/decode.rs` — add `FastPlan` + wire scanner; branch `decode_batch`
  to fast path or reflection fallback. (largest change)
- `src/ingest/router.rs` — build a `FastPlan` alongside each `ChannelBinding`
  (or store `Option<FastPlan>` on the binding).
- `src/record/mqtt_schema.rs` — batch schema generation; `encode` emits `Sample`
  bytes; return sample bytes + type.
- `src/record/queue.rs` — `RecordMsg::DynamicProto` already carries
  `schema`+`data`+`ts`; keep shape (data now = Sample bytes).
- `src/record/writer.rs` — per-topic `DynBuf`, size/time flush, tick arm,
  flush-all on finish/rotate.
- `src/record/lazy/source.rs` — batch-shape detection in `plan_file`;
  `SPAN_PAD_NS` in `overlapping`.
- `src/ingest/scalar.rs`, `src/script/runner.rs` — pass batch schema + sample
  bytes (mechanical).
- `examples/mcap_profile.rs` — kept as the before/after measurement harness.

## Testing

**Part A**
- Direct == reflection roundtrip for Float/Int/Bool/Text, flat and batch shapes,
  with non-unit `eu_scale`/`eu_offset` and zero/negative `t_ns`.
- `FastPlan::build` returns `None` for an unsupported val_path depth → reflection
  path still decodes correctly.
- Malformed/truncated bytes: no panic, no over-read.

**Part B**
- `build_entry` produces a batch schema; `value_sample_type` on the inner
  `Sample` returns the right type; `plan_file` builds a 2-deep binding for it.
- Writer batches: write 3000 samples on one topic, flush; the file contains
  ⌈3000/1024⌉ = 3 MCAP messages (not 3000), and replay returns all 3000 samples
  with exact timestamps and values.
- Time flush: write 5 samples, advance past the 500 ms deadline via a tick,
  assert a message is written before the size threshold.
- `finish` flushes a partial buffer (write 10, finish, replay sees 10).
- Backward compat: an old-style flat file (existing test helpers) still loads
  and returns exact samples.
- Timeline: a batched file's `snapshot` over a narrow window at a chunk edge
  returns the correct samples (span-pad covers the coarsened bounds).

## Resolved decisions

1. **Flush policy** — size OR time, per topic: 1024 samples **or** 500 ms,
   whichever first; flush-all on finish/rotate. (User-selected.)
2. **Decode approach** — hand-rolled field-number scanner with a mandatory
   reflection fallback for shapes it can't represent. (User-selected;
   fallback is required for correctness on arbitrary ZMQ schemas.)
3. **Batching site** — in the recorder, by byte-concatenating producer-encoded
   `Sample` messages into a repeated field. No decode/re-encode; producers
   change only to encode `Sample` (identical bytes) + emit the batch schema.
4. **Batch `log_time`** — last sample ts (keeps duration/`latest` tight);
   coarsened chunk bounds handled by a `SPAN_PAD_NS` pad in `overlapping`.
5. **ZMQ path** — unchanged (already batched).
