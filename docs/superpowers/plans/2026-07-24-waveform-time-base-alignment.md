# Waveform Time-Base Alignment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every waveform panel share one time base per frame so equal-length live windows start at the same time and all grid lines fall at the same absolute times.

**Architecture:** The app publishes the active store's `now_ns()` once per frame into `ctx.data` (mirroring `global_window_s`). Each waveform reads that single value for its live window end and derives its whole-second grid origin from it, replacing the per-panel `store.now_ns()` call and the frozen per-panel `epoch_ns`.

**Tech Stack:** Rust, egui/eframe 0.28, egui_plot.

## Global Constraints

- Grid alignment applies to `waveform` panels only; `state_graph` and other panel types are out of scope.
- Independently-zoomed panels keep their explicit `zoom` window; do not force a shared start across independent zooms (that is the link-zoom checkbox's job). Grid origin is still shared with them.
- No persistence: the frame clock lives in `ctx.data` per frame; nothing is written to `layout.toml`.
- Code, comments, and commit messages in normal English. No `Co-Authored-By` / self-attribution on commits.
- The grid origin formula is `clock - clock.rem_euclid(1_000_000_000)` (whole-second floor). All waveforms must derive their origin from the *same* shared `clock` value so origins are identical.

---

### Task 1: Shared frame-clock helpers in `common.rs`

**Files:**
- Modify: `src/viz/common.rs` (add helpers next to `global_window_s` / the `linked_zoom_*` helpers, ~line 278)
- Test: `src/viz/common.rs` (`#[cfg(test)]` module in the same file)

**Interfaces:**
- Produces:
  - `pub fn frame_clock(ctx: &egui::Context) -> Option<i64>` — the published per-frame clock, `None` when unpublished.
  - `pub fn set_frame_clock(ctx: &egui::Context, ns: i64)` — publish it.

- [ ] **Step 1: Write the failing test**

Add to the `common.rs` test module:

```rust
#[test]
fn frame_clock_round_trips_through_ctx() {
    let ctx = egui::Context::default();
    // Unpublished default is None.
    assert_eq!(frame_clock(&ctx), None);
    set_frame_clock(&ctx, 1_700_000_000_000_000_000);
    assert_eq!(frame_clock(&ctx), Some(1_700_000_000_000_000_000));
}
```

If the test module does not already `use super::*;`, rely on the existing imports the neighboring `linked_zoom` tests use (they call the helpers unqualified). Match whatever the surrounding tests do.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p data_visualizer frame_clock_round_trips_through_ctx 2>&1 | tail -20`
(If the crate name differs, use `cargo test frame_clock_round_trips_through_ctx`.)
Expected: FAIL — `frame_clock` / `set_frame_clock` not found.

- [ ] **Step 3: Write the helpers**

Insert after the `set_linked_zoom_range` helper (near line 278):

```rust
fn frame_clock_id() -> egui::Id {
    egui::Id::new("datavis_frame_clock_ns")
}

/// The active store clock for this frame, published once by the app so every
/// panel shares one value instead of each sampling `now_ns()` independently.
/// `None` when unpublished (e.g. headless panel tests); callers fall back to
/// their own `store.now_ns()`.
pub fn frame_clock(ctx: &egui::Context) -> Option<i64> {
    ctx.data(|d| d.get_temp::<i64>(frame_clock_id()))
}

/// Publish the shared per-frame clock. Called once per frame by the app.
pub fn set_frame_clock(ctx: &egui::Context, ns: i64) {
    ctx.data_mut(|d| d.insert_temp(frame_clock_id(), ns));
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test frame_clock_round_trips_through_ctx 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/viz/common.rs
git commit -m "feat: shared per-frame clock helpers for panel time-base alignment"
```

---

### Task 2: Waveform reads the shared clock and shared grid origin

**Files:**
- Modify: `src/viz/waveform.rs` — remove the `epoch_ns` field (~line 45-49), its ctor init (~line 83), and its three test struct-literal inits; change the window/anchor computation (~line 242-266)
- Test: `src/viz/waveform.rs` (`#[cfg(test)]` module in the same file)

**Interfaces:**
- Consumes: `crate::viz::common::frame_clock` (Task 1).

- [ ] **Step 1: Write the failing test**

Add to the `waveform.rs` test module. This asserts a render under a published frame clock does not panic and leaves the clock untouched, and checks the shared-origin property:

```rust
#[test]
fn shared_frame_clock_render_without_panic() {
    let channels = registry();
    let store = LiveStore::from_registry(&channels);
    let id = channels.id("demo.sine").unwrap();
    for i in 0..100i64 {
        store.write_numeric(id, i * 1_000_000, NumericVal::Float((i as f64).sin()));
    }
    let reg = PanelRegistry::with_builtins();
    let e: PanelEntry =
        toml::from_str("type = \"waveform\"\nchannels = [\"demo.sine\"]").unwrap();
    let mut p = reg.build(&e, &channels).unwrap();

    let ctx = egui::Context::default();
    crate::viz::common::set_frame_clock(&ctx, 1_700_000_000_500_000_000);
    let _ = ctx.run(egui::RawInput::default(), |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| p.render(ui, &store));
    });
    // A pure render must not disturb the published clock.
    assert_eq!(
        crate::viz::common::frame_clock(&ctx),
        Some(1_700_000_000_500_000_000)
    );
}

#[test]
fn shared_grid_origin_is_whole_second_and_step_preserving() {
    // The grid origin is the whole-second floor of the shared clock. Two clocks
    // one whole second apart floor to origins one whole second apart, and any
    // origin is itself a whole second — the property that makes grid lines
    // coincide across panels sharing one clock.
    let floor = |c: i64| c - c.rem_euclid(1_000_000_000);
    let a = 1_700_000_000_500_000_000i64;
    let b = a + 1_000_000_000;
    assert_eq!(floor(a) % 1_000_000_000, 0);
    assert_eq!(floor(b) - floor(a), 1_000_000_000);
    // Sub-second jitter does not move the origin.
    assert_eq!(floor(a), floor(a + 123_456));
}
```

- [ ] **Step 2: Run tests to confirm the starting state**

Run: `cargo test shared_frame_clock_render_without_panic shared_grid_origin_is_whole_second_and_step_preserving 2>&1 | tail -20`
Expected: both PASS. These are safety/characterization tests, not red-first — the property test is pure arithmetic and the render test only proves publishing a clock does not perturb a render. They exist to stay green through the refactor in Steps 3-4; the real failure state to guard against is a compile break while the `epoch_ns` field is half-removed. Run this step so a regression in Steps 3-4 is visible against a known-green baseline.

- [ ] **Step 3: Remove the `epoch_ns` field**

Delete these lines from the struct (the doc comment and the field, ~line 45-49):

```rust
    /// Fixed x-origin (absolute ns, whole second) picked once. Plotting relative
    /// to this constant — rather than the per-frame window start — keeps grid
    /// lines anchored to absolute time (they scroll with the data) while still
    /// fitting the samples' small offsets in f64 without precision loss.
    epoch_ns: Option<i64>,
```

Delete the ctor init line (~line 83):

```rust
        epoch_ns: None,
```

Delete the `epoch_ns: None,` line from each of the three test struct literals (`zoomed_window_fetches_frozen_range_without_panic`, `y_zoomed_render_preserves_range_without_panic`, `freeze_time_zoom_sets_local_zoom`).

- [ ] **Step 4: Change the window and anchor computation**

Replace this block (~line 242-266):

```rust
        let win_s = effective_window_s(ui.ctx(), self.time_window_s);
        // When the toolbar link is armed, a shared time window (once any
        // waveform has zoomed) overrides this panel's own zoom; before the
        // first shared zoom (`linked == None`) the panel keeps its own view.
        let linked = if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
            crate::viz::common::linked_zoom_range(ui.ctx())
        } else {
            None
        };
        let (t0, end_ns) = match linked.or(self.zoom) {
            Some((a, b)) => (a, b),
            None => {
                let end = store.now_ns();
                (end - (win_s * 1e9) as i64, end)
            }
        };
        let window = TimeWindow { start_ns: t0, end_ns: end_ns + 1 };

        // Fixed plot origin (whole second) chosen on first render. All x values
        // are (ns - anchor)/1e9, so grid lines sit at absolute times and scroll
        // with the data instead of staying pinned to the screen.
        let anchor = *self
            .epoch_ns
            .get_or_insert(end_ns - end_ns.rem_euclid(1_000_000_000));
        let x_of = move |ns: i64| (ns - anchor) as f64 / 1e9;
```

with:

```rust
        let win_s = effective_window_s(ui.ctx(), self.time_window_s);
        // One shared clock per frame, published by the app, so every waveform
        // uses the same live end and the same grid origin: equal windows then
        // start at the same time and grid lines coincide. Fall back to this
        // store when unpublished (e.g. headless panel tests).
        let clock =
            crate::viz::common::frame_clock(ui.ctx()).unwrap_or_else(|| store.now_ns());
        // When the toolbar link is armed, a shared time window (once any
        // waveform has zoomed) overrides this panel's own zoom; before the
        // first shared zoom (`linked == None`) the panel keeps its own view.
        let linked = if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
            crate::viz::common::linked_zoom_range(ui.ctx())
        } else {
            None
        };
        let (t0, end_ns) = match linked.or(self.zoom) {
            Some((a, b)) => (a, b),
            None => (clock - (win_s * 1e9) as i64, clock),
        };
        let window = TimeWindow { start_ns: t0, end_ns: end_ns + 1 };

        // Grid origin: the whole-second floor of the shared clock, recomputed
        // each frame. Because the clock is identical across panels this frame,
        // the origin is identical, so grid lines fall at the same absolute
        // times in every waveform. All x values are (ns - anchor)/1e9; the
        // include_x bounds, sample positions, and x-axis formatter all add the
        // origin back, so the visible window and tick labels are unchanged.
        let anchor = clock - clock.rem_euclid(1_000_000_000);
        let x_of = move |ns: i64| (ns - anchor) as f64 / 1e9;
```

- [ ] **Step 5: Run the waveform suite to verify it passes**

Run: `cargo test --lib viz::waveform 2>&1 | tail -25`
Expected: PASS — all waveform tests green (including the two new ones and the three edited struct-literal tests).

- [ ] **Step 6: Commit**

```bash
git add src/viz/waveform.rs
git commit -m "feat: waveforms share one per-frame clock and grid origin"
```

---

### Task 3: App publishes the frame clock each frame

**Files:**
- Modify: `src/app.rs` — in `update`, just after the `live_view_ns` sync block (~line 755), before `self.menu_bar(ctx)` / `self.toolbar(ctx)`

**Interfaces:**
- Consumes: `crate::viz::common::set_frame_clock` (Task 1).

- [ ] **Step 1: Add the publish line**

After the `if matches!(self.mode, AppMode::Live) { ... self.live_view_ns.store(v, ...) }` block (~line 755) and before the MQTT-snapshot refresh block, insert:

```rust
        // Publish the active store's clock once per frame so every panel shares
        // one time base: equal live windows start at the same time and grid
        // lines coincide. `self.store` is the active store in both modes (the
        // playback store during replay), read after the live view/playback
        // clock is settled above.
        crate::viz::common::set_frame_clock(ctx, self.store.now_ns());
```

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build 2>&1 | tail -20`
Expected: builds clean (only the pre-existing binrw future-incompat warning).

- [ ] **Step 3: Run the full suite**

Run: `cargo test 2>&1 | tail -25`
Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "feat: publish shared per-frame clock for waveform time-base alignment"
```

---

## Manual Verification

1. Open two waveform panels on the same live channel with the **same** `time_window_s`. Confirm their left edges (start time) and grid lines line up exactly.
2. Set one panel's `time_window_s` to a value giving a 2 s (or larger) grid step; add a second panel created a few seconds later. Confirm grid lines still coincide (this is the case that failed before).
3. Zoom one panel (box-drag). Confirm the other keeps its live view and grid lines still align where the windows overlap.
4. Check the link-zoom checkbox still forces a shared window when used.
5. Load a replay recording; confirm two equal-window panels align and grids coincide as playback scrubs.
