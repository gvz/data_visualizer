# Free-Canvas Panel Layout — Design

**Status:** Approved for planning
**Date:** 2026-08-13
**Target:** egui app (`theme/catppuccin` branch)

## Goal

Replace the `egui_tiles` split-tree layout with a free canvas: users add
panels, drag them to any position, and resize them freely. Placement is
normalized (panels scale/reflow with the window), overlap is impossible
(a drag or resize blocks flush against neighbors), and geometry persists
per panel in `config.toml`.

## Motivation

Today each screen is an `egui_tiles::Tree<usize>` — a tree of binary splits
plus a grid container. Users can only split panes and drag split handles;
they cannot place a panel at an arbitrary position or size. The goal is
direct, spatial control of the dashboard while keeping every existing panel
type (`VizPanel`) untouched.

## Decisions (locked)

| Question | Decision |
|---|---|
| Layout model | Free canvas — arbitrary per-panel position and size |
| Coordinate space | Normalized `0..1` of the canvas (panels scale with the window) |
| Overlap | Impossible — collisions **block at contact** |
| Collision resolution | Axis-independent clamp; moving/growing edge stops flush at nearest neighbor and at canvas bounds |
| Snapping | While dragging/resizing, edges snap to align with neighbor (and canvas) edges, and size snaps to match a neighbor's width/height, within a pixel threshold |
| Rendering | Custom themed frames placed by `allocate_ui_at_rect` (no `egui::Area`, no z-order) |
| Target codebase | egui app on `theme/catppuccin` |

## Non-goals

- No push/reflow packing, no snap-to-grid.
- No change to any `VizPanel` implementation or the `VizPanel` trait.
- No change to screens/tabs, channel sidebar, data sources, or the
  workspace-level floating **settings** window.
- No z-order / bring-to-front (unreachable state with no overlap).

---

## Architecture

### Data model

`ScreenState` drops the tiling tree and holds a flat, index-aligned
placement list beside its panels:

```rust
pub struct ScreenState {
    pub panels: Vec<PanelSlot>,        // unchanged: type_name + Box<dyn VizPanel>
    pub layout: Vec<PanelPlacement>,   // index-aligned with `panels`
    pub settings_open: Option<usize>,  // unchanged
}

pub struct PanelPlacement {
    pub rect: NormRect,
}

/// A rectangle in normalized canvas coordinates. Each field is 0.0..=1.0.
pub struct NormRect { pub fx: f32, pub fy: f32, pub fw: f32, pub fh: f32 }
```

`panels[i]` and `layout[i]` describe the same panel; both are removed
together on delete. There is no separate `tree` and no `z`.

Invariants enforced on every write:
- `fw, fh >= MIN` (e.g. `MIN = 0.05`).
- `0.0 <= fx`, `0.0 <= fy`, `fx + fw <= 1.0`, `fy + fh <= 1.0` (fully
  on-canvas — a panel can never be lost off-edge).
- No two rects in a screen overlap.

### Coordinate mapping

Each frame the canvas rect is `ui.max_rect()`. Mapping is a pure pair:

```rust
fn to_pixels(r: NormRect, canvas: Rect) -> Rect;   // norm -> screen px
fn px_delta_to_norm(delta: Vec2, canvas: Rect) -> Vec2; // px -> norm
```

Because state is stored normalized and mapped fresh every frame, a window
resize needs no migration — panels simply re-project onto the new canvas.

### Modules

- **`src/canvas.rs`** (new): placement math, collision helpers, per-frame
  rendering + input for a screen. Pure helpers (`snap_move`, `snap_resize`,
  `resolve_move`, `resolve_resize`, `auto_arrange`, `to_pixels`,
  `px_delta_to_norm`) are unit-testable without egui.
- **`src/workspace.rs`** (trimmed): keeps `Workspace`, `ScreenState`,
  `PanelSlot`, add/delete/screen management, persistence glue, and the
  workspace-level settings window. `Workspace::ui` calls `canvas::show(...)`
  instead of `tree.ui(behavior)`. All `egui_tiles` code is removed.
- **`src/config/layout.rs`** (edited): `ScreenConfig` loses `tiles_json`;
  `PanelEntry` gains `frame`.

---

## Rendering & interaction

`canvas::show(ui, screen_state, store, channels, mqtt, &mut events)` does,
each frame:

1. `canvas_rect = ui.max_rect()`.
2. For each panel `i`, compute `px = to_pixels(layout[i].rect, canvas_rect)`
   and place a child UI with `ui.allocate_ui_at_rect(px, ...)`. No `Area`
   and no z-order — nothing ever stacks, so draw order is irrelevant.
