# Waveform Zoom Modes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the waveform panel's left-drag box zoom from horizontal-only into a plain drag that snaps to the dominant axis (X or Y) plus a Shift+drag free both-axis box zoom.

**Architecture:** A pure helper `zoom_axes(dx, dy, free) -> (bool, bool)` decides which axes a completed drag box zooms (dominant-axis for plain, per-axis-over-threshold for free). `WaveformPanel` gains a `y_zoom: Option<(f64,f64)>` field and a renamed `zoom_drag_origin: Option<egui::Pos2>` drag origin; `render` converts the chosen box edges to ns / Y-values and applies `.include_y(..)` bounds when Y-zoomed.

**Tech Stack:** Rust, egui/eframe 0.28, egui_plot. Existing crate — no new dependencies.

## Global Constraints

- All changes confined to `src/viz/waveform.rs`.
- Zoom state is in-memory only — nothing zoom-related is serialized to config.
- 5px min-travel threshold (`MIN_DRAG_PX`) prevents a sloppy click collapsing the view.
- egui_plot's built-in pan/scroll/zoom stay disabled; all zoom is our own box-drag logic.
- Caveman prose style does NOT apply to code, comments, or commit messages — write those normally.
- Never add Co-Authored-By / AI self-attribution to commits.

---

### Task 1: Pure axis-decision helper `zoom_axes`

**Files:**
- Modify: `src/viz/waveform.rs` (add module-level `const MIN_DRAG_PX` and `fn zoom_axes`, near the existing `nearest_sample_ts` helper ~line 82; add a `#[test]` in the `tests` module)

**Interfaces:**
- Produces: `fn zoom_axes(dx: f32, dy: f32, free: bool) -> (bool, bool)` — returns `(zoom_x, zoom_y)`. `dx`/`dy` are the drag box's pixel width/height. `free` = Shift held. Plain (`free == false`): only the dominant axis, and only if it clears `MIN_DRAG_PX`. Free (`free == true`): each axis independently if it clears `MIN_DRAG_PX`. Ties (`dx == dy`) resolve to X.
- Produces: `const MIN_DRAG_PX: f32 = 5.0;`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/viz/waveform.rs`:

```rust
    #[test]
    fn zoom_axes_snap_and_free() {
        // Plain drag snaps to the dominant axis only.
        assert_eq!(zoom_axes(20.0, 3.0, false), (true, false)); // horizontal
        assert_eq!(zoom_axes(3.0, 20.0, false), (false, true)); // vertical
        // Tie resolves to X.
        assert_eq!(zoom_axes(10.0, 10.0, false), (true, false));
        // Dominant axis under threshold → nothing zooms.
        assert_eq!(zoom_axes(3.0, 2.0, false), (false, false));
        // Free drag zooms each axis independently over threshold.
        assert_eq!(zoom_axes(20.0, 20.0, true), (true, true));
        // Free drag with one axis under threshold zooms only the other.
        assert_eq!(zoom_axes(20.0, 2.0, true), (true, false));
        assert_eq!(zoom_axes(2.0, 20.0, true), (false, true));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib zoom_axes_snap_and_free`
Expected: FAIL — compile error, `cannot find function 'zoom_axes' in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add near the top of `src/viz/waveform.rs`, just after the existing `const MAX_PLOT_BUCKETS: usize = 1000;` (~line 22):

```rust
/// Minimum drag travel (screen pixels) on an axis before a box-drag zooms it,
/// so a barely-dragged click doesn't collapse the view to a near-zero span.
const MIN_DRAG_PX: f32 = 5.0;
```

Add just before the `nearest_sample_ts` helper (~line 82):

```rust
/// Decide which axes a completed zoom-drag box applies to.
///
/// `dx`/`dy` are the box's pixel width/height; `free` is true when Shift is
/// held. Plain drags snap to the dominant axis (X on a tie); free drags zoom
/// each axis independently. An axis only zooms if its travel clears
/// `MIN_DRAG_PX`. Returns `(zoom_x, zoom_y)`.
fn zoom_axes(dx: f32, dy: f32, free: bool) -> (bool, bool) {
    if free {
        (dx >= MIN_DRAG_PX, dy >= MIN_DRAG_PX)
    } else if dx >= dy {
        (dx >= MIN_DRAG_PX, false)
    } else {
        (false, dy >= MIN_DRAG_PX)
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib zoom_axes_snap_and_free`
Expected: PASS — `test result: ok. 1 passed`.

- [ ] **Step 5: Commit**

```bash
git add src/viz/waveform.rs
git commit -m "feat(waveform): pure zoom_axes helper for snap/free axis selection"
```

---

### Task 2: Wire y_zoom state, gesture, bounds, and reset into the panel

**Files:**
- Modify: `src/viz/waveform.rs` — struct fields (~48-56), `ctor` (~76-77), plot builder (~285), drag-handling block (~335-365), `reset_zoom` (~458-461), and the two `WaveformPanel { .. }` literals in the `tests` module (~584-599); add one headless render test.

**Interfaces:**
- Consumes: `fn zoom_axes(dx: f32, dy: f32, free: bool) -> (bool, bool)` and `const MIN_DRAG_PX` from Task 1.
- Produces: `WaveformPanel` field `y_zoom: Option<(f64, f64)>` (absolute Y-value range, in-memory only) and renamed field `zoom_drag_origin: Option<egui::Pos2>` (was `zoom_drag_x0: Option<f32>`).

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/viz/waveform.rs`. Note it uses the new/renamed fields, so it will not compile until the struct is updated:

```rust
    #[test]
    fn y_zoomed_render_preserves_range_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let sine = channels.id("demo.sine").unwrap();
        for i in 0..1000i64 {
            store.write_numeric(sine, i * 1_000_000, NumericVal::Float((i as f64 * 0.1).sin()));
        }
        let mut p = WaveformPanel {
            title: "demo.sine".into(),
            label: None,
            bound: vec![bind("demo.sine", &channels, ACCEPTED)],
            time_window_s: None,
            cursors: false,
            dots: false,
            y_unit: String::new(),
            cursor_a_ns: None,
            cursor_b_ns: None,
            epoch_ns: None,
            hidden: std::collections::HashSet::new(),
            zoom: None,
            // Vertical zoom set; horizontal scroll stays live.
            y_zoom: Some((-0.5, 0.5)),
            zoom_drag_origin: None,
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| p.render(ui, &store));
        });
        // Y zoom is independent of the store clock, so it survives a render.
        assert_eq!(p.y_zoom, Some((-0.5, 0.5)));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib`
Expected: FAIL — compile errors: `struct WaveformPanel has no field named y_zoom` / `no field zoom_drag_origin`, and the existing `zoomed_window_fetches_frozen_range_without_panic` test's literal is now missing `y_zoom`.

- [ ] **Step 3: Update struct fields**

In the `WaveformPanel` struct (~48-56), replace the trailing `zoom` / `zoom_drag_x0` fields:

```rust
    /// Active horizontal time-zoom as an absolute-ns [start, end] range. When
    /// set the panel freezes live scrolling: both the data-fetch window and the
    /// plot x-bounds follow this range. In-memory only; double-click clears it.
    zoom: Option<(i64, i64)>,
    /// Active vertical zoom as an absolute Y-value [lo, hi] range. When set, the
    /// plot's y-bounds follow this range instead of auto-fitting. Unlike the X
    /// zoom it does NOT freeze horizontal scrolling — data keeps scrolling under
    /// a fixed Y window. In-memory only; double-click clears it.
    y_zoom: Option<(f64, f64)>,
    /// Screen position where the current zoom drag began, captured on
    /// drag-start. Needed on the release frame to know the box's opposite corner
    /// (the press origin is gone once the button is up). In-memory only.
    zoom_drag_origin: Option<egui::Pos2>,
