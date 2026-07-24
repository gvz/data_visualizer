# Linked Time-Zoom Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A top-toolbar checkbox that links the time (X) axis across all time-based panels, so zooming one waveform snaps every waveform and state_graph to the same absolute time window.

**Architecture:** A shared time window lives in egui's `ctx.data`, exactly like the existing `global_window_s` side-channel — no change to the `VizPanel::render` signature. Waveform panels write the shared window on an X-zoom gesture; waveform and state_graph read it each frame. Unchecking the box "freezes" the shared window into each panel's own zoom via a new `freeze_time_zoom` trait method.

**Tech Stack:** Rust, egui/eframe 0.28, egui_plot, egui-phosphor icons.

## Global Constraints

- Participants are **waveform** (initiates + follows) and **state_graph** (follows only). Spectrum is excluded — its X-axis is Hz, not time.
- State_graph gets NO drag-zoom gesture of its own; it only follows the linked window and can be frozen/reset.
- Linked Y (value) zoom is out of scope — only the time axis is shared.
- No new persistence: `link_zoom` and the shared range are in-memory only; nothing new is written to `layout.toml`.
- Follow the existing `ctx.data` pattern (`global_window_s` / `set_global_window_s` in `src/viz/common.rs`) for all cross-panel state. Do NOT change the `render` trait signature.
- Commits carry NO `Co-Authored-By` / AI-attribution trailer.
- Effective-window rule (both participating panels use this exact expression): `let active = linked.or(self.zoom);` where `linked` is `Some` only when the box is checked AND a shared range exists.

---

### Task 1: Shared infrastructure — ctx.data helpers + trait method

**Files:**
- Modify: `src/viz/common.rs` (add helpers after `set_global_window_s`, ~line 278; add unit test in the existing `mod tests`, ~line 371)
- Modify: `src/viz/mod.rs` (add trait method after `reset_zoom`, line 42)

**Interfaces:**
- Produces:
  - `common::linked_zoom_enabled(ctx: &egui::Context) -> bool`
  - `common::set_linked_zoom_enabled(ctx: &egui::Context, on: bool)`
  - `common::linked_zoom_range(ctx: &egui::Context) -> Option<(i64, i64)>`
  - `common::set_linked_zoom_range(ctx: &egui::Context, range: Option<(i64, i64)>)`
  - `VizPanel::freeze_time_zoom(&mut self, range: (i64, i64))` — default no-op.

- [ ] **Step 1: Write the failing test**

Add to `src/viz/common.rs` inside `mod tests` (after the last test, before the closing `}` at line 524):

```rust
    #[test]
    fn linked_zoom_round_trips_through_ctx() {
        let ctx = egui::Context::default();
        // Defaults when nothing has been published.
        assert!(!linked_zoom_enabled(&ctx));
        assert_eq!(linked_zoom_range(&ctx), None);

        set_linked_zoom_enabled(&ctx, true);
        assert!(linked_zoom_enabled(&ctx));

        set_linked_zoom_range(&ctx, Some((100, 200)));
        assert_eq!(linked_zoom_range(&ctx), Some((100, 200)));

        set_linked_zoom_range(&ctx, None);
        assert_eq!(linked_zoom_range(&ctx), None);
    }
```

The test module already has `use super::*;` and `use eframe::egui;` is available via `super` (common.rs imports `egui`). If `egui` is not in scope inside the test, add `use eframe::egui;` to the test module.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p data_visualizer --lib viz::common::tests::linked_zoom_round_trips_through_ctx`
Expected: FAIL — `cannot find function linked_zoom_enabled`. (If the crate name differs, use `cargo test linked_zoom_round_trips_through_ctx`.)

- [ ] **Step 3: Add the helpers to `common.rs`**

Insert immediately after `set_global_window_s` (the function ending at line 278):

```rust
fn linked_zoom_enabled_id() -> egui::Id {
    egui::Id::new("datavis_linked_zoom_enabled")
}

fn linked_zoom_range_id() -> egui::Id {
    egui::Id::new("datavis_linked_zoom_range")
}

/// Whether linked time-zoom is armed (the toolbar checkbox). The app
/// republishes this into ctx data each frame; time-based panels read it during
/// render. Absent (e.g. headless panel tests) → false.
pub fn linked_zoom_enabled(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(linked_zoom_enabled_id())).unwrap_or(false)
}

/// Publish whether linked time-zoom is armed.
pub fn set_linked_zoom_enabled(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(linked_zoom_enabled_id(), on));
}