3. Inside each child UI, draw a themed `Frame`:
   - **Title bar** (~22 px): a drag handle.
     `ui.interact(titlebar_rect, id, Sense::drag())`. Holds the panel
     title, a `⚙` gear (opens settings), and a `✕` close (delete).
   - **Body**: `panel.render(ui, store)` — unchanged.
   - **Resize grip**: a small bottom-right corner rect,
     `Sense::drag()`.
   - The title-bar and grip `interact` calls run **before**
     `panel.render`, so the panel's own widgets (drag-values, plot zoom)
     register afterward and sit on top within the body — the same z-order
     lesson as the old `pane_ui`.
4. Drag on the title bar or grip produces a *desired* rect, which flows
   through the **snap → block** pipeline (below); the resulting legal
   `NormRect` is written back to `layout[i].rect`. While a drag/resize is
   active, the matched snap guides are drawn as thin theme-accent lines
   across the canvas so the alignment is visible.

All frame chrome is custom-drawn with Catppuccin theme colors — the visual
control that motivated this work.

### The snap → block pipeline

Four pure helpers process every gesture. Order matters: build the desired
rect from the drag delta, **snap** it toward nearby guides, then **block**
it out of any overlap. Snap is a soft attractor; block is the hard wall.

```rust
fn snap_move(desired: NormRect, others: &[NormRect], snap: Vec2) -> NormRect;
fn snap_resize(desired: NormRect, others: &[NormRect], snap: Vec2) -> NormRect;
fn resolve_move(prev: NormRect, desired: NormRect, others: &[NormRect]) -> NormRect;
fn resolve_resize(prev: NormRect, desired: NormRect, others: &[NormRect]) -> NormRect;
```

`others` excludes the panel being manipulated. Bounds `[0,1]` are baked
into every clamp, so no separate `bounds` argument is needed. `snap` is the
per-axis threshold in normalized units: a fixed `SNAP_PX` (~8 px) divided by
the canvas width/height each frame, so the pull feels constant at any window
size.

**Guides** are derived once per gesture from `others` plus the canvas frame:
- *Vertical* (for X): `{0, 1}` ∪ each other's `fx` and `fx + fw`.
- *Horizontal* (for Y): `{0, 1}` ∪ each other's `fy` and `fy + fh`.
- *Size*: the set of neighbor widths `fw` and heights `fh`.

**`snap_move`** (rect keeps its size): for X, consider aligning either the
left edge `fx` or the right edge `fx + fw` to any vertical guide; take the
smallest correction `|Δ| ≤ snap.x` and apply it to `fx`. Repeat for Y with
the horizontal guides. This makes flush, aligned placement effortless —
edges click to neighbor edges and to the canvas border.

**`snap_resize`** (bottom-right grip moves the right/bottom edges): snap the
right edge `fx + fw` to a vertical guide, and independently snap the width
`fw` to a neighbor width (size-match); apply whichever correction is smaller
and `≤ snap.x`. Repeat for the bottom edge / height with `snap.y`.

**`resolve_move` / `resolve_resize`** then guarantee no overlap,
**axis-independently** (X, then Y):
- *Move, X:* if `desired`'s X-span overlaps a neighbor whose Y-span it also
  overlaps, clamp `fx` flush against that neighbor on the side it came from
  (sign of `desired.fx − prev.fx`); clamp to `[0, 1 − fw]`. Repeat for Y.
  Axis independence yields natural "slide along the wall" past a neighbor.
- *Resize, X:* cap the right edge at the nearest neighbor's left edge (for
  neighbors overlapping on Y); clamp `fw` to `[MIN, 1 − fx]`. Repeat for
  height.

