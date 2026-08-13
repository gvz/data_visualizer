# Free-Canvas Panel Layout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the `egui_tiles` split-tree screen layout with a free canvas where panels have arbitrary normalized position/size, snap to neighbors, and can never overlap.

**Architecture:** Each screen keeps its `Vec<PanelSlot>` and gains an index-aligned `Vec<PanelPlacement>` (one normalized rect per panel). A new `src/canvas.rs` module owns pure geometry helpers (snap, block-at-contact collision, auto-arrange) plus the per-frame render+input loop. `Workspace::ui` delegates to `canvas::show` and applies returned events. Geometry persists as a `frame = [x,y,w,h]` array on each panel entry; `egui_tiles` and `tiles_json` are removed.

**Tech Stack:** Rust, egui/eframe 0.28, egui-phosphor icons, toml/serde.

## Global Constraints

- Normalized coordinates only: every rect field is `f32` in `0.0..=1.0`.
- `MIN = 0.05` (minimum panel width/height, normalized). `SNAP_PX = 8.0` (snap threshold in screen pixels, converted to normalized per-axis each frame).
- Overlap must be unreachable: every proposed geometry passes through snap → block before being stored.
- The `VizPanel` trait and every panel implementation stay **unchanged**.
- `Workspace::ui(ui, store, channels, reg, mqtt)` keeps its exact current signature — `src/app.rs` is not modified.
- Commits: **no** `Co-Authored-By` / AI self-attribution lines (project rule).
- Never stage or commit `config.toml` (local session state).
- Target branch: `theme/catppuccin`. egui version is 0.28 — use only 0.28 APIs.

---

### Task 1: Add `frame` geometry field to `PanelEntry`

Additive schema change. `tiles_json` stays for now (removed in Task 4) so the crate keeps compiling.

**Files:**
- Modify: `src/config/layout.rs`
- Test: `src/config/layout.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `PanelEntry { panel_type: String, frame: Option<[f32; 4]>, config: toml::Table }` — `frame` is `[fx, fy, fw, fh]`, each `0..1`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/config/layout.rs`:

```rust
#[test]
fn frame_round_trips_and_is_separate_from_config() {
    let src = r#"
[[screens.main.panels]]
type = "numeric"
frame = [0.0, 0.25, 0.5, 0.5]
channel = "demo.sine"
"#;
    let l = LayoutConfig::from_toml_str(src).unwrap();
    let p = &l.screens["main"].panels[0];
    assert_eq!(p.frame, Some([0.0, 0.25, 0.5, 0.5]));
    // `frame` must NOT leak into the panel's own config table.
    assert!(!p.config.contains_key("frame"));
    assert_eq!(p.config["channel"], toml::Value::String("demo.sine".into()));
    // Round-trips through serialization.
    let l2 = LayoutConfig::from_toml_str(&l.to_toml_string().unwrap()).unwrap();
    assert_eq!(l, l2);
}

#[test]
fn frame_absent_is_none() {
    let src = r#"
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
"#;
    let l = LayoutConfig::from_toml_str(src).unwrap();
    assert_eq!(l.screens["main"].panels[0].frame, None);
}
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p datavis --lib config::layout 2>&1 | tail -20`
(If the crate name differs, use `cargo test --lib config::layout`.)
Expected: compile error — `PanelEntry` has no field `frame`.

- [ ] **Step 3: Add the field**

In `src/config/layout.rs`, change `PanelEntry` to:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelEntry {
    #[serde(rename = "type")]
    pub panel_type: String,
    /// Normalized canvas geometry `[fx, fy, fw, fh]`, each `0..1`. Absent on
    /// hand-written configs and pre-canvas layouts; the workspace auto-arranges
    /// panels that lack it. A named field so it never enters `config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<[f32; 4]>,
    #[serde(flatten)]
    pub config: toml::Table,
}
```

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test --lib config::layout 2>&1 | tail -20`
Expected: PASS. The existing `parses_screens_and_panels`, `round_trips`, `tiles_json_round_trips` tests still pass.

- [ ] **Step 5: Commit**

```bash
git add src/config/layout.rs
git commit -m "feat(config): add per-panel frame geometry to PanelEntry"
```

---

### Task 2: Canvas geometry core (`src/canvas.rs` pure helpers)

Pure, egui-value-only math. No rendering yet. `egui::Rect`/`egui::Vec2` are plain value types usable without a context.

