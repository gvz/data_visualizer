# Script Output Frequency Design

## Goal

Make a computed (script) channel's **output sample rate a property of the
script's intent**, not an accident of the engine's tick timer — and, for
per-sample transforms, guarantee the output frequency equals the input
frequency. Do this without the redundant full-window recompute the engine
performs today.

Builds on [the Python Scripting Engine design](2026-08-01-python-scripting-engine-design.md).

## Background: how it works today

The engine thread ticks on a fixed `Duration::from_millis(16)` timer
(`src/script/mod.rs:313`). Each tick:

1. Snapshots a **time window** per input: `window = last(window_s, now)`
   (config `window_s`, default 10 s).
2. Calls the compiled `compute(ts, vals)`, which returns `(ts, vals)` batches.
3. Writes every returned sample whose timestamp is newer than a per-output
   cursor (`runner.rs:203`):

   ```rust
   for (&t, &v) in batch.ts.iter().zip(&batch.vals) {
       if t > self.last_written[i] {
           store.write_numeric(id, t, nv);
           self.last_written[i] = t;
       }
   }
   ```

So **the output rate is whatever distinct new timestamps the script returns
per tick.** Two real scripts show the two regimes:

- `channel_diff` returns `(t0[:n], a[:n] - b[:n])` — one output per input
  sample, at the input timestamps. Output frequency already **equals input
  frequency**; the 16 ms tick only batches.
- `sine_rms` returns `(t[-1:], [rms])` — one value per tick, stamped at the
  newest input time. Output frequency **equals the tick rate** (~62 Hz),
  independent of the input.

### Problems

1. **No explicit contract.** Whether a channel tracks input frequency or tick
   frequency is an emergent property of the script body. A script author has no
   declared knob, and a reader cannot tell the intended rate without reading
   `compute`.
2. **Latent data loss.** The input feed is a fixed *time* window plus a cursor.
   If more than `window_s` of new samples ever arrive between two ticks (a
   stalled tick, a burst, or `window_s` set below the tick interval), samples
   older than the window but newer than `last_written` are silently skipped —
   output frequency drops below input frequency, invisibly. Safe at 62 Hz /
   10 s today, but fragile and undocumented.
3. **Redundant recompute.** Every tick re-snapshots and re-processes the whole
   `window_s` (millions of samples under load) to emit ~16 ms of genuinely new
   output. This is the same full-window waste already removed from the viz
   panels this cycle.

## Goals / non-goals

**Goals**

- A script declares its output-rate regime explicitly.
- Per-sample transforms produce output at exactly the input frequency, by
  construction — no dependence on tick timing, no dropped samples under bursts.
- Eliminate the per-tick full-window re-snapshot for per-sample transforms.
- Keep windowed reductions (RMS and friends) working unchanged in behaviour.
- Backward compatible: existing scripts keep running with no edit.

**Non-goals**

- Changing the Python `compute` signature or the numpy/numba contract.
- Interpolation or resampling of inputs onto a common grid (still out of scope,
  as in v1).
- Making reductions emit at input frequency — that is meaningless for a
  reduction and explicitly not a goal.

## Design

### Two script kinds

Introduce an optional script-declared mode, read like `INPUTS`/`OUTPUTS`:

```python
MODE = "map"      # or "reduce"   (default: "reduce")
```

| Mode | Feed | Output rate | State | Examples |
|------|------|-------------|-------|----------|
| `map` | cursor: samples since last processed | **= input rate** | stateless per sample | `channel_diff`, `sine_squared` |
| `reduce` | trailing time window (`window_s`) | tick/hop rate | needs history | `sine_rms` |

**Default is `reduce`** so every existing script — which today gets the full
time window — keeps its exact current feed and behaviour. `map` is opt-in.

### `map`: cursor-fed, input-frequency output

For a `map` script, replace the time-window feed with a **cursor window**: per
input, snapshot the half-open range `(cursor, now]` where `cursor` is the last
input timestamp already fed to this script (tracked per input, distinct from the
per-output `last_written`).

- Each input sample enters `compute` exactly once, ever.
- A `map` script returns one output per input sample at the input timestamp
  (the existing `channel_diff` shape). Every one is new, so every one is
  written → **output frequency == input frequency by construction**, regardless
  of tick rate, tick jitter, or burst size.
- No `window_s` re-snapshot: the fed slice is only the new tail (~one tick's
  worth), so per-tick work is O(new samples), not O(window).

Cursor advances to the newest input timestamp actually fed, after a successful
`compute`. On the first tick the cursor is unset → feed **at most the last
`window_s`** of existing samples (a bounded startup batch, same size as a
`reduce` window), then pure cursor thereafter. History older than `window_s` at
load time is skipped once.

### `reduce`: unchanged behaviour, optional hop

A `reduce` script keeps today's `last(window_s, now)` feed and the
dedup-by-`last_written` write. By default it recomputes and emits **every
tick** — the status quo. A script may declare an optional throttle:

```python
MODE = "reduce"
HOP_MS = 200      # recompute + emit at most every 200 ms (default: every tick)
```

`HOP_MS` gates both the compute and the emit, so a slow aggregate (a 10 s RMS)
need not be recomputed 62×/s. It is opt-in and backward compatible: absent, the
behaviour is exactly today's per-tick emit. `HOP_MS` below the tick interval is
clamped to the tick.

### Recording joins (`break_times`)

- **`map`** ignores joins. Its outputs are per-sample at the input timestamps,
  so a join needs no handling — the break is a display concern the panels
  already honour via the store's `break_times()`.
- **`reduce`** must not average across a join. Clamp its window to the current
  segment: if a `break_times()` boundary falls inside `last(window_s, now)`,
  start the window at that boundary so an RMS never blends two recordings.

