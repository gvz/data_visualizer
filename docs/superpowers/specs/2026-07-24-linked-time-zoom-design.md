# Linked Time-Zoom Design

**Date:** 2026-07-24
**Components:** `src/viz/common.rs`, `src/viz/waveform.rs`,
`src/viz/state_graph.rs`, `src/viz/mod.rs`, `src/workspace.rs`, `src/app.rs`

## Goal

Add a top-toolbar checkbox that links the time (X) axis of every time-based
panel. While checked, zooming the time axis on any waveform snaps all
participating panels to that same absolute time window.

## Participants

- **Waveform** — initiates and follows. Has interactive X box-zoom today.
- **State_graph** — follows only. Renders a trailing `time_window_s` today
  with no interactive zoom; gains the ability to display a linked absolute
  window. It does NOT get its own drag-zoom gesture.
- **Spectrum is excluded** — its X-axis is frequency (Hz), not time.

## Mechanism

The codebase already shares cross-panel state through egui's `ctx.data`:
`global_window_s(ctx)` / `set_global_window_s(ctx)` publish the toolbar's
default window to every panel without touching the `VizPanel::render`
signature. Linked zoom uses the same channel — no signature change, and the
five non-participating panels are untouched.

### New helpers in `common.rs`

Backed by `ctx.data` under fixed `egui::Id`s, mirroring `global_window_s`:

```rust
/// Whether linked time-zoom is armed (the toolbar checkbox). The app
/// republishes this each frame; panels read it during render.
pub fn linked_zoom_enabled(ctx: &egui::Context) -> bool;
pub fn set_linked_zoom_enabled(ctx: &egui::Context, on: bool);

/// The shared absolute-ns time window [start, end] while linked. `None`
/// means "armed but no shared zoom yet" (inert). A waveform's zoom gesture
/// writes it; every participating panel reads it.
pub fn linked_zoom_range(ctx: &egui::Context) -> Option<(i64, i64)>;
pub fn set_linked_zoom_range(ctx: &egui::Context, range: Option<(i64, i64)>);
```

`insert_temp` values persist across frames, so the range survives frame to
frame until overwritten. Default (nothing published, e.g. in headless
panel tests): `enabled = false`, `range = None` — existing behavior.

### New trait method in `mod.rs`

```rust
/// Freeze a linked time-window into this panel's own zoom state, so it
/// stays put after the link is released. Default no-op; waveform and
/// state_graph override. Only called for participating panels when a
/// shared zoom is active.
fn freeze_time_zoom(&mut self, _range: (i64, i64)) {}
```

## Effective-window rule (waveform and state_graph)

At the top of `render`, both panels compute their time window the same way:

```rust
let linked_on = common::linked_zoom_enabled(ui.ctx());
let linked = if linked_on { common::linked_zoom_range(ui.ctx()) } else { None };
// `active` is the zoom that governs this frame.
let active = linked.or(self.zoom); // linked wins; else this panel's own zoom
let (t0, end_ns) = match active {
    Some((a, b)) => (a, b),
    None => { /* existing trailing-window computation */ }
};
```

`linked.or(self.zoom)` honors "checking does not change any view": while
armed with no shared range yet (`linked == None`), a panel keeps its own
`self.zoom` (or trailing). Once a shared zoom exists, it overrides every
panel's individual zoom.

## Waveform gesture change

Only the X-zoom commit branch changes. On drag release, when the X axis is
zoomed:

```rust
let new = Some((a.min(b), a.max(b)));
if common::linked_zoom_enabled(ui.ctx()) {
    common::set_linked_zoom_range(ui.ctx(), new); // propagate to all
} else {
    self.zoom = new; // individual, as today
}
```

Y-zoom is unaffected — vertical zoom is never linked.

Double-click reset: when linked, clear the shared range so every panel
releases together; otherwise clear this panel's own zoom as today.