**Files:**
- Create: `src/canvas.rs`
- Modify: `src/lib.rs` (add `pub mod canvas;`)
- Test: `src/canvas.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces:
  - `pub struct NormRect { pub fx: f32, pub fy: f32, pub fw: f32, pub fh: f32 }`
  - `NormRect::from_array([f32;4]) -> NormRect`, `NormRect::to_array(self) -> [f32;4]`
  - `NormRect::to_pixels(self, canvas: egui::Rect) -> egui::Rect`
  - `pub struct PanelPlacement { pub rect: NormRect }`
  - `pub const MIN: f32 = 0.05;` `pub const SNAP_PX: f32 = 8.0;`
  - `px_delta_to_norm(delta: egui::Vec2, canvas: egui::Rect) -> egui::Vec2`
  - `snap_move(desired: NormRect, others: &[NormRect], snap: egui::Vec2) -> NormRect`
  - `snap_resize(desired: NormRect, others: &[NormRect], snap: egui::Vec2) -> NormRect`
  - `resolve_move(prev: NormRect, desired: NormRect, others: &[NormRect]) -> NormRect`
  - `resolve_resize(prev: NormRect, desired: NormRect, others: &[NormRect]) -> NormRect`
  - `auto_arrange(n: usize) -> Vec<NormRect>`

- [ ] **Step 1: Write the failing tests**

Create `src/canvas.rs` with only the test module first (so it fails to compile against missing items):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    fn r(fx: f32, fy: f32, fw: f32, fh: f32) -> NormRect {
        NormRect { fx, fy, fw, fh }
    }

    #[test]
    fn to_pixels_maps_into_canvas() {
        let canvas = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let px = r(0.5, 0.0, 0.5, 1.0).to_pixels(canvas);
        assert!((px.left() - 110.0).abs() < 1e-3);
        assert!((px.top() - 20.0).abs() < 1e-3);
        assert!((px.width() - 100.0).abs() < 1e-3);
        assert!((px.height() - 100.0).abs() < 1e-3);
    }

    #[test]
    fn snap_move_aligns_edge_within_threshold() {
        // Neighbor's left edge is at 0.50. Panel's right edge at 0.49 -> snaps flush.
        let me = r(0.29, 0.0, 0.20, 0.3);
        let neighbor = r(0.50, 0.0, 0.30, 0.3);
        let snap = egui::vec2(0.03, 0.03);
        let out = snap_move(me, &[neighbor], snap);
        assert!((out.fx + out.fw - 0.50).abs() < 1e-4, "right edge should snap to 0.50");
    }

    #[test]
    fn snap_move_ignores_far_edges() {
        let me = r(0.10, 0.0, 0.20, 0.3);
        let neighbor = r(0.50, 0.0, 0.30, 0.3);
        let snap = egui::vec2(0.03, 0.03);
        let out = snap_move(me, &[neighbor], snap);
        assert_eq!(out, me);
    }

    #[test]
    fn snap_resize_matches_neighbor_width() {
        // Neighbor width 0.30; my width 0.29 -> snaps to 0.30.
        let me = r(0.0, 0.0, 0.29, 0.3);
        let neighbor = r(0.60, 0.0, 0.30, 0.3);
        let snap = egui::vec2(0.03, 0.03);
        let out = snap_resize(me, &[neighbor], snap);
        assert!((out.fw - 0.30).abs() < 1e-4);
    }

    #[test]
    fn resolve_move_blocks_flush_against_neighbor() {
        let prev = r(0.0, 0.0, 0.30, 0.30);
        let neighbor = r(0.40, 0.0, 0.30, 0.30);
        // Try to move right into the neighbor.
        let desired = r(0.30, 0.0, 0.30, 0.30);
        let out = resolve_move(prev, desired, &[neighbor]);
        // Right edge cannot cross 0.40 -> fx capped at 0.10.
        assert!((out.fx - 0.10).abs() < 1e-4, "got {}", out.fx);
        assert!(out.fx + out.fw <= 0.40 + 1e-4);
    }

    #[test]
    fn resolve_move_slides_along_perpendicular_axis() {
        // Neighbor only overlaps in Y for part of the range; moving in Y past it
        // is unobstructed because X spans differ.
        let prev = r(0.0, 0.0, 0.30, 0.30);
        let neighbor = r(0.50, 0.0, 0.30, 0.30); // different X band
        let desired = r(0.0, 0.60, 0.30, 0.30);  // pure Y move
        let out = resolve_move(prev, desired, &[neighbor]);
        assert!((out.fy - 0.60).abs() < 1e-4);
    }

    #[test]
    fn resolve_resize_blocks_and_enforces_min() {
        let prev = r(0.0, 0.0, 0.20, 0.20);
        let neighbor = r(0.50, 0.0, 0.30, 0.20);
        let desired = r(0.0, 0.0, 0.90, 0.20); // grow width past neighbor
        let out = resolve_resize(prev, desired, &[neighbor]);
        assert!(out.fx + out.fw <= 0.50 + 1e-4, "blocked at neighbor left edge");
        assert!(out.fw >= MIN - 1e-6);
    }

    #[test]
    fn auto_arrange_is_in_bounds_and_non_overlapping() {
        for n in 1..=7usize {
            let rects = auto_arrange(n);
            assert_eq!(rects.len(), n);
            for a in &rects {
                assert!(a.fx >= -1e-6 && a.fy >= -1e-6);
                assert!(a.fx + a.fw <= 1.0 + 1e-6);
                assert!(a.fy + a.fh <= 1.0 + 1e-6);
            }
            for i in 0..n {
                for j in (i + 1)..n {
                    let (a, b) = (rects[i], rects[j]);
                    let overlap = a.fx < b.fx + b.fw - 1e-4
                        && b.fx < a.fx + a.fw - 1e-4
                        && a.fy < b.fy + b.fh - 1e-4
                        && b.fy < a.fy + a.fh - 1e-4;
                    assert!(!overlap, "rects {i} and {j} overlap");
                }
            }
        }
    }
}
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test --lib canvas 2>&1 | tail -20`
Expected: compile error — `NormRect`, `snap_move`, etc. not found.

- [ ] **Step 3: Implement the geometry core**

Prepend to `src/canvas.rs` (above the test module):

```rust
use eframe::egui;

/// Minimum normalized panel size (width and height).
pub const MIN: f32 = 0.05;
/// Snap threshold in screen pixels, converted to normalized per-axis per frame.
pub const SNAP_PX: f32 = 8.0;

/// A rectangle in normalized canvas coordinates; every field is `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormRect {
    pub fx: f32,
    pub fy: f32,
    pub fw: f32,
    pub fh: f32,
}

/// One panel's placement on the canvas. Parallel to `ScreenState::panels`.
#[derive(Debug, Clone, Copy)]
pub struct PanelPlacement {
    pub rect: NormRect,
}

impl NormRect {
    pub fn from_array(a: [f32; 4]) -> Self {
        Self { fx: a[0], fy: a[1], fw: a[2], fh: a[3] }
    }
    pub fn to_array(self) -> [f32; 4] {
        [self.fx, self.fy, self.fw, self.fh]
    }
    pub fn right(self) -> f32 {
        self.fx + self.fw
    }
    pub fn bottom(self) -> f32 {
        self.fy + self.fh
    }
    fn overlaps_x(self, o: NormRect) -> bool {
        self.fx < o.right() && o.fx < self.right()
    }
    fn overlaps_y(self, o: NormRect) -> bool {
        self.fy < o.bottom() && o.fy < self.bottom()
    }
    /// Map to pixel-space against the canvas rect.
    pub fn to_pixels(self, c: egui::Rect) -> egui::Rect {
        egui::Rect::from_min_size(
            egui::pos2(c.left() + self.fx * c.width(), c.top() + self.fy * c.height()),
            egui::vec2(self.fw * c.width(), self.fh * c.height()),
        )
    }
}

/// Convert a pixel delta to a normalized delta against the canvas size.
pub fn px_delta_to_norm(d: egui::Vec2, c: egui::Rect) -> egui::Vec2 {
    egui::vec2(d.x / c.width().max(1.0), d.y / c.height().max(1.0))
}