/// The shared absolute-ns time window `[start, end]` while linked, or `None`
/// when armed but not yet zoomed. A waveform's zoom gesture writes it; every
/// participating panel reads it. Absent → None.
pub fn linked_zoom_range(ctx: &egui::Context) -> Option<(i64, i64)> {
    ctx.data(|d| d.get_temp::<Option<(i64, i64)>>(linked_zoom_range_id())).flatten()
}

/// Publish (or clear, with `None`) the shared linked time window.
pub fn set_linked_zoom_range(ctx: &egui::Context, range: Option<(i64, i64)>) {
    ctx.data_mut(|d| d.insert_temp(linked_zoom_range_id(), range));
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p data_visualizer --lib viz::common::tests::linked_zoom_round_trips_through_ctx`
Expected: PASS

- [ ] **Step 5: Add the trait method to `mod.rs`**

In `src/viz/mod.rs`, immediately after the `reset_zoom` method (the `fn reset_zoom(&mut self) {}` ending at line 42, inside `trait VizPanel`), add:

```rust
    /// Freeze a linked time-window into this panel's own zoom state so it stays
    /// put after the link is released. Default no-op; waveform and state_graph
    /// override. Only called for participating panels when a shared linked zoom
    /// is active.
    fn freeze_time_zoom(&mut self, _range: (i64, i64)) {}
```

- [ ] **Step 6: Verify the crate still builds**

Run: `cargo build -p data_visualizer`
Expected: builds clean (the new trait method has a default body, so no existing impl breaks).

- [ ] **Step 7: Commit**

```bash
git add src/viz/common.rs src/viz/mod.rs
git commit -m "feat: linked-zoom ctx.data helpers and freeze_time_zoom trait method"
```

---

### Task 2: Waveform reads, writes, and freezes the linked window

**Files:**
- Modify: `src/viz/waveform.rs` (render window computation ~line 242; X-zoom commit ~line 403; double-click ~line 423; add `freeze_time_zoom` impl in the `impl VizPanel for WaveformPanel` block; add test in `mod tests`)

**Interfaces:**
- Consumes (from Task 1): `common::linked_zoom_enabled`, `common::linked_zoom_range`, `common::set_linked_zoom_range`, and the `VizPanel::freeze_time_zoom` method it overrides.
- Produces: waveform now honors and drives the shared linked window.

**Note on imports:** `waveform.rs` imports selected names from `crate::viz::common` (lines 8-12). Rather than add four names to that list, call the new helpers fully-qualified as `crate::viz::common::linked_zoom_enabled(...)` etc. in the code below.

- [ ] **Step 1: Write the failing test**

Add to `src/viz/waveform.rs` inside `mod tests` (place beside the existing headless render tests). This test builds a panel via the registry, publishes a linked window into the context, renders, and asserts the render consumed the linked window without panicking and left the shared range intact:

```rust
    #[test]
    fn linked_window_render_uses_shared_range_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        for i in 0..100i64 {
            store.write_numeric(id, i * 1_000_000, NumericVal::Float((i as f64).sin()));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            "type = \"waveform\"\nchannels = [\"demo.sine\"]",
        )
        .unwrap();
        let mut p = reg.build(&e, &channels).unwrap();

        let ctx = egui::Context::default();
        crate::viz::common::set_linked_zoom_enabled(&ctx, true);
        crate::viz::common::set_linked_zoom_range(&ctx, Some((10_000_000, 50_000_000)));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
        // A pure render must not disturb the shared range.
        assert_eq!(
            crate::viz::common::linked_zoom_range(&ctx),
            Some((10_000_000, 50_000_000))
        );
    }

    #[test]
    fn freeze_time_zoom_sets_local_zoom() {
        let mut p = WaveformPanel {
            title: String::new(),
            label: None,
            bound: Vec::new(),
            time_window_s: None,
            cursors: false,
            dots: false,
            y_unit: String::new(),
            cursor_a_ns: None,
            cursor_b_ns: None,
            epoch_ns: None,
            hidden: std::collections::HashSet::new(),
            zoom: None,
            y_zoom: None,
            zoom_drag_origin: None,
        };
        p.freeze_time_zoom((5, 9));
        assert_eq!(p.zoom, Some((5, 9)));
    }
```

Confirm the test module already imports `registry`, `PanelRegistry`, `PanelEntry`, `LiveStore`, `NumericVal`, and `egui` — the existing headless test in this module uses them, so they are in scope. Match the exact field list of `WaveformPanel` (lines 29-65) if it has drifted.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p data_visualizer --lib viz::waveform::tests::freeze_time_zoom_sets_local_zoom viz::waveform::tests::linked_window_render_uses_shared_range_without_panic`
Expected: FAIL — `no method named freeze_time_zoom` (the render test may pass by luck without the window rule, but must compile; the freeze test fails to compile until Step 4).