```rust
if inner.response.double_clicked() {
    if common::linked_zoom_enabled(ui.ctx()) {
        common::set_linked_zoom_range(ui.ctx(), None);
    } else {
        self.zoom = None;
    }
    self.y_zoom = None; // Y always local
}
```

## State_graph changes

- Add field `zoom: Option<(i64, i64)>` (in-memory, not serialized).
- Apply the effective-window rule above to pick `t0`/`span` instead of the
  current trailing-only computation. When `active` is `Some((a, b))`,
  `t0 = a`, `span = b - a`; otherwise the existing trailing window.
- Override `freeze_time_zoom(range)` → `self.zoom = Some(range)`.
- Override `reset_zoom()` → `self.zoom = None`.
- No drag-zoom gesture, no double-click handling (follower only).

## App / workspace wiring

### App field

One new field: `link_zoom: bool` (default `false`, in-memory only).

### Toolbar checkbox

In the top toolbar, immediately after the reset-zoom magnifier button:

```rust
if ui
    .checkbox(&mut self.link_zoom, icon::LINK)
    .on_hover_text("Link time zoom across all panels")
    .changed()
{
    if self.link_zoom {
        // Just armed: start inert.
        common::set_linked_zoom_range(ctx, None);
    } else {
        // Just released: freeze the shared window into each panel, if any.
        if let Some(r) = common::linked_zoom_range(ctx) {
            self.workspace.freeze_time_zoom(r);
        }
        common::set_linked_zoom_range(ctx, None);
    }
}
common::set_linked_zoom_enabled(ctx, self.link_zoom); // publish every frame
```

The toolbar is drawn before the central workspace each frame, so a freeze
applied on the uncheck frame is visible that same frame.

### Workspace method

```rust
/// Copy a linked time-window into every panel's own zoom state (used when
/// the link is released so panels stay frozen where they were).
pub fn freeze_time_zoom(&mut self, range: (i64, i64)) {
    for st in self.screens.values_mut() {
        for slot in &mut st.panels {
            slot.panel.freeze_time_zoom(range);
        }
    }
}
```

### Global reset-zoom button

The existing "reset zoom on all waveforms" button additionally clears the
shared range so a linked view resets too:

```rust
if ui.button(icon::MAGNIFYING_GLASS_MINUS)...clicked() {
    common::set_linked_zoom_range(ctx, None);
    self.workspace.reset_zoom();
}
```

## Behavior summary

| Event | Result |
|-------|--------|
| Check box | Enabled published; shared range cleared. No view change. |
| First X-zoom on a waveform | Shared range set; all participants snap to it. |
| Further zooms while linked | Shared range updated; all stay synced. |
| Double-click a linked waveform | Shared range cleared; all release. |
| Uncheck | Active shared window frozen into each panel as its own zoom. |
| Global reset-zoom button | Shared range cleared + every panel reset. |

## Persistence

None. `link_zoom` and the shared range are in-memory only, matching the
existing (unserialized) per-panel zoom state. Nothing new is written to
`layout.toml`.

## Testing

- **common.rs:** unit test — `set_linked_zoom_enabled` / `set_linked_zoom_range`
  round-trip through a `Context`; defaults are `false` / `None`.
- **waveform.rs:** headless render test — publish `enabled = true` and a
  `range` into the context, render, assert no panic and that the panel used
  the linked window (e.g. shared range unchanged by a pure render). Unit
  test `freeze_time_zoom` sets `self.zoom`.
- **state_graph.rs:** headless render with a linked range set → no panic;
  `freeze_time_zoom` sets `self.zoom`; `reset_zoom` clears it. Existing
  `WaveformPanel`/`StateGraphPanel` struct literals in tests add the new
  field.
- **workspace.rs:** `freeze_time_zoom` propagates to panels across screens.

## Out of Scope

- A drag-zoom gesture on state_graph (follower only).
- Linking the Y (value) axis — only time is shared.
- Linking spectrum or any frequency/other-axis panel.
- Persisting the checkbox state to config.
- Per-screen independent links — the link is app-global (only the active
  screen renders, so it effectively links the visible panels).