/// Snap position: pull the left or right edge to the nearest vertical guide
/// (neighbor edges + canvas borders), and top/bottom to horizontal guides,
/// within `snap`. Keeps size fixed.
pub fn snap_move(mut r: NormRect, others: &[NormRect], snap: egui::Vec2) -> NormRect {
    let mut xs = vec![0.0f32, 1.0];
    for o in others {
        xs.push(o.fx);
        xs.push(o.right());
    }
    let (mut best, mut adj) = (snap.x, 0.0f32);
    for g in &xs {
        for edge in [r.fx, r.right()] {
            let d = g - edge;
            if d.abs() < best {
                best = d.abs();
                adj = d;
            }
        }
    }
    r.fx += adj;

    let mut ys = vec![0.0f32, 1.0];
    for o in others {
        ys.push(o.fy);
        ys.push(o.bottom());
    }
    let (mut best, mut adj) = (snap.y, 0.0f32);
    for g in &ys {
        for edge in [r.fy, r.bottom()] {
            let d = g - edge;
            if d.abs() < best {
                best = d.abs();
                adj = d;
            }
        }
    }
    r.fy += adj;
    r
}

/// Snap size: pull the right/bottom edge to a guide, or the width/height to a
/// neighbor's dimension, whichever correction is smaller and within `snap`.
/// Top-left is fixed (bottom-right grip).
pub fn snap_resize(mut r: NormRect, others: &[NormRect], snap: egui::Vec2) -> NormRect {
    let mut xs = vec![0.0f32, 1.0];
    for o in others {
        xs.push(o.fx);
        xs.push(o.right());
    }
    let (mut best, mut new_w) = (snap.x, r.fw);
    for g in &xs {
        let d = (g - r.right()).abs();
        if d < best {
            best = d;
            new_w = g - r.fx;
        }
    }
    for o in others {
        let d = (o.fw - r.fw).abs();
        if d < best {
            best = d;
            new_w = o.fw;
        }
    }
    r.fw = new_w.max(MIN);

    let mut ys = vec![0.0f32, 1.0];
    for o in others {
        ys.push(o.fy);
        ys.push(o.bottom());
    }
    let (mut best, mut new_h) = (snap.y, r.fh);
    for g in &ys {
        let d = (g - r.bottom()).abs();
        if d < best {
            best = d;
            new_h = g - r.fy;
        }
    }
    for o in others {
        let d = (o.fh - r.fh).abs();
        if d < best {
            best = d;
            new_h = o.fh;
        }
    }
    r.fh = new_h.max(MIN);
    r
}

/// Clamp a move on the X axis so the rect (size fixed) never crosses a neighbor
/// that overlaps it on Y, and stays within `[0, 1]`.
fn clamp_x(prev: NormRect, target_fx: f32, others: &[NormRect]) -> f32 {
    let fw = prev.fw;
    let dir = target_fx - prev.fx;
    let mut fx = target_fx.clamp(0.0, 1.0 - fw);
    for o in others {
        if prev.overlaps_y(*o) {
            if dir > 0.0 && prev.right() <= o.fx + f32::EPSILON {
                fx = fx.min(o.fx - fw);
            } else if dir < 0.0 && prev.fx + f32::EPSILON >= o.right() {
                fx = fx.max(o.right());
            }
        }
    }
    fx.clamp(0.0, 1.0 - fw)
}

/// Y-axis twin of [`clamp_x`].
fn clamp_y(prev: NormRect, target_fy: f32, others: &[NormRect]) -> f32 {
    let fh = prev.fh;
    let dir = target_fy - prev.fy;
    let mut fy = target_fy.clamp(0.0, 1.0 - fh);
    for o in others {
        if prev.overlaps_x(*o) {
            if dir > 0.0 && prev.bottom() <= o.fy + f32::EPSILON {
                fy = fy.min(o.fy - fh);
            } else if dir < 0.0 && prev.fy + f32::EPSILON >= o.bottom() {
                fy = fy.max(o.bottom());
            }
        }
    }
    fy.clamp(0.0, 1.0 - fh)
}

/// Block-at-contact move. Resolves X then Y independently ("slide along wall").
pub fn resolve_move(prev: NormRect, desired: NormRect, others: &[NormRect]) -> NormRect {
    let fx = clamp_x(prev, desired.fx, others);
    let stepped = NormRect { fx, ..prev };
    let fy = clamp_y(stepped, desired.fy, others);
    NormRect { fx, fy, fw: prev.fw, fh: prev.fh }
}

/// Block-at-contact resize (bottom-right grip). Growth stops at the nearest
/// neighbor edge; enforces `MIN` and canvas bounds.
pub fn resolve_resize(prev: NormRect, desired: NormRect, others: &[NormRect]) -> NormRect {
    let mut fw = desired.fw.clamp(MIN, 1.0 - prev.fx);
    let mut fh = desired.fh.clamp(MIN, 1.0 - prev.fy);
    for o in others {
        if prev.overlaps_y(*o) && prev.fx < o.fx {
            fw = fw.min((o.fx - prev.fx).max(MIN));
        }
    }
    for o in others {
        if prev.overlaps_x(*o) && prev.fy < o.fy {
            fh = fh.min((o.fy - prev.fy).max(MIN));
        }
    }
    NormRect { fx: prev.fx, fy: prev.fy, fw, fh }
}

/// Lay `n` panels into an equal, non-overlapping flow grid. Used to seed
/// geometry when a loaded layout has no `frame`s.
pub fn auto_arrange(n: usize) -> Vec<NormRect> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f32).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let fw = 1.0 / cols as f32;
    let fh = 1.0 / rows as f32;
    (0..n)
        .map(|i| {
            let c = i % cols;
            let r = i / cols;
            NormRect { fx: c as f32 * fw, fy: r as f32 * fh, fw, fh }
        })
        .collect()
}
```

Then add `pub mod canvas;` to `src/lib.rs` alongside the other `pub mod` lines.

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test --lib canvas 2>&1 | tail -20`
Expected: all 8 canvas tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/canvas.rs src/lib.rs
git commit -m "feat(canvas): normalized geometry with snap and block-at-contact helpers"
```

---

### Task 3: Canvas render loop + workspace placement model

Wire the geometry into a live free canvas: add `canvas::show` (render + input), swap `ScreenState` from `Tree<usize>` to `Vec<PanelPlacement>`, and route add/delete/settings/define/drop through events. `egui_tiles` usage leaves `workspace.rs`. `Workspace::ui`'s signature is unchanged, so `app.rs` is untouched.

**Files:**
- Modify: `src/canvas.rs` (add `CanvasEvent`, `show`, `draw_type_picker`, `type_pictogram`)
- Modify: `src/workspace.rs` (rewrite `ScreenState`/`Workspace` off `egui_tiles`)
- Test: `src/workspace.rs` (rewrite the `#[cfg(test)]` module)