### Replay

Computed channels come from the recording when possible:

- If a script's output channel is **present in the recording** (it was written
  to MCAP at capture time), replay those recorded samples directly — the engine
  does **not** tick against the playback store for it.
- Only when an output channel is **absent** from the recording does the engine
  recompute during replay. In that fallback path, a playback seek makes
  `now_ns` jump: on any `now_ns` discontinuity, reset the per-input cursor
  (re-backfill the last `window_s`, cap forward jumps to `window_s`) so a
  `map` script neither stalls on a backward seek nor floods on a forward one.

This keeps replay faithful to what was captured and avoids redundant recompute
whenever the derived channel was recorded.

### Multi-input `map` alignment

`map` inputs may arrive at different rates or phases. The cursor is tracked
**per input**, and the fed window for each input is its own `(cursor_i, now]`.
The script still receives parallel `(ts, vals)` arrays per input and remains
responsible for pairing (as `channel_diff` already does via `min(len)` at
`ch0`'s timestamps). We do **not** add interpolation. Consequence: a `map`
script's output rate tracks the input it timestamps against (by convention the
first input), which is the intended, documented behaviour.

## Efficiency

Under the reference load (`channel_diff` as `map`), the per-tick input snapshot
drops from `window_s` × rate (10 s ≈ millions of samples) to one tick's arrivals
(~a few thousand), and the redundant reprocessing of already-emitted samples
disappears. This mirrors the snapshot-bounding win already measured on the
spectrum panel.

**Measured baseline** (2026-08-16, scripting on, 8 ch @ 100 kHz, 3 runners:
`sine_squared`, `sine_rms`, `channel_diff`): `ScriptEngine::tick_rec` runs at
**11.7 ms and 21.9 MB per call** (p95 23.5 ms), consuming **67.7 % of a core**
and 22 % of all allocation. Because each tick waits 16 ms then runs all three
runners (~50 ms total), the loop completes only **~19 times/second** — the
full-window recompute starves its own 16 ms timer, so computed channels update
well below both the tick rate and their input rate. This is the before-number
the `map` path must beat: cursor-feeding the two elementwise scripts should cut
their per-call cost by ~2 orders of magnitude and let the tick reach 62 Hz;
`HOP_MS` bounds the remaining `reduce` recompute.

**After** (2026-08-16, `scripts/output-frequency` branch, same load; `channel_diff`
and `sine_squared` declared `map`, `sine_rms` `reduce`): `tick_rec` drops to
**1.90 ms and 5.0 MB per call** (from 11.7 ms / 21.9 MB) — a 6.2× time and 4.4×
allocation reduction. The engine loop's effective rate rises from ~19 Hz to
**~46 Hz** and the script thread falls from **67.7 % of a core to 16.0 %**. The
two `map` scripts are now near-free per call; the residual is `sine_rms`, still
correctly windowed over `window_s`. (A one-line integration defect surfaced
during this measurement — `load_into` rebuilt the runner's `ScriptMeta` with
`..Default::default()`, dropping the declared mode so every script ran as
`reduce`; fixed by carrying `loaded.meta.mode`/`hop_ms`, with a regression test.)

## Tick rate vs output rate

With `map` decoupled from the tick, the 16 ms tick becomes purely a
**latency/batch** knob (how promptly new outputs appear), never a resolution
knob. It should become configurable (per engine, `[scripts].tick_ms`, default
16) so a deployment can trade latency for CPU without affecting any channel's
sample rate. `reduce` scripts that want a specific emit cadence get a hop
(Open Questions) rather than inheriting the tick.

## Migration

- `channel_diff`, `sine_squared` → add `MODE = "map"`. Bodies already emit
  per-sample at input timestamps, so no logic change.
- `sine_rms` → add `MODE = "reduce"` (or rely on the default).
- No config changes required; `MODE` lives in the script file next to
  `INPUTS`/`OUTPUTS`.

## Testing

- `map` cursor advance: two ticks over a store; assert the second tick feeds
  only samples newer than the first tick's max, and that no input sample is fed
  twice.
- `map` input-frequency parity: write N input samples across several ticks;
  assert the output channel receives exactly N samples with the input
  timestamps.
- `map` burst safety: within one tick interval, inject more than `window_s` of
  samples; assert none are dropped (the failure mode of today's time window).
- `map` startup backfill: first tick over a pre-filled ring emits each existing
  sample once.
- `reduce` unchanged: existing `sine_rms` tests pass verbatim.
- Default mode: a script with no `MODE` behaves exactly as today (`reduce`).

## Resolved decisions

1. **`reduce` hop** — default to per-tick emit (status quo); add an optional
   `HOP_MS` that throttles both compute and emit. Opt-in, backward compatible.
2. **First-tick backfill** — cap to the last `window_s` of existing samples,
   then pure cursor. Bounded startup batch.
3. **Recording joins** — `map` ignores them (per-sample outputs, breaks are a
   display concern); `reduce` clamps its window to the current segment so it
   never averages across a join.
4. **Replay** — replay recorded output samples directly when the channel is in
   the recording (no engine tick); recompute only for channels absent from the
   recording, resetting per-input cursors on any `now_ns` discontinuity.

## Testing additions from the decisions

- `reduce` with `HOP_MS`: assert it emits no more often than the hop and that an
  absent `HOP_MS` still emits every tick.
- `reduce` window clamp at a `break_times()` boundary: assert the aggregate uses
  only current-segment samples.
- Replay: assert a recorded output channel replays its recorded samples and the
  engine does not tick it; assert an unrecorded output recomputes and its cursor
  resets across a seek (backward seek re-backfills, forward seek is capped).
