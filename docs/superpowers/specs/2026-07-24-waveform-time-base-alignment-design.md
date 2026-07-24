# Waveform Time-Base Alignment Design

**Date:** 2026-07-24
**Components:** `src/viz/common.rs`, `src/viz/waveform.rs`, `src/app.rs`

## Goal

Two waveform panels showing the same amount of time must start at the same
time and draw their grid lines at the same points. Today they can drift:
each panel samples the clock and freezes its own grid origin independently,
so equal-length live windows differ by microseconds and grid lines land at
different absolute times (visible at grid steps of 2 s and larger).

## Root Cause

Each waveform's `render`:

1. Computes its live window end from its own `store.now_ns()` call. In live
   mode with no scrub, `now_ns()` is wall-clock, read once per panel per
   frame, so panels drift by the microseconds between calls.
2. Freezes a per-panel grid origin `epoch_ns` at first render, as the
   whole-second floor of *that panel's* end. Two panels first rendered in
   different seconds get whole-second origins that differ by a whole number
   of seconds. egui_plot places grid lines at multiples of the tick step in
   plot coordinates `(ns - epoch) / 1e9`; when the step does not divide the
   origin difference (e.g. a 2 s step with origins 1 s apart) the two panels'
   grid lines fall at different absolute times.

## Fix

Give every waveform one shared time base per frame:

- **Shared frame clock.** The app publishes the active store's `now_ns()`
  once per frame into `ctx.data`. Every waveform reads that single value for
  its live window end, so equal `time_window_s` yields an identical
  `[start, end]`. This also covers replay (all panels read the one published
  playback position).
- **Shared grid origin.** A single whole-second origin, seeded once from the
  shared clock and stored in `ctx.data` (`shared_epoch_ns`), is reused by
  every waveform for the rest of the session. It must be BOTH shared and
  frozen. Shared so all panels plot against the same origin and their grid
  lines coincide regardless of when a panel was created or what window length
  it shows. Frozen so the origin never moves between frames. The per-panel
  `epoch_ns` field is removed.

The origin must not be recomputed each frame. A per-frame whole-second floor
of an advancing clock jumps by one second at each whole-second boundary; for
grid steps that do not divide one second (2 s, 5 s, ...) that shifts every
grid line to a different absolute time, so the grid appears to change. A
fixed origin keeps grid lines pinned to absolute time. The tick labels add
the origin back, so they read as correct wall-clock time regardless of the
origin's phase, and f64 precision stays fine because realistic data sits
within days of the seed.

## Scope of alignment

- **Grid lines:** aligned across *all* waveforms, always, for any window
  length (shared origin needs no equal-length condition).
- **Start / window:** identical for any two waveforms in the live (trailing)
  view with equal `time_window_s`, because they share the frame clock.
- **Independently-zoomed panels:** left independent. A panel with an active
  `zoom` keeps its explicit `[start, end]`; its grid still aligns via the
  shared origin. "Same start" across two independent zooms of equal length
  has no non-arbitrary definition, so it is not forced here — the existing
  link-zoom checkbox remains the deliberate opt-in for a shared zoom window.

## Mechanism (mirrors `global_window_s`)

### New helpers in `common.rs`

```rust
fn frame_clock_id() -> egui::Id { egui::Id::new("datavis_frame_clock_ns") }

/// The active store clock for this frame, published once by the app so every
/// panel shares one value instead of each sampling `now_ns()` independently.
/// `None` when unpublished (e.g. headless panel tests); callers fall back to
/// their own `store.now_ns()`.
pub fn frame_clock(ctx: &egui::Context) -> Option<i64> {
    ctx.data(|d| d.get_temp::<i64>(frame_clock_id()))
}

pub fn set_frame_clock(ctx: &egui::Context, ns: i64) {
    ctx.data_mut(|d| d.insert_temp(frame_clock_id(), ns));
}
```

### Waveform `render`

Replace the two independent reads:

```rust
// Shared clock (fallback to this store when unpublished, e.g. tests).
let clock = crate::viz::common::frame_clock(ui.ctx())
    .unwrap_or_else(|| store.now_ns());

let (t0, end_ns) = match linked.or(self.zoom) {
    Some((a, b)) => (a, b),
    None => (clock - (win_s * 1e9) as i64, clock),
};

// Shared whole-second grid origin, identical across panels this frame.
let anchor = clock - clock.rem_euclid(1_000_000_000);
```

`self.epoch_ns` and its `get_or_insert` are removed (field, ctor init, and
the three test struct literals).

### App `update`

After the live-view / playback clock is settled for the frame (just after the
`live_view_ns` sync block, before the toolbar and central panel render):

```rust
crate::viz::common::set_frame_clock(ctx, self.store.now_ns());
```

`self.store` is already the active store in both modes (swapped to the
playback store during replay), so its `now_ns()` is the correct shared clock.

## Out of Scope

- `state_graph` and other panel types — this targets waveform alignment only.
- Forcing a shared start across independently-zoomed panels (that is the
  link-zoom checkbox's job).
- Any persistence — the frame clock is per-frame `ctx.data`, nothing is
  written to `layout.toml`.

## Testing

- **common.rs:** `set_frame_clock` / `frame_clock` round-trip through a
  `Context`; default (unpublished) is `None`.
- **waveform.rs:** a headless render with `set_frame_clock` published renders
  without panic and does not disturb the published clock; existing zoom / y-zoom
  struct-literal tests updated for the removed `epoch_ns` field. A unit test
  asserts the shared-origin formula: two clock values one whole second apart
  floor to origins one whole second apart, and any clock floors to a
  whole-second (`clock.rem_euclid(1e9) == 0` on the result) — the property that
  makes grids coincide.
- **app.rs:** existing tests unaffected (no UI harness); the publish line is
  covered by the waveform render path.