**Interfaces:**
- Consumes (from Task 2): `NormRect`, `PanelPlacement`, `snap_move`, `snap_resize`, `resolve_move`, `resolve_resize`, `px_delta_to_norm`, `auto_arrange`, `MIN`, `SNAP_PX`.
- Consumes (existing): `PanelSlot { type_name: String, panel: Box<dyn VizPanel> }`, `MqttDropCtx<'a>`, `crate::dynamic_channel::resolve_or_register_drop`, `crate::viz::placeholder::TYPE_NAME`, `PanelRegistry::pickable_type_names`.
- Produces:
  - `pub enum CanvasEvent { Define(usize, String), Settings(usize), ResetZoom(usize), Delete(usize), AddAt(NormRect) }`
  - `pub fn show(ui, panels: &mut [PanelSlot], layout: &mut [PanelPlacement], store, channels, mqtt, type_names) -> Vec<CanvasEvent>`
  - `ScreenState { panels: Vec<PanelSlot>, layout: Vec<PanelPlacement>, settings_open: Option<usize> }`

- [ ] **Step 1: Write failing workspace tests**

Replace the entire `#[cfg(test)] mod tests` block in `src/workspace.rs` with the placement-based suite below (drops all `egui_tiles`/`pane_count`/`split`/`tiles_json` tests):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::NormRect;
    use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::{placeholder, PanelRegistry};
    use eframe::egui;

    fn channels() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"

[channels."demo.counter"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"
"#,
        )
        .unwrap()
    }

    const LAYOUT: &str = r#"
[screens.main]
[[screens.main.panels]]
type = "numeric"
frame = [0.0, 0.0, 0.5, 1.0]
channel = "demo.sine"

[[screens.main.panels]]
type = "numeric"
frame = [0.5, 0.0, 0.5, 1.0]
channel = "demo.counter"
"#;

    fn build() -> (ChannelRegistry, PanelRegistry, Workspace) {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let cfg = LayoutConfig::from_toml_str(LAYOUT).unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        (ch, reg, ws)
    }

    #[test]
    fn from_config_reads_frames() {
        let (_, _, ws) = build();
        let st = &ws.screens["main"];
        assert_eq!(st.panels.len(), 2);
        assert_eq!(st.layout.len(), 2);
        assert_eq!(st.layout[0].rect, NormRect { fx: 0.0, fy: 0.0, fw: 0.5, fh: 1.0 });
        assert_eq!(st.layout[1].rect.fx, 0.5);
    }

    #[test]
    fn round_trip_preserves_panels_and_frames() {
        let (ch, reg, ws) = build();
        let cfg = ws.to_config();
        let main = &cfg.screens["main"];
        assert_eq!(main.panels.len(), 2);
        assert_eq!(main.panels[0].panel_type, "numeric");
        assert_eq!(main.panels[0].frame, Some([0.0, 0.0, 0.5, 1.0]));
        // Reload keeps geometry.
        let ws2 = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(ws2.screens["main"].layout[0].rect.fw, 0.5);
    }

    #[test]
    fn missing_frames_auto_arrange() {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let cfg = LayoutConfig::from_toml_str(
            r#"
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
[[screens.main.panels]]
type = "numeric"
channel = "demo.counter"
"#,
        )
        .unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        let st = &ws.screens["main"];
        assert_eq!(st.layout.len(), 2);
        // Two panels auto-arrange into a 2-column row (each width 0.5).
        assert!((st.layout[0].rect.fw - 0.5).abs() < 1e-4);
        assert!(st.layout[0].rect.fx != st.layout[1].rect.fx);
    }

    #[test]
    fn stale_tiles_json_loads_without_error() {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        // Old config with tiles_json and no frames must not error.
        let cfg = LayoutConfig::from_toml_str(
            r#"
[screens.main]
tiles_json = '{"anything":1}'
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
"#,
        )
        .unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(ws.screens["main"].panels.len(), 1);
    }

    #[test]
    fn add_panel_appends_non_overlapping_placement() {
        let (ch, reg, mut ws) = build();
        let entry = PanelEntry {
            panel_type: "numeric".into(),
            frame: None,
            config: toml::from_str(r#"channel = "demo.sine""#).unwrap(),
        };
        ws.add_panel(&entry, &reg, &ch).unwrap();
        let st = &ws.screens["main"];
        assert_eq!(st.panels.len(), 3);
        assert_eq!(st.layout.len(), 3);
        // New placement does not overlap any existing one.
        let new = st.layout[2].rect;
        for i in 0..2 {
            let o = st.layout[i].rect;
            let overlap = new.fx < o.fx + o.fw - 1e-4
                && o.fx < new.fx + new.fw - 1e-4
                && new.fy < o.fy + o.fh - 1e-4
                && o.fy < new.fy + new.fh - 1e-4;
            assert!(!overlap);
        }
    }

    #[test]
    fn remove_panel_drops_from_both_vecs() {
        let (_ch, _reg, mut ws) = build();
        let st = ws.screens.get_mut("main").unwrap();
        st.remove_panel(0);
        assert_eq!(st.panels.len(), 1);
        assert_eq!(st.layout.len(), 1);
        assert!(st.settings_open.is_none());
    }

    #[test]
    fn empty_screen_seeds_undefined_panel() {
        let mut st = ScreenState::empty("blank");
        st.ensure_undefined_pane();
        assert_eq!(st.panels.len(), 1);
        assert_eq!(st.layout.len(), 1);
        assert_eq!(st.panels[0].type_name, placeholder::TYPE_NAME);
    }

    #[test]
    fn define_panel_replaces_type() {
        let (ch, reg, _ws) = build();
        let mut st = ScreenState::empty("blank");
        st.ensure_undefined_pane();
        st.define_panel(0, "gauge", &reg, &ch);
        assert_eq!(st.panels[0].type_name, "gauge");
    }

    #[test]
    fn ui_renders_headless_without_panic() {
        let (ch, reg, mut ws) = build();
        let store = LiveStore::from_registry(&ch);
        store.write_numeric(ch.id("demo.sine").unwrap(), 1, NumericVal::Float(1.0));
        ws.active = "main".to_string();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ws.ui(ui, &store, &ch, &reg, None);
            });
        });
    }

    #[test]
    fn refresh_bindings_resolves_dynamic_mqtt_channel() {
        use std::collections::{BTreeMap, HashMap};
        use std::sync::RwLock;

        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let store = LiveStore::from_registry(&ch);
        let cfg = LayoutConfig::from_toml_str(
            r#"
[[screens.main.panels]]
type = "numeric"
channel = "home/sensors/temp"
"#,
        )
        .unwrap();
        let mut ws = Workspace::from_config(&cfg, &reg, &ch);
        assert!(ch.id("home/sensors/temp").is_none());

        let topic_map: MqttTopicMap = RwLock::new(HashMap::new());
        let mut snap = BTreeMap::new();
        snap.insert("home/sensors/temp".to_string(), "21.5".to_string());
        ws.refresh_bindings(&ch, &store, Some((&topic_map, &snap)));
        assert!(ch.id("home/sensors/temp").is_some());
    }
}
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test --lib workspace 2>&1 | tail -25`
Expected: compile errors — `ScreenState.layout` missing, `remove_panel(usize)` signature differs, `canvas::show` missing, etc.

- [ ] **Step 3: Add `CanvasEvent`, `show`, and picker helpers to `src/canvas.rs`**

Append to `src/canvas.rs` (above the test module). This reuses the existing drop/context-menu/type-picker behavior from the old `TreeBehavior::pane_ui`:

```rust
use egui_phosphor::regular as icon;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::resolve_or_register_drop;
use crate::store::ChannelStore;
use crate::viz::placeholder;
use crate::workspace::{MqttDropCtx, PanelSlot};