- [ ] **Step 3: Apply the effective-window rule in `render`**

In `src/viz/waveform.rs`, replace the current window computation (lines 242-249):

```rust
        let win_s = effective_window_s(ui.ctx(), self.time_window_s);
        let (t0, end_ns) = match self.zoom {
            Some((a, b)) => (a, b),
            None => {
                let end = store.now_ns();
                (end - (win_s * 1e9) as i64, end)
            }
        };
```

with:

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
```

- [ ] **Step 4: Route the X-zoom commit and double-click through the shared range**

Replace the X-zoom commit block (lines 404-412), currently:

```rust
                if zx {
                    let ns_at = |x: f32| {
                        anchor
                            + (tf.value_from_position(egui::pos2(x, frame.center().y)).x * 1e9)
                                as i64
                    };
                    let (a, b) = (ns_at(x0), ns_at(x1));
                    self.zoom = Some((a.min(b), a.max(b)));
                }
```

with:

```rust
                if zx {
                    let ns_at = |x: f32| {
                        anchor
                            + (tf.value_from_position(egui::pos2(x, frame.center().y)).x * 1e9)
                                as i64
                    };
                    let (a, b) = (ns_at(x0), ns_at(x1));
                    let new = Some((a.min(b), a.max(b)));
                    // While linked, propagate to every participant instead of
                    // zooming this panel alone.
                    if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
                        crate::viz::common::set_linked_zoom_range(ui.ctx(), new);
                    } else {
                        self.zoom = new;
                    }
                }
```

Then replace the double-click block (lines 423-426), currently:

```rust
        if inner.response.double_clicked() {
            self.zoom = None;
            self.y_zoom = None;
        }
```

with:

```rust
        if inner.response.double_clicked() {
            // While linked, releasing clears the shared window so every
            // participant returns to its own view together.
            if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
                crate::viz::common::set_linked_zoom_range(ui.ctx(), None);
            } else {
                self.zoom = None;
            }
            self.y_zoom = None; // Y is always local
        }
```

- [ ] **Step 5: Add the `freeze_time_zoom` override**

In the `impl VizPanel for WaveformPanel` block (starts line 137), add this method (place it next to the existing `reset_zoom` override if present, otherwise anywhere in the impl):

```rust
    fn freeze_time_zoom(&mut self, range: (i64, i64)) {
        self.zoom = Some(range);
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p data_visualizer --lib viz::waveform`
Expected: PASS (all waveform tests, including the two new ones).

- [ ] **Step 7: Commit**

```bash
git add src/viz/waveform.rs
git commit -m "feat: waveform reads, drives, and freezes the linked time window"
```

---

### Task 3: State_graph follows and freezes the linked window

**Files:**
- Modify: `src/viz/state_graph.rs` (add `zoom` field to struct ~line 33-39; set it in `ctor` ~line 55-60; apply effective-window rule in `render` ~line 122-124; add `freeze_time_zoom` and `reset_zoom` overrides in `impl VizPanel`; add tests in `mod tests`)

**Interfaces:**
- Consumes (from Task 1): `common::linked_zoom_enabled`, `common::linked_zoom_range`, and the `VizPanel::freeze_time_zoom` / `reset_zoom` methods it overrides.
- Produces: state_graph honors the shared linked window and can be frozen/reset.

- [ ] **Step 1: Write the failing test**

Add to `src/viz/state_graph.rs` inside `mod tests` (after `renders_headless_without_panic`, before the module's closing `}` at line 288):

```rust
    #[test]
    fn linked_window_render_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let motor = channels.id("motor.state").unwrap();
        for i in 0..50i64 {
            store.write_numeric(motor, i * 1_000_000, NumericVal::Int(i / 20));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            "type = \"state_graph\"\nchannel = \"motor.state\"",
        )
        .unwrap();
        let mut p = reg.build(&e, &channels).unwrap();

        let ctx = egui::Context::default();
        crate::viz::common::set_linked_zoom_enabled(&ctx, true);
        crate::viz::common::set_linked_zoom_range(&ctx, Some((5_000_000, 30_000_000)));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
    }

    #[test]
    fn freeze_then_reset_zoom() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            "type = \"state_graph\"\nchannel = \"motor.state\"",
        )
        .unwrap();
        let mut p = reg.build(&e, &channels).unwrap();
        p.freeze_time_zoom((5, 9));
        // reset_zoom must clear it back to the trailing/live view.
        p.reset_zoom();
        // A render after reset must still not panic (no lingering bad range).
        let store = LiveStore::from_registry(&channels);
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
    }
