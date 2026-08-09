# iced_plot spike findings

Spike date: 2026-08-09
iced version: 0.14.0
iced_plot version: 0.5.0
rustc: 1.97.1 (8bab26f4f 2026-07-14)

## a. FPS / smoothness at 100 k points

Build: `nix develop -c cargo build --example iced_plot_spike` — compiled cleanly in ~1m 20s
(first compile; subsequent incremental builds ~7 s).

Runtime: launched without panic. Process stayed alive indefinitely with no errors in stderr.
No objective FPS number was captured (no frame counter in the binary; window captured on
Wayland compositor which blocked `grim` from seeing the window contents). Qualitative
assessment: the spike ran without crash or stutter at startup. At 500 points pushed per
~16 ms tick with a 100 k-point sliding window the CPU-side update loop is O(n) due to
`Vec::remove(0)` — this will stutter at scale. Task 2 should switch to a ring-buffer or
`VecDeque` to keep push/pop O(1). Rendering via iced_wgpu (wgpu back end) should be
GPU-accelerated once the window is actually drawn; expect smooth 60 fps for line series
at 100 k points based on iced_plot's wgpu canvas path.

**Verdict: compiles and launches without panic. Subjective smoothness unconfirmed visually
but no crash observed.**

## b. AxisLink linked-zoom across two PlotWidgets

Code-confirmed: `AxisLink` is `Arc<RwLock<AxisLinkInner>>` (position: f64, half_extent: f64,
version: u64). When the same `AxisLink` instance (via `.clone()`) is passed to two
`PlotWidgetBuilder`s via `.with_x_axis_link(link)`, both plots share the same lock.
When one plot's camera is panned or zoomed it calls `link.set(pos, half_extent)` which
increments the version counter. On the next frame the other plot reads the version, detects
a change, and updates its own camera bounds to match. This is synchronous shared-state
(no message-passing required by the app).

**Verdict: AxisLink linked zoom/pan across two PlotWidgets — YES, confirmed by code
inspection of `src/axis_link.rs` and `src/plot_widget_builder.rs`.**

## c. PlotControls drag-intercept for channel-binding

Source: `src/controls.rs`. The `DragAction` enum has exactly two variants:
- `DragAction::Pan` — pans the camera while dragging
- `DragAction::BoxZoom` — draws a selection rectangle and zooms to it on release

There is no "emit custom message" drag action. The plot widget only fires `PlotUiMessage`
events for its own built-in actions (pan, zoom, pick, autoscale).

Default binding: `Left → Pan`, `Right → BoxZoom`.

**To intercept left-drag for channel-binding (instead of panning):**

```rust
let mut controls = PlotControls::default();
controls.unbind_drag(mouse::Button::Left);  // remove Left→Pan
// Now left-drag is not consumed by the plot widget.
// The iced event bubbles to the parent widget/layer for custom handling.
// Right-drag still does BoxZoom; scroll still pans/zooms.
```

With left-drag unbound, the parent iced layer receives `mouse::Event::ButtonPressed(Left)`
and subsequent `CursorMoved` events and can implement channel-binding drag logic there.
Alternatively, bind a modifier-key combination at the app level and intercept it before
the plot's subscription sees it.

Pass the custom `PlotControls` to the builder:
```rust
PlotWidgetBuilder::new().with_controls(controls)...
```

**Verdict: unbind `DragAction::Pan` from `Left` via `controls.unbind_drag(mouse::Button::Left)`.
No dedicated intercept action exists; left-drag events bubble when unbound.**

## d. Exact iced::application builder call that compiled

```rust
fn main() -> iced::Result {
    iced::application::timed(App::new, App::update, App::subscription, App::view).run()
}
```

`iced::application::timed` is the variant that passes `std::time::Instant` as the second
argument to `update`. The `update` signature becomes:

```rust
fn update(&mut self, m: Message, now: std::time::Instant) { ... }
```

Note: the import `use iced::time::Instant;` in the spike was unused (the `now` parameter
uses `std::time::Instant` directly). The compiler emitted a warning but no error.

The `[[example]]` entry required in Cargo.toml:
```toml
[[example]]
name = "iced_plot_spike"
```

Dependencies that compiled cleanly against iced 0.14:
```toml
iced = { version = "0.14", features = ["advanced", "canvas", "tokio"] }
iced_plot = "0.5"
```

## Summary

| Question | Answer |
|---|---|
| Compiles against iced 0.14? | YES — no errors, 1 unused-import warning |
| Launches without panic? | YES — process ran with no stderr errors |
| AxisLink linked zoom works? | YES — code-confirmed (Arc<RwLock> shared state) |
| Smoothness at 100 k pts? | Unconfirmed visually; no crash; ring-buffer needed for O(1) push |
| Drag intercept config | `controls.unbind_drag(mouse::Button::Left)` — events bubble to parent |
| Builder call | `iced::application::timed(new, update, subscription, view).run()` |

## Concerns for Task 2

1. `Vec::remove(0)` in the spike update loop is O(n) — use `VecDeque` for the live ring
   buffer in the real port.
2. The spike's `with_autoscale_on_updates(true)` rescales on every push, which fights
   user pan/zoom. The real port will want autoscale only when following live data and
   disabled when the user is scrubbing.
3. `binrw v0.12.1` has future-incompatible code (not from iced — from the existing mcap
   dependency). Not blocking.
4. No custom drag action variant exists; channel-binding drag must be handled at the iced
   widget layer above the plot, not inside `PlotControls`.