const TITLE_H: f32 = 22.0;
const GRIP: f32 = 12.0;

/// A user action the canvas cannot apply itself (it only borrows slices, so it
/// cannot add/remove entries). The workspace applies these after `show`.
pub enum CanvasEvent {
    Define(usize, String),
    Settings(usize),
    ResetZoom(usize),
    Delete(usize),
    AddAt(NormRect),
}

/// A phosphor glyph hinting at each panel type on the type picker.
fn type_pictogram(type_name: &str) -> &'static str {
    match type_name {
        "gauge" => icon::GAUGE,
        "waveform" => icon::WAVE_SINE,
        "numeric" => icon::HASH,
        "spectrum" => icon::CHART_BAR,
        "state_graph" => icon::WAVE_SQUARE,
        "status" => icon::TAG,
        "log" => icon::SCROLL,
        "xy_scatter" => icon::CHART_SCATTER,
        _ => icon::SQUARE,
    }
}

fn draw_type_picker(
    ui: &mut egui::Ui,
    type_names: &[&'static str],
    idx: usize,
    events: &mut Vec<CanvasEvent>,
) {
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        ui.label(egui::RichText::new("Choose a panel type").weak());
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for t in type_names {
                ui.allocate_ui(egui::vec2(64.0, 74.0), |ui| {
                    ui.vertical_centered(|ui| {
                        let glyph = type_pictogram(t);
                        let btn = egui::Button::new(egui::RichText::new(glyph).size(30.0))
                            .min_size(egui::vec2(52.0, 52.0));
                        if ui.add(btn).on_hover_text(*t).clicked() {
                            events.push(CanvasEvent::Define(idx, t.to_string()));
                        }
                        ui.label(egui::RichText::new(*t).small());
                    });
                });
            }
        });
    });
}

/// Render every panel of a screen as a free-floating, snappable, non-overlapping
/// frame. Mutates geometry (`layout`) and per-panel state (drop) in place;
/// structural changes come back as [`CanvasEvent`]s.
pub fn show(
    ui: &mut egui::Ui,
    panels: &mut [PanelSlot],
    layout: &mut [PanelPlacement],
    store: &dyn ChannelStore,
    channels: &ChannelRegistry,
    mqtt: Option<MqttDropCtx>,
    type_names: &[&'static str],
) -> Vec<CanvasEvent> {
    let canvas = ui.max_rect();
    let snap = egui::vec2(SNAP_PX / canvas.width().max(1.0), SNAP_PX / canvas.height().max(1.0));
    let rects: Vec<NormRect> = layout.iter().map(|p| p.rect).collect();
    let mut events = Vec::new();

    // Right-click empty canvas → add a panel at that spot.
    let bg = ui.interact(canvas, ui.id().with("canvas-bg"), egui::Sense::click());
    bg.context_menu(|ui| {
        if ui.button(format!("{} Add panel here", icon::PLUS_SQUARE)).clicked() {
            if let Some(p) = ui.ctx().pointer_interact_pos() {
                let fx = ((p.x - canvas.left()) / canvas.width().max(1.0)).clamp(0.0, 1.0 - 0.3);
                let fy = ((p.y - canvas.top()) / canvas.height().max(1.0)).clamp(0.0, 1.0 - 0.3);
                events.push(CanvasEvent::AddAt(NormRect { fx, fy, fw: 0.3, fh: 0.3 }));
            }
            ui.close_menu();
        }
    });

    for i in 0..panels.len() {
        let px = rects[i].to_pixels(canvas);
        let id = ui.id().with(("panel", i));
        let title_rect = egui::Rect::from_min_size(px.min, egui::vec2(px.width(), TITLE_H));
        let grip_rect = egui::Rect::from_min_size(
            egui::pos2(px.right() - GRIP, px.bottom() - GRIP),
            egui::vec2(GRIP, GRIP),
        );
        let others: Vec<NormRect> =
            rects.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, r)| *r).collect();

        // Interactions FIRST so the panel body's own widgets sit on top.
        let title_resp = ui.interact(title_rect, id.with("title"), egui::Sense::click_and_drag());
        let grip_resp = ui.interact(grip_rect, id.with("grip"), egui::Sense::drag());

        if title_resp.dragged() {
            let d = px_delta_to_norm(title_resp.drag_delta(), canvas);
            let desired = NormRect { fx: rects[i].fx + d.x, fy: rects[i].fy + d.y, ..rects[i] };
            let snapped = snap_move(desired, &others, snap);
            layout[i].rect = resolve_move(rects[i], snapped, &others);
        }
        if grip_resp.dragged() {
            let d = px_delta_to_norm(grip_resp.drag_delta(), canvas);
            let desired = NormRect { fw: rects[i].fw + d.x, fh: rects[i].fh + d.y, ..rects[i] };
            let snapped = snap_resize(desired, &others, snap);
            layout[i].rect = resolve_resize(rects[i], snapped, &others);
        }

        // Chrome.
        let v = ui.visuals().clone();
        ui.painter().rect_filled(px, 4.0, v.window_fill);
        ui.painter().rect_stroke(px, 4.0, v.window_stroke());
        ui.painter().rect_filled(
            title_rect,
            egui::Rounding { nw: 4.0, ne: 4.0, sw: 0.0, se: 0.0 },
            v.widgets.active.bg_fill,
        );
        // Resize grip: two short diagonal ticks.
        let gs = grip_rect.shrink(3.0);
        for off in [0.0, 4.0] {
            ui.painter().line_segment(
                [
                    egui::pos2(gs.right() - off, gs.bottom()),
                    egui::pos2(gs.right(), gs.bottom() - off),
                ],
                egui::Stroke::new(1.0, v.weak_text_color()),
            );
        }

        let is_undefined = panels[i].type_name == placeholder::TYPE_NAME;

        // Title bar contents.
        ui.allocate_ui_at_rect(title_rect.shrink2(egui::vec2(6.0, 2.0)), |ui| {
            ui.horizontal(|ui| {
                if !is_undefined {
                    ui.strong(panels[i].panel.title());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(icon::X).clicked() {
                        events.push(CanvasEvent::Delete(i));
                    }
                    if ui.small_button(icon::GEAR).clicked() {
                        events.push(CanvasEvent::Settings(i));
                    }
                });
            });
        });

        // Body.
        let body_rect =
            egui::Rect::from_min_max(egui::pos2(px.left(), px.top() + TITLE_H), px.max).shrink(4.0);
        ui.allocate_ui_at_rect(body_rect, |ui| {
            if is_undefined {
                draw_type_picker(ui, type_names, i, &mut events);
            } else {
                panels[i].panel.render(ui, store);
            }
        });

        // Drag-drop channels onto the panel.
        let drop_resp = ui.interact(px, id.with("drop"), egui::Sense::hover());
        if drop_resp.dnd_hover_payload::<Vec<String>>().is_some() {
            ui.painter()
                .rect_stroke(px, 4.0, egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 140, 255)));
        }
        if let Some(dropped) = drop_resp.dnd_release_payload::<Vec<String>>() {
            for raw in dropped.iter() {
                if let Some(name) = resolve_or_register_drop(raw, channels, store, mqtt) {
                    panels[i].panel.drop_channel(&name, channels);
                }
            }
        }

        // Context menu.
        drop_resp.context_menu(|ui| {
            if ui.button(format!("{} Settings", icon::GEAR)).clicked() {
                events.push(CanvasEvent::Settings(i));
                ui.close_menu();
            }
            if ui.button(format!("{} Reset zoom", icon::MAGNIFYING_GLASS_MINUS)).clicked() {
                events.push(CanvasEvent::ResetZoom(i));
                ui.close_menu();
            }
            ui.separator();
            if ui.button(format!("{} Delete panel", icon::TRASH)).clicked() {
                events.push(CanvasEvent::Delete(i));
                ui.close_menu();
            }
        });
    }
    events
}
```

- [ ] **Step 4: Rewrite `ScreenState`/`Workspace` in `src/workspace.rs`**

Make these changes (keep `PanelSlot`, `ErrorPanel`, `MqttDropCtx`, `refresh_bindings`, `reset_zoom`, `freeze_time_zoom`, `add_screen`, `remove_screen` as they are):

1. Replace the imports at the top:

```rust
use std::collections::BTreeMap;