```

The existing `renders_headless_without_panic` test already uses `registry`, `PanelRegistry`, `PanelEntry`, `LiveStore`, `NumericVal`, and `egui`, so they are in scope.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p data_visualizer --lib viz::state_graph::tests::freeze_then_reset_zoom viz::state_graph::tests::linked_window_render_without_panic`
Expected: FAIL — `no method named freeze_time_zoom` on the boxed panel (compile error) until Step 5.

- [ ] **Step 3: Add the `zoom` field**

In `src/viz/state_graph.rs`, add a field to `struct StateGraphPanel` (currently lines 33-39). After the `time_window_s` field:

```rust
    /// Visible span in seconds; `None` follows the global default.
    time_window_s: Option<f64>,
    /// Active absolute-ns time window `[start, end]`. Set by the linked-zoom
    /// freeze (or a linked follow), it overrides the trailing window. This
    /// panel has no drag-zoom of its own — it only follows and freezes.
    /// In-memory only; not serialized.
    zoom: Option<(i64, i64)>,
```

Then set it in `ctor` (the `StateGraphPanel { ... }` literal, lines 55-60). After `time_window_s: opt_f64_opt(cfg, "time_window_s"),`:

```rust
        time_window_s: opt_f64_opt(cfg, "time_window_s"),
        zoom: None,
```

- [ ] **Step 4: Apply the effective-window rule in `render`**

In `render`, replace the current window computation (lines 122-124), currently:

```rust
        let end_ns = store.now_ns();
        let span = (effective_window_s(ui.ctx(), self.time_window_s) * 1e9) as i64;
        let t0 = end_ns - span;
```

with:

```rust
        // When the toolbar link is armed, a shared time window overrides this
        // panel's trailing view; before any shared zoom (`linked == None`) it
        // keeps its own frozen zoom, else the live trailing window.
        let linked = if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
            crate::viz::common::linked_zoom_range(ui.ctx())
        } else {
            None
        };
        let (t0, end_ns) = match linked.or(self.zoom) {
            Some((a, b)) => (a, b),
            None => {
                let end = store.now_ns();
                let span = (effective_window_s(ui.ctx(), self.time_window_s) * 1e9) as i64;
                (end - span, end)
            }
        };
        let span = (end_ns - t0).max(1);
```

Note: the rest of `render` uses both `end_ns` and `span` (e.g. `store.snapshot(... end_ns: end_ns + 1)` and `x_of` which divides by `span.max(1)`). The block above defines both. The existing `.max(1)` guard inside `x_of` still applies; keeping `span = (end_ns - t0).max(1)` here makes the zoomed span safe too. Leave the existing `let snap = store.snapshot(...)` line and everything below unchanged.

- [ ] **Step 5: Add the `freeze_time_zoom` and `reset_zoom` overrides**

In the `impl VizPanel for StateGraphPanel` block (starts line 91), add:

```rust
    fn freeze_time_zoom(&mut self, range: (i64, i64)) {
        self.zoom = Some(range);
    }

    fn reset_zoom(&mut self) {
        self.zoom = None;
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p data_visualizer --lib viz::state_graph`
Expected: PASS (all state_graph tests, including the two new ones).

- [ ] **Step 7: Commit**

```bash
git add src/viz/state_graph.rs
git commit -m "feat: state_graph follows and freezes the linked time window"
```

---

### Task 4: App/workspace wiring — checkbox, freeze propagation, reset

**Files:**
- Modify: `src/workspace.rs` (add `freeze_time_zoom` method near `reset_zoom` ~line 339; add a test in `mod tests`)
- Modify: `src/app.rs` (add `link_zoom: bool` field to the app struct ~line 188; init it ~line 245; add the toolbar checkbox after the reset-zoom button ~line 469; update the reset-zoom button ~line 463-469)

**Interfaces:**
- Consumes (from Task 1): `common::linked_zoom_enabled`/`set_linked_zoom_enabled`, `common::linked_zoom_range`/`set_linked_zoom_range`, and (via workspace) `VizPanel::freeze_time_zoom`.
- Produces: `Workspace::freeze_time_zoom(&mut self, range: (i64, i64))`.

- [ ] **Step 1: Write the failing workspace test**