```

- [ ] **Step 4: Update `ctor`**

In `ctor` (~76-77), replace `zoom: None,` / `zoom_drag_x0: None,` with:

```rust
        zoom: None,
        y_zoom: None,
        zoom_drag_origin: None,
```

- [ ] **Step 5: Apply Y bounds in the plot builder**

In `render`, immediately after the `if !unit.is_empty() { plot = plot.y_axis_formatter(..); }` block and before `let inner = plot.show(ui, |plot_ui| {` (~285), insert:

```rust
        // A vertical zoom pins the y-bounds; without it Y stays auto-fit.
        if let Some((lo, hi)) = self.y_zoom {
            plot = plot.include_y(lo).include_y(hi);
        }
```

- [ ] **Step 6: Replace the drag-handling block**

Replace the entire block from `let tf = &inner.transform;` through the `if inner.response.double_clicked() { self.zoom = None; }` closing brace (~335-365) with:

```rust
        // Left-drag draws a selection box; releasing zooms. A plain drag snaps
        // to the dominant axis (full-height band → X, full-width band → Y); a
        // Shift-drag is a free both-axis box zoom. Double-click clears both
        // zooms. A plain click is not a drag, so this never fires cursor
        // placement below.
        let tf = &inner.transform;
        let frame = *tf.frame();
        let primary = egui::PointerButton::Primary;
        if inner.response.drag_started_by(primary) {
            self.zoom_drag_origin = inner.response.interact_pointer_pos();
        }
        if let (Some(p0), Some(cur)) =
            (self.zoom_drag_origin, inner.response.interact_pointer_pos())
        {
            let free = ui.input(|i| i.modifiers.shift);
            let x0 = p0.x.min(cur.x).clamp(frame.left(), frame.right());
            let x1 = p0.x.max(cur.x).clamp(frame.left(), frame.right());
            let y0 = p0.y.min(cur.y).clamp(frame.top(), frame.bottom());
            let y1 = p0.y.max(cur.y).clamp(frame.top(), frame.bottom());
            let (dx, dy) = (x1 - x0, y1 - y0);
            let (zx, zy) = zoom_axes(dx, dy, free);

            // Preview the region that will apply: full 2D box under Shift, else
            // a band on whichever axis is dominant. Drawn while dragging
            // regardless of threshold so the intent is visible immediately.
            if inner.response.dragged_by(primary) {
                let rect = if free {
                    egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
                } else if dx >= dy {
                    egui::Rect::from_min_max(
                        egui::pos2(x0, frame.top()),
                        egui::pos2(x1, frame.bottom()),
                    )
                } else {
                    egui::Rect::from_min_max(
                        egui::pos2(frame.left(), y0),
                        egui::pos2(frame.right(), y1),
                    )
                };
                ui.painter().rect_filled(rect, 0.0, Color32::from_white_alpha(24));
                ui.painter().rect_stroke(rect, 0.0, egui::Stroke::new(1.0_f32, Color32::WHITE));
            }
            if inner.response.drag_stopped_by(primary) {
                if zx {
                    let ns_at = |x: f32| {
                        anchor
                            + (tf.value_from_position(egui::pos2(x, frame.center().y)).x * 1e9)
                                as i64
                    };
                    let (a, b) = (ns_at(x0), ns_at(x1));
                    self.zoom = Some((a.min(b), a.max(b)));
                }
                if zy {
                    let val_at =
                        |y: f32| tf.value_from_position(egui::pos2(frame.center().x, y)).y;
                    // Screen y grows downward, so y0 (top) is the larger value.
                    let (a, b) = (val_at(y0), val_at(y1));
                    self.y_zoom = Some((a.min(b), a.max(b)));
                }
                self.zoom_drag_origin = None;
            }
        }
        if inner.response.double_clicked() {
            self.zoom = None;
            self.y_zoom = None;
        }
```

- [ ] **Step 7: Update `reset_zoom`**

Replace the `reset_zoom` method (~458-461):

```rust
    fn reset_zoom(&mut self) {
        self.zoom = None;
        self.y_zoom = None;
        self.zoom_drag_origin = None;
    }
```

- [ ] **Step 8: Update the existing test literal**

In `zoomed_window_fetches_frozen_range_without_panic` (~584-599), the `WaveformPanel { .. }` literal has `zoom: Some((200_000_000, 400_000_000)),` then `zoom_drag_x0: None,`. Replace those two lines with:

```rust
            zoom: Some((200_000_000, 400_000_000)),
            y_zoom: None,
            zoom_drag_origin: None,
```

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS — all lib tests green, including `y_zoomed_render_preserves_range_without_panic`, `zoomed_window_fetches_frozen_range_without_panic`, and `zoom_axes_snap_and_free`.

- [ ] **Step 10: Verify a clean build**

Run: `cargo build`
Expected: compiles with no errors and no new warnings (no unused `zoom_drag_x0`, no unused imports).

- [ ] **Step 11: Commit**

```bash
git add src/viz/waveform.rs
git commit -m "feat(waveform): snap-to-axis and Shift free box zoom with Y bounds"
```

---

## Self-Review

**Spec coverage:**
- New `y_zoom` state → Task 2 Step 3. ✅
- `zoom_drag_origin` replacing `zoom_drag_x0` → Task 2 Step 3. ✅
- Plain-drag snap to dominant axis / Shift free zoom → `zoom_axes` (Task 1) + drag block (Task 2 Step 6). ✅
- Per-axis 5px threshold, free zoom leaves under-threshold axis unchanged → `zoom_axes` free branch + test (Task 1). ✅
- Live preview band (full-height X / full-width Y / 2D box under Shift) → Task 2 Step 6. ✅
- Apply `.include_y(lo).include_y(hi)` only when zoomed → Task 2 Step 5. ✅
- Y screen→value conversion, store as (min,max) → Task 2 Step 6 `val_at`. ✅
- Double-click clears both; `reset_zoom` clears both + origin → Task 2 Steps 6, 7. ✅
- Not persisted → no `serialize` change; nothing added there. ✅
- Headless y_zoom test → Task 2 Step 1. ✅

**Placeholder scan:** No TBD/TODO/"handle edge cases"; every code step shows full code. ✅

**Type consistency:** `zoom_axes(f32, f32, bool) -> (bool, bool)` and `const MIN_DRAG_PX: f32` defined in Task 1, consumed identically in Task 2 Step 6. Field names `y_zoom: Option<(f64,f64)>` and `zoom_drag_origin: Option<egui::Pos2>` used consistently across struct, ctor, render, reset, and both test literals. ✅