use anyhow::anyhow;
use eframe::egui;

use crate::canvas::{self, auto_arrange, CanvasEvent, NormRect, PanelPlacement};
use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry, ScreenConfig};
use crate::dynamic_channel::{resolve_or_register_drop, MqttTopicMap};
use crate::store::ChannelStore;
use crate::types::SampleType;
use crate::viz::{placeholder, PanelRegistry, VizPanel};
```

(`resolve_or_register_drop` and `LinearDir`/`egui_tiles`/`egui_phosphor` are no longer used here; the drop logic moved to `canvas`. Remove the `egui_phosphor`, `egui_tiles`, and `resolve_or_register_drop` imports if the compiler flags them unused. `MqttTopicMap` stays for the type alias.)

2. Replace `ScreenState` and its impls. Delete `default_tree`, `parse_tree`, `tree_panes_valid`, `TreeBehavior`, `type_pictogram`, `split_panel`, and the old `remove_panel`/`ensure_undefined_pane`/`from_screen_config`. New definitions:

```rust
/// One screen: panels plus their canvas placements (index-aligned).
pub struct ScreenState {
    pub panels: Vec<PanelSlot>,
    pub layout: Vec<PanelPlacement>,
    /// Panel index whose settings window is currently open, if any.
    pub settings_open: Option<usize>,
}

impl ScreenState {
    fn empty(_name: &str) -> Self {
        Self { panels: Vec::new(), layout: Vec::new(), settings_open: None }
    }

    /// Seed a single undefined pane on an empty screen so the type-picker shows
    /// instead of a blank canvas.
    fn ensure_undefined_pane(&mut self) {
        if !self.panels.is_empty() {
            return;
        }
        self.panels.push(PanelSlot {
            type_name: placeholder::TYPE_NAME.to_string(),
            panel: Box::new(placeholder::PlaceholderPanel),
        });
        self.layout.push(PanelPlacement {
            rect: NormRect { fx: 0.15, fy: 0.15, fw: 0.7, fh: 0.7 },
        });
    }

    fn remove_panel(&mut self, idx: usize) {
        if idx >= self.panels.len() {
            return;
        }
        self.panels.remove(idx);
        self.layout.remove(idx);
        self.settings_open = None;
    }

    /// Add an undefined panel at `rect` (from a canvas right-click).
    fn add_undefined_at(&mut self, rect: NormRect) {
        self.panels.push(PanelSlot {
            type_name: placeholder::TYPE_NAME.to_string(),
            panel: Box::new(placeholder::PlaceholderPanel),
        });
        self.layout.push(PanelPlacement { rect });
    }

    /// Replace the pane at `idx` with a fresh, unconfigured panel of `type_name`.
    fn define_panel(
        &mut self,
        idx: usize,
        type_name: &str,
        reg: &PanelRegistry,
        channels: &ChannelRegistry,
    ) {
        let Some(slot) = self.panels.get_mut(idx) else { return };
        let entry = PanelEntry {
            panel_type: type_name.to_string(),
            frame: None,
            config: toml::Table::new(),
        };
        let panel = reg.build(&entry, channels).unwrap_or_else(|e| {
            Box::new(ErrorPanel {
                title: format!("{type_name} (unconfigured)"),
                msg: e.to_string(),
                orig: entry.config.clone(),
            })
        });
        slot.type_name = type_name.to_string();
        slot.panel = panel;
    }

