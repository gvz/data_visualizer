# Waveform Zoom Modes Design

**Date:** 2026-07-24
**Component:** `src/viz/waveform.rs` (`WaveformPanel`)

## Goal

Extend the waveform panel's left-drag box zoom from horizontal-only to a
two-mode gesture: a plain drag that snaps to the dominant axis (X or Y),
and a Shift+drag that does a free both-axis box zoom.

## Current Behavior (baseline)

- Left-drag draws a full-height selection box; releasing zooms the **time
  (X) axis** to that span. Vertical extent is ignored.
- The X zoom is stored as `zoom: Option<(i64, i64)>` — an absolute-ns
  `[start, end]` range. While set, it freezes live scrolling and drives
  both the data-fetch window and the plot x-bounds.
- The Y axis is always auto-fit (egui_plot auto-bounds).
- Double-click clears `zoom` and resumes live scrolling.
- `reset_zoom()` (global reset hook) clears `zoom`.
- egui_plot's own pan/scroll/zoom are disabled; all zoom is our own
  box-drag logic. In-memory only; nothing zoom-related is serialized.
- A 5px min-travel threshold prevents a sloppy click from collapsing the
  view to a near-zero span.

## New Behavior

### State

Add one field beside `zoom`:

```rust
/// Active vertical zoom as an absolute Y-value [lo, hi] range. When set,
/// the plot's y-bounds follow this range instead of auto-fitting. Unlike
/// the X zoom it does NOT freeze horizontal scrolling — data keeps
/// scrolling under a fixed Y window. In-memory only; double-click clears it.
y_zoom: Option<(f64, f64)>,
```

`zoom_drag_x0: Option<f32>` is replaced by a two-coordinate drag origin so
the Y extent of the drag is known on the release frame (the press origin is
gone once the button is up):

```rust
/// Screen position where the current zoom drag began, captured on
/// drag-start. Needed on the release frame to know the box's opposite
/// corner. In-memory only.
zoom_drag_origin: Option<egui::Pos2>,
```

### Gesture

On left-drag release, with drag box screen corners `(x0,y0)-(x1,y1)`:

- **Plain drag → snap to dominant axis.** Compare box width vs height:
  - `|x1 - x0| >= |y1 - y0|` → **X zoom**: set `zoom` from the x-span
    (existing behavior). Leave `y_zoom` untouched.
  - else → **Y zoom**: set `y_zoom` from the y-span. Leave `zoom`
    untouched (horizontal scroll keeps running live).
- **Shift held → free box zoom.** Set both `zoom` (x-span) and `y_zoom`
  (y-span) from the box.

The 5px min-travel threshold applies to the axis being set: for a snap
zoom, require the dominant axis to exceed 5px; for a free zoom, require
both axes to exceed 5px (an axis under threshold is left unchanged so a
mostly-horizontal Shift-drag still just zooms X).

### Live preview during drag

Draw the region that will apply, decided live from press-origin → current
pointer:

- **X-dominant (plain):** full-height vertical band spanning `[x0, x1]`.
- **Y-dominant (plain):** full-width horizontal band spanning `[y0, y1]`.
- **Shift held:** the real 2D rectangle `(x0,y0)-(x1,y1)`.

Same fill/stroke styling as today (`Color32::from_white_alpha(24)` fill,
white 1px stroke).

### Applying bounds

The plot builder already emits `.include_x(x_of(t0))` / `.include_x(x_of(end_ns))`
while X-zoomed. Add, when `y_zoom` is `Some((lo, hi))`:

```rust
plot = plot.include_y(lo).include_y(hi);
```

When `y_zoom` is `None`, emit no `include_y` — Y stays auto-fit as today.

X and Y zoom are independent: a Y zoom persists while the data scrolls
horizontally; an X zoom freezes time while Y auto-fits (unless Y is also
zoomed).

### Reset

- **Double-click** clears both `zoom` and `y_zoom` → live scroll + Y
  auto-fit.
- **`reset_zoom()`** clears both `zoom` and `y_zoom` and `zoom_drag_origin`.

### Persistence

None. `y_zoom` is in-memory only, matching the existing `zoom` — nothing
zoom-related is written to config.

## Coordinate Conversions

- X: existing `ns_at(x)` maps screen-x → absolute ns via
  `anchor + tf.value_from_position(...).x * 1e9`.
- Y: `tf.value_from_position(egui::pos2(frame.center().x, y)).y` maps
  screen-y directly to a plot Y value (no anchor — Y is plotted in raw
  sample units). Store as `(min, max)` regardless of drag direction.

## Testing

Add a headless render test mirroring
`zoomed_window_fetches_frozen_range_without_panic`: construct a
`WaveformPanel` with `y_zoom: Some((lo, hi))` (and `zoom: None`), render
under a default `egui::Context`, assert no panic and that `y_zoom` is
preserved (Y zoom is independent of the store clock, so it survives a
render unchanged). Existing tests that construct `WaveformPanel` literals
must add the two new/renamed fields.

## Out of Scope

- Scroll-wheel or pinch zoom (egui_plot built-ins stay disabled).
- Snapping Y bounds to "nice" round numbers.
- Per-axis independent reset (double-click clears both — simplest, matches
  the single existing reset gesture).