The `mod tests` in `src/workspace.rs` (starts line 515) has a `build()` helper returning `(ChannelRegistry, PanelRegistry, Workspace)` with two screens of panels. Add this test after the existing tests (before the module's closing `}`):

```rust
    #[test]
    fn freeze_time_zoom_reaches_panels() {
        let (_ch, _reg, mut ws) = build();
        // Iterates every panel across both screens; must not panic.
        ws.freeze_time_zoom((1_000, 2_000));
        // Re-freezing with a new range is also fine.
        ws.freeze_time_zoom((3_000, 4_000));
    }
```

The `build()` layout uses numeric panels, for which `freeze_time_zoom` is the default no-op — this test exercises the propagation loop itself; the panel-level freeze behavior is verified by the waveform/state_graph tests in Tasks 2-3.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p data_visualizer --lib workspace::tests::freeze_time_zoom_reaches_panels`
Expected: FAIL — `no method named freeze_time_zoom` on `Workspace`.

- [ ] **Step 3: Add `Workspace::freeze_time_zoom`**

In `src/workspace.rs`, immediately after the `reset_zoom` method (lines 339-345), add:

```rust
    /// Copy a linked time-window into every panel's own zoom state across all
    /// screens — used when the toolbar link is released so panels stay frozen
    /// where they were.
    pub fn freeze_time_zoom(&mut self, range: (i64, i64)) {
        for st in self.screens.values_mut() {
            for slot in &mut st.panels {
                slot.panel.freeze_time_zoom(range);
            }
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p data_visualizer --lib workspace::tests::freeze_time_zoom_reaches_panels`
Expected: PASS

- [ ] **Step 5: Add the `link_zoom` field to the app struct**

In `src/app.rs`, add a field to the app struct after `default_window_s: f64,` (line 188):

```rust
    default_window_s: f64,
    /// Whether the toolbar "link time zoom" checkbox is on. In-memory only;
    /// published to egui ctx data each frame so time-based panels follow it.
    link_zoom: bool,
```

Initialize it in `new` — in the `Self { ... }` literal after `default_window_s,` (line 245):

```rust
            default_window_s,
            link_zoom: false,
```

- [ ] **Step 6: Update the reset-zoom button and add the checkbox**

In `src/app.rs`, the toolbar block. Replace the reset-zoom button (lines 463-469), currently:

```rust
                if ui
                    .button(icon::MAGNIFYING_GLASS_MINUS)
                    .on_hover_text("Reset zoom on all waveforms")
                    .clicked()
                {
                    self.workspace.reset_zoom();
                }
```

with (updated hover text + clear shared range, then the new checkbox right after):

```rust
                if ui
                    .button(icon::MAGNIFYING_GLASS_MINUS)
                    .on_hover_text("Reset zoom on all panels")
                    .clicked()
                {
                    crate::viz::common::set_linked_zoom_range(ctx, None);
                    self.workspace.reset_zoom();
                }
                if ui
                    .checkbox(&mut self.link_zoom, icon::LINK)
                    .on_hover_text("Link time zoom across all panels")
                    .changed()
                {
                    if self.link_zoom {
                        // Just armed: start inert (no shared window yet).
                        crate::viz::common::set_linked_zoom_range(ctx, None);
                    } else {
                        // Just released: freeze the shared window into each
                        // panel (if any zoom was active), then clear it.
                        if let Some(r) = crate::viz::common::linked_zoom_range(ctx) {
                            self.workspace.freeze_time_zoom(r);
                        }
                        crate::viz::common::set_linked_zoom_range(ctx, None);
                    }
                }
                crate::viz::common::set_linked_zoom_enabled(ctx, self.link_zoom);
```

Note: the toolbar closure has `ctx` in scope (it is `egui::TopBottomPanel::top("toolbar").show(ctx, ...)`). If the closure captured `ctx` by a different name, use `ui.ctx()` instead — it returns the same `&egui::Context`.

- [ ] **Step 7: Verify build and run the full suite**

Run: `cargo build -p data_visualizer && cargo test -p data_visualizer`
Expected: builds clean, all tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/app.rs src/workspace.rs
git commit -m "feat: toolbar link-zoom checkbox, freeze propagation, and reset"
```

---

## Manual Verification (after all tasks)

Not automated — confirm the interaction end to end by running the app:

1. Open a screen with two waveforms (and a state_graph) on the same channels/time base.
2. Check the **link** box in the toolbar. Nothing should change.
3. Box-zoom the time axis on one waveform. All waveforms and the state_graph should snap to the same time window.
4. Double-click one waveform. All should release to live together.
5. Zoom again, then **uncheck** the box. All panels should stay frozen at the shared window.
6. Click the reset-zoom (magnifier) button. Everything returns to live.