    fn from_screen_config(
        _name: &str,
        sc: &ScreenConfig,
        reg: &PanelRegistry,
        channels: &ChannelRegistry,
    ) -> Self {
        let panels: Vec<PanelSlot> = sc
            .panels
            .iter()
            .map(|entry| {
                let panel = reg.build(entry, channels).unwrap_or_else(|e| {
                    Box::new(ErrorPanel {
                        title: format!("{} (error)", entry.panel_type),
                        msg: e.to_string(),
                        orig: entry.config.clone(),
                    })
                });
                PanelSlot { type_name: entry.panel_type.clone(), panel }
            })
            .collect();

        // Use saved frames only if every panel has one; otherwise auto-arrange.
        let layout = if !sc.panels.is_empty() && sc.panels.iter().all(|e| e.frame.is_some()) {
            sc.panels
                .iter()
                .map(|e| PanelPlacement { rect: NormRect::from_array(e.frame.unwrap()) })
                .collect()
        } else {
            auto_arrange(sc.panels.len())
                .into_iter()
                .map(|rect| PanelPlacement { rect })
                .collect()
        };

        Self { panels, layout, settings_open: None }
    }
}
```

3. Rewrite `Workspace::to_config` to emit `frame` and no `tiles_json`:

```rust
pub fn to_config(&self) -> LayoutConfig {
    let mut cfg = LayoutConfig::default();
    for (name, st) in &self.screens {
        let entries: Vec<PanelEntry> = st
            .panels
            .iter()
            .enumerate()
            .map(|(i, slot)| PanelEntry {
                panel_type: slot.type_name.clone(),
                frame: Some(st.layout[i].rect.to_array()),
                config: slot.panel.serialize(),
            })
            .collect();
        cfg.screens.insert(
            name.clone(),
            ScreenConfig { panels: entries, ..Default::default() },
        );
    }
    cfg
}
```

4. Rewrite `Workspace::ui` to delegate to `canvas::show` and apply events:

```rust
pub fn ui(
    &mut self,
    ui: &mut egui::Ui,
    store: &dyn ChannelStore,
    channels: &ChannelRegistry,
    reg: &PanelRegistry,
    mqtt: Option<MqttDropCtx>,
) {
    let type_names = reg.pickable_type_names();
    let Some(st) = self.screens.get_mut(&self.active) else { return };
    st.ensure_undefined_pane();

    let events = canvas::show(
        ui,
        &mut st.panels,
        &mut st.layout,
        store,
        channels,
        mqtt,
        &type_names,
    );
    for ev in events {
        match ev {
            CanvasEvent::Define(idx, t) => st.define_panel(idx, &t, reg, channels),
            CanvasEvent::Settings(idx) => st.settings_open = Some(idx),
            CanvasEvent::ResetZoom(idx) => {
                if let Some(slot) = st.panels.get_mut(idx) {
                    slot.panel.reset_zoom();
                }
            }
            CanvasEvent::Delete(idx) => st.remove_panel(idx),
            CanvasEvent::AddAt(rect) => st.add_undefined_at(rect),
        }
    }

    // Floating settings window (unchanged behavior).
    if let Some(pane) = st.settings_open {
        if let Some(slot) = st.panels.get_mut(pane) {
            let mut open = true;
            egui::Window::new(format!("{} settings", slot.panel.title()))
                .id(egui::Id::new(("panel-settings-window", pane)))
                .collapsible(false)
                .resizable(true)
                .open(&mut open)
                .show(ui.ctx(), |ui| slot.panel.config_ui(ui));
            if !open {
                st.settings_open = None;
            }
        } else {
            st.settings_open = None;
        }
    }
}
```

5. Rewrite `Workspace::add_panel` to push a placement (first non-overlapping default slot):

```rust
pub fn add_panel(
    &mut self,
    entry: &PanelEntry,
    reg: &PanelRegistry,
    channels: &ChannelRegistry,
) -> anyhow::Result<()> {
    let st = self
        .screens
        .get_mut(&self.active)
        .ok_or_else(|| anyhow!("no active screen"))?;
    let panel = reg.build(entry, channels).unwrap_or_else(|e| {
        Box::new(ErrorPanel {
            title: format!("{} (unconfigured)", entry.panel_type),
            msg: e.to_string(),
            orig: entry.config.clone(),
        })
    });
    st.panels.push(PanelSlot { type_name: entry.panel_type.clone(), panel });
    let rect = entry
        .frame
        .map(NormRect::from_array)
        .unwrap_or_else(|| free_slot(&st.layout));
    st.layout.push(PanelPlacement { rect });
    Ok(())
}
```

6. Add a `free_slot` helper (module-level in `workspace.rs`):

```rust
/// Pick a default-sized rect that does not overlap existing placements, by
/// scanning a coarse grid of candidate positions; falls back to top-left.
fn free_slot(existing: &[PanelPlacement]) -> NormRect {
    const W: f32 = 0.3;
    const H: f32 = 0.3;
    let overlaps = |c: NormRect| {
        existing.iter().any(|p| {
            let o = p.rect;
            c.fx < o.fx + o.fw - 1e-4
                && o.fx < c.fx + c.fw - 1e-4
                && c.fy < o.fy + o.fh - 1e-4
                && o.fy < c.fy + c.fh - 1e-4
        })
    };
    let steps = [0.0, 0.15, 0.3, 0.45, 0.6, 0.7];
    for fy in steps {
        for fx in steps {
            let c = NormRect { fx, fy, fw: W, fh: H };
            if !overlaps(c) {
                return c;
            }
        }
    }
    NormRect { fx: 0.0, fy: 0.0, fw: W, fh: H }
}
```

- [ ] **Step 5: Build and run the tests**

Run: `cargo test --lib workspace canvas config::layout 2>&1 | tail -30`
Expected: all workspace, canvas, and layout tests PASS. Fix any egui 0.28 API mismatches the compiler reports (e.g. `allocate_ui_at_rect`, `Rounding` field names) — these are the only expected friction.

- [ ] **Step 6: Full build + clippy**

Run: `cargo build 2>&1 | tail -20 && cargo clippy --lib 2>&1 | tail -20`
Expected: clean build; address any unused-import warnings from the removed `egui_tiles`/phosphor imports in `workspace.rs`.

- [ ] **Step 7: Launch and drive the app**

Run: `cargo run -- --demo` (let it open; drag a panel by its title bar, resize by the bottom-right grip, drag one toward another to confirm it blocks flush and snaps, right-click empty canvas to add a panel, pick a type, delete a panel). Confirm no panic and behavior matches the spec. Close the window.
Expected: free-canvas interactions work; panels never overlap.

- [ ] **Step 8: Commit**

```bash
git add src/canvas.rs src/workspace.rs
git commit -m "feat(canvas): free-canvas panel layout replacing the tiling tree"
```

---

### Task 4: Remove `tiles_json` and the `egui_tiles` dependency

Nothing references either after Task 3. Delete them.

**Files:**
- Modify: `src/config/layout.rs` (drop `tiles_json` field + its tests)
- Modify: `Cargo.toml` (remove `egui_tiles`)
- Test: `src/config/layout.rs`

- [ ] **Step 1: Remove the field**

In `src/config/layout.rs`, change `ScreenConfig` to:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScreenConfig {
    #[serde(default)]
    pub panels: Vec<PanelEntry>,
}
```