Because snapping aligns edges to exactly the block boundary (a right edge
meeting a neighbor's left edge is flush, not overlapping), the two stages
reinforce each other: snap lands the panel flush, block guarantees it never
crosses.

---

## Panel lifecycle

- **Add:** toolbar `+` button and canvas right-click → "Add panel here".
  Spawns an *undefined* panel (existing `placeholder` type) at default size
  (~`0.3 × 0.3`), positioned at the first non-overlapping candidate from a
  coarse scan; if none is found it falls back to top-left and the block
  rules keep it legal. Its body shows the existing pictogram **type
  picker**; choosing a type calls the existing `define_panel`. An empty
  screen still seeds one undefined panel (`ensure_undefined_pane` kept).
- **Delete:** title-bar `✕` or context-menu "Delete" → remove index `i`
  from both `panels` and `layout`; clear `settings_open` if it pointed at
  a shifted index (same guard as today).
- **Drop channels:** unchanged semantics, relocated into the panel body —
  detect `dnd_hover_payload`/`dnd_release_payload::<Vec<String>>` over the
  panel rect, run `resolve_or_register_drop` then `panel.drop_channel`.
  Blue hover outline retained.
- **Context menu** (right-click title bar/body): **Settings**, **Reset
  zoom**, **Delete**. Split horizontal/vertical are removed. Settings opens
  the existing workspace-level floating `egui::Window` — the one legitimate
  overlay, which stays.

---

## Persistence & migration

### Schema

`ScreenConfig` drops `tiles_json`. `PanelEntry` gains a named `frame`:

```rust
pub struct PanelEntry {
    #[serde(rename = "type")]
    pub panel_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<[f32; 4]>,   // [fx, fy, fw, fh], each 0..1
    #[serde(flatten)]
    pub config: toml::Table,       // panel-specific keys, unchanged
}
```

`frame` is a named field, so serde extracts it before the flatten map — it
never leaks into the panel's own `config`. It is a plain inline array,
serialized before any array-of-tables in the same panel table (e.g. status
`states`), satisfying TOML's scalar/array-before-table ordering. A
round-trip test guards this ordering.

`to_config` writes `frame = Some(layout[i].rect.into())`;
`from_screen_config` reads `frame` back into `layout[i].rect`.

Example:

```toml
[[screens.main.panels]]
type = "waveform"
frame = [0.0, 0.0, 0.5, 0.4]
channels = ["load/ch0", "load/ch1"]
```

### Migration

- **Stale `tiles_json`:** now an unknown key. `ScreenConfig` does not set
  `deny_unknown_fields`, so old files load without error and the key is
  dropped on the next save.
- **Missing `frame`:** any screen where one or more panels lack `frame`
  triggers **auto-arrange**: `auto_arrange(n)` lays the panels into a
  `ceil(sqrt(n))`-column flow grid of equal, non-overlapping norm rects.
  Old layouts thus open as a tidy grid; the first free drag/save writes
  real `frame`s. Same fallback serves hand-written configs that omit
  geometry.

---

## Removals

- `egui_tiles` dependency in `Cargo.toml`.
- `workspace.rs`: `Tree`/`TreeBehavior`, `split_panel`, `parse_tree`,
  `tree_panes_valid`, `default_tree`, grid-column code
  (`grid_auto_column_count`), and every `tiles_json` reference in
  `to_config`/`from_screen_config`.
- `config/layout.rs`: the `tiles_json` field and its tests.

---

## Testing

Pure helpers keep most coverage at the unit level (no egui):

- **`snap_move`:** a left/right edge within `snap` of a neighbor or canvas
  edge snaps flush; nothing snaps beyond the threshold; the nearest of
  several candidates wins.
- **`snap_resize`:** the right/bottom edge snaps to a neighbor edge, and
  width/height snaps to match a neighbor's dimension, within `snap`; nothing
  snaps beyond it.
- **`resolve_move`:** stops flush at a neighbor; slides along the
  perpendicular axis when dragged past; clamps to `[0,1]` bounds.
- **`resolve_resize`:** growth blocked at a neighbor edge; enforces `MIN`
  size; clamped to bounds.
- **Pipeline:** snap-then-block on a gesture aimed into a neighbor lands the
  panel flush and non-overlapping.
- **`auto_arrange(n)`:** returns `n` rects, all in-bounds, pairwise
  non-overlapping.
- **Persistence:** `frame` round-trips through `to_config`/`from_config`;
  a panel missing `frame` is auto-arranged on load; a config carrying stale
  `tiles_json` loads without error and does not re-serialize it.
- **Lifecycle:** delete removes the entry from both `panels` and `layout`;
  `add_panel` yields a non-overlapping undefined panel; an empty screen
  seeds the picker.
- **Smoke:** keep `ui_renders_headless_without_panic` (`ctx.run`) against
  the new canvas path.

## Docs

- `docs/book/src/configuration/layout.md`: replace the `tiles_json` section
  with `frame = [x, y, w, h]` (normalized `0..1`) per panel; document
  drag-to-move, grip-resize, block-on-contact, edge/size snapping, and the
  toolbar / right-click add.
- `docs/book/src/panels.md`: drop the split instructions; note free
  placement.

## File structure

| File | Responsibility |
|---|---|
| `src/canvas.rs` (new) | Norm↔pixel mapping, `snap_move`/`snap_resize`, `resolve_move`/`resolve_resize`, `auto_arrange`, per-frame render + input (incl. snap-guide lines) for a screen |
| `src/workspace.rs` (edit) | `Workspace`/`ScreenState`/`PanelSlot`, add/delete/screen mgmt, persistence glue, settings window; delegates drawing to `canvas` |
| `src/config/layout.rs` (edit) | `PanelEntry.frame`; remove `ScreenConfig.tiles_json` |
| `Cargo.toml` (edit) | Remove `egui_tiles` |
| `docs/book/src/configuration/layout.md`, `panels.md` (edit) | Document `frame` + free-canvas interactions |