Delete the `tiles_json_round_trips` and `tiles_json_absent_is_none_and_not_serialized` tests (they reference the removed field). Add a test that a stale `tiles_json` key is ignored on load:

```rust
#[test]
fn stale_tiles_json_key_is_ignored() {
    let src = r#"
[screens.main]
tiles_json = '{"x":1}'
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
"#;
    let l = LayoutConfig::from_toml_str(src).unwrap();
    assert_eq!(l.screens["main"].panels.len(), 1);
    // Not re-serialized.
    assert!(!l.to_toml_string().unwrap().contains("tiles_json"));
}
```

- [ ] **Step 2: Run tests, verify they pass**

Run: `cargo test --lib config::layout 2>&1 | tail -20`
Expected: PASS (including the new `stale_tiles_json_key_is_ignored`).

- [ ] **Step 3: Remove the dependency**

In `Cargo.toml`, delete the line:

```toml
egui_tiles = { version = "0.9", features = ["serde"] }
```

- [ ] **Step 4: Verify the whole crate builds and tests pass**

Run: `cargo build 2>&1 | tail -20 && cargo test 2>&1 | tail -20`
Expected: clean build with no `egui_tiles` references; full test suite green.

- [ ] **Step 5: Commit**

```bash
git add src/config/layout.rs Cargo.toml Cargo.lock
git commit -m "chore: drop tiles_json and egui_tiles after canvas migration"
```

---

### Task 5: Documentation

Update the mdBook guide to describe free-canvas placement and the `frame` key.

**Files:**
- Modify: `docs/book/src/configuration/layout.md`
- Modify: `docs/book/src/panels.md`

- [ ] **Step 1: Update `layout.md`**

In `docs/book/src/configuration/layout.md`:
- Replace the `### `tiles_json`` section (the paragraph about the egui_tiles JSON tree) with a `### `frame`` section:

```markdown
### `frame`

Each panel carries its canvas geometry as `frame = [x, y, w, h]`, four numbers
in `0..1` normalized to the screen area (`x`/`y` = top-left corner, `w`/`h` =
size). Panels are placed freely, never overlap, and scale with the window.
Omit `frame` and datavis auto-arranges the screen's panels into a grid on load,
then writes real `frame`s back as you move them.
```

- In the `[[screens.<name>.panels]]` example block, add a `frame` line, e.g.:

```toml
[[screens.main.panels]]
type = "waveform"
frame = [0.0, 0.0, 0.5, 0.4]
channels = ["load/ch0", "load/ch7", "scripts/rms"]
cursors = false
dots = false
```

- Rewrite the "Building a layout in the GUI" list to describe free placement:

```markdown
## Building a layout in the GUI

1. Add a panel from the toolbar, or right-click empty canvas → **Add panel here**.
2. Choose a panel type from the picker.
3. Drag a panel by its title bar to move it; drag the bottom-right grip to
   resize. Panels snap to each other's edges and sizes and block on contact, so
   they never overlap.
4. Drag channels from the sidebar onto a panel. Select several and drop them
   together onto a multi-channel panel.
5. The arrangement is saved back to `config.toml` automatically.
```

- [ ] **Step 2: Update `panels.md`**

In `docs/book/src/panels.md`, remove any reference to splitting panes (the old split-based workflow). If the file mentions "split", replace with a note that panels are placed freely on the canvas. (Verify with a grep; if there is no such mention, no change is needed beyond confirming.)

Run: `grep -rn "split\|tiles_json\|tiling" docs/book/src/`
Expected after edits: no stale references to `tiles_json` or split-based layout remain.

- [ ] **Step 3: Commit**

```bash
git add docs/book/src/configuration/layout.md docs/book/src/panels.md
git commit -m "docs: document free-canvas layout and the frame key"
```

---

## Self-Review

**Spec coverage:**
- Data model (`ScreenState.layout`, `PanelPlacement`, `NormRect`) → Tasks 2, 3.
- Coordinate mapping (`to_pixels`, `px_delta_to_norm`) → Task 2.
- Snap → block pipeline (`snap_move`/`snap_resize`/`resolve_move`/`resolve_resize`, `SNAP_PX`, `MIN`) → Task 2, wired in Task 3.
- Rendering (custom frames, title-bar drag, grip resize, interactions-before-body, guide feel) → Task 3 `show`.
- Lifecycle (add via toolbar/right-click, undefined type picker, delete, drop, context menu) → Task 3.
- Persistence + migration (`frame` field, auto-arrange, stale `tiles_json` ignored) → Tasks 1, 3, 4.
- Removals (`egui_tiles`, `tiles_json`, tree code) → Tasks 3, 4.
- Docs → Task 5.

**Placeholder scan:** No TBD/TODO; every code step has concrete code; every test has assertions.

**Type consistency:** `NormRect`/`PanelPlacement` fields and helper signatures match across Tasks 2–3; `PanelEntry.frame: Option<[f32;4]>` used consistently in Tasks 1, 3, 4; `Workspace::ui` signature preserved so `app.rs` is untouched; `remove_panel(usize)` / `define_panel(usize, &str, …)` used consistently in tests and impl.

**Note for the implementer:** snap guide *lines* (drawing thin lines at matched guides during a drag) from the spec are a visual nicety not covered by a task step; the grip ticks and block/snap behavior are implemented. If desired, add guide-line drawing inside the `title_resp.dragged()`/`grip_resp.dragged()` branches in `show` — optional polish, no test depends on it.
