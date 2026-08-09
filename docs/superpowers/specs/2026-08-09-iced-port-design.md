# Porting datavis to the iced UI framework — Design

**Date:** 2026-08-09
**Status:** Approved for planning
**Branch/worktree:** `worktree-iced-port` (`.claude/worktrees/iced-port`)

## Goal

Replace the egui/eframe UI layer of `datavis` with the [iced](https://iced.rs)
GUI framework, reaching **full feature parity** in a single implementation
plan, on top of the existing (UI-agnostic) data backend.

## Decisions (locked)

| Question | Decision |
| --- | --- |
| Parity target | **Full parity**, one plan (all panels, replay, recording, data-source GUI, scripting). |
| iced version | **iced 0.14** (current). |
| Plotting | **`iced_plot` 0.5** (GPU-accelerated, iced-0.14-native) as the primary plot widget, with a **custom `iced::widget::Canvas` fallback** for panels it cannot express. Validated by a spike task before the rest of the plan commits. |
| Docking / workspace | **`pane_grid`, splits only** — no tabbed tile groups. New layout schema. |
| Migration | egui/eframe is **hard-replaced immediately**; no dual-UI coexistence. |

### Why iced_plot over plotters-iced

- `plotters-iced` 0.11 (Sept 2024) depends on `iced ^0.13`; adopting it would
  pin the whole app one major version behind, and it still provides only
  drawing (no pan/zoom/hover interaction).
- `iced_plot` 0.5 (Jun 2026) depends on `iced ^0.14`, is GPU-accelerated
  (suits high-rate telemetry redraw), and is a real *widget* with built-in
  interaction — closer to `egui_plot`'s model.
- Risk accepted: `iced_plot` is young (≈419 downloads, no docs.rs). The first
  plan task is a spike that de-risks it; anything it cannot express falls back
  to a custom Canvas widget.

## Scope

**Codebase:** ~19.8k LOC / 64 files. **19 files touch egui.**

**Reused untouched** (behind the `ChannelStore` trait and framework-free
types): `src/ingest/`, `src/store/`, `src/record/`, `src/config/` (except the
workspace-layout schema), `src/script/` engine (not its panel), the
`ChannelTree` model logic.

**Rewritten** (the egui files):
`app.rs` → `ui/app.rs` (the Elm loop), `workspace.rs`, every `viz/*` panel,
`data_source_window.rs`, `script/panel.rs`.

**Dependency changes:** drop `eframe`, `egui`, `egui_plot`, `egui_tiles`,
`egui_phosphor`; add `iced` 0.14 and `iced_plot` 0.5. `rfd` (native file
dialogs) stays — it is framework-agnostic. Phosphor icons are loaded via
iced's font-loading API.

## Architecture

### 1. Elm loop replaces immediate mode

The app becomes a single iced application:

- `State` — holds what `DataVisApp` holds today, nearly 1:1: `store`,
  `live_store`, `channels`, `registry`, `workspace`, `manager` (`SourceManager`),
  `record_handle`, `channel_tree` (+ `saved_channel_tree`), `pending_load`,
  `sidebar_visible`, `data_source_window_open`, `add_source_form`,
  `record_max_bytes`, `status`/`status_clear_at`, `mqtt_snapshot`, etc.
- `Message` — enum of every user/async event.
- `update(&mut State, Message) -> Task<Message>`.
- `view(&State) -> Element<Message>` — pure.
- `subscription(&State) -> Subscription<Message>` — drives repaint (§4) and
  bridges async work.

The adaptive-repaint counter `last_write_seq` moves out of the frame loop and
into the subscription logic.

### 2. Panel model: `VizPanel` redesigned for `view`

egui's `render(&mut self, ui)` (draw **and** handle input inline) splits into
iced's two phases:

```rust
trait VizPanel {
    fn title(&self) -> String;
    fn view(&self, store: &dyn ChannelStore) -> Element<'_, PanelMessage>; // pure
    fn update(&mut self, msg: PanelMessage);        // mouse/zoom/config mutations
    fn config_view(&self) -> Element<'_, PanelMessage>;
    fn bind_channel(&mut self, name: &str) -> bool; // drag-drop target
    fn serialize(&self) -> toml::Table;
}
```

- **Plot panels** (`waveform`, `spectrum`, `xy_scatter`): `view` returns an
  `iced_plot` widget fed the series; pan/zoom/hover come from the widget,
  surfaced as `PanelMessage`.
- **Canvas-fallback panels** (`gauge`, `numeric`, `status`, `log`,
  `state_graph`, `measure`): each a custom `iced::widget::Canvas` `Program`.
  These were never true plots. `state_graph`'s step rendering and drag-bind
  hit-testing live here if `iced_plot` cannot express them (the spike decides).
- `PanelRegistry` keeps its `panel_type → constructor` map and the
  `PanelEntry { panel_type, config: toml::Table }` contract unchanged.
- **Binding errors stay inline**: a panel with an unknown/wrong-type channel
  still constructs; its `view` returns an error `Element`. Same contract as
  today's `PanelCtor` `Err` path.

### 3. Workspace: `pane_grid`, splits only

- `Workspace` holds one `pane_grid::State<PaneId>` per named **screen**.
  Screens (today's tab strip) render as a custom top button row.
- Splits, resize, and drag-reorder come from `pane_grid`. No tabbed tile
  groups.
- **New `layout.toml` workspace schema**: the egui_tiles JSON blob per screen
  is replaced by a serde-serialized pane_grid layout — a split tree of ratios
  with leaf `PanelEntry`s. Round-trip (serialize → deserialize → serialize) is
  tested.
- **No migration** of old egui_tiles layouts: an unrecognized/old workspace
  section is ignored on load, a default grid is built, and a status note is
  shown. `[channels]`, `[sources]`, `[scripts]`, `[recording]` sections are
  framework-agnostic and preserved exactly.
- **Drag-to-bind**: a channel dragged from the sidebar onto a pane. Done via
  `pane_grid` drag events plus a drag-payload field in `State`; on drop it
  resolves to `panel.bind_channel(name)`.

### 4. Live data & repaint

egui free-ran at 60 fps (`vsync:false`) and throttled via `write_seq`. iced
redraws only on `Message`, so:

- A subscription emits `Tick` on `iced::time::every(…)`. Default ~60 fps while
  data flows; when `store.write_seq()` stops advancing (idle), back off to a
  slow heartbeat (~4 Hz). Same adaptive logic as today, relocated. Each `Tick`
  re-evaluates `view`, which re-reads the `ChannelStore`.
- No sample data is copied into the Elm state — panels read the shared
  `Arc<dyn ChannelStore>` at `view` time, exactly as today.

### 5. Replay, recording & timeline

- **Async MCAP load** (today a worker thread feeding a `crossbeam` receiver
  polled each frame) becomes an iced `Task::perform`, or a subscription that
  bridges the existing `crossbeam` receiver. On completion,
  `Message::RecordingLoaded(Arc<PlaybackStore>)` swaps `store`, saves
  `live_store`, rebuilds `channel_tree`, and re-resolves panel bindings —
  identical logic, relocated into `update`.
- **Timeline scrub**: a custom `Canvas` widget (playhead, envelope overview,
  drag-to-seek) emitting `Message::Seek(ns)`. Shown as a bottom bar only while
  a `PlaybackStore` is active.
- **Recording** (`start_recording`/`stop_recording`, `record_handle`,
  `record_max_bytes` auto-split) is backend code, reused verbatim; only the
  toolbar buttons are rebuilt.

### 6. Data-source window & dialogs

iced has no floating `Window` like egui. The Data Source panel renders as a
**modal overlay** (iced `stack`/modal pattern) toggled by
`data_source_window_open`. The `AddSourceForm` state, its `to_spec()`, and the
persist-to-config logic move over unchanged; only the widgets are rebuilt
(text inputs, kind picker, `rfd` file-pick buttons, per-row status dot + Stop).
Add-panel and add-screen dialogs use the same modal-overlay pattern.

## Testing strategy

iced's `view` is a pure function returning an `Element`, so today's
`renders_headless_without_panic` egui tests do not port directly. Replacement:

1. Keep/extend **logic-level** panel tests (binding, config parse, serialize
   round-trip, decimation, FFT) — these are framework-free.
2. Add **`update`-function tests**: feed a `Message`, assert resulting `State`.
3. **Workspace layout** serialize/deserialize round-trip tests on the new
   schema.

Per-task gate: `cargo build` + `cargo test` (both `--no-default-features` and
default/`scripting`, matching current CI). Because egui is hard-replaced, a
running-app smoke test becomes possible only once the shell + first panel land;
from then on, each task that changes a UI region ends with a manual `cargo run`
check.

## Plan ordering (risk-first)

1. **Spike** (throwaway example binary): live waveform via `iced_plot` fed from
   a real `LiveStore`, plus one hard case (linked zoom **or** drag-bind
   hit-test); measure perf at high sample rate. **Gate:** confirm `iced_plot`
   covers plot panels, or fall the specific panel back to Canvas. Deleted after.
2. **App shell**: replace `eframe` in `main.rs`; iced
   `State`/`Message`/`update`/`view`/`subscription` skeleton; menu/toolbar +
   status line; empty central area. App runs.
3. **Workspace**: `pane_grid` + screens + new layout schema + serialize
   round-trip tests.
4. **Sidebar**: channel tree + drag source.
5. **Panels** — one task each (waveform first as the `iced_plot` reference,
   then spectrum, xy_scatter, numeric, gauge, status, log, state_graph,
   measure), each ending in a runnable app.
6. **Drag-to-bind** wiring.
7. **Replay + timeline scrub**; **recording toolbar**.
8. **Data-source modal** + add-panel/add-screen dialogs.
9. **Scripting panel** (`script/panel.rs`).
10. **Cleanup**: delete dead egui glue; drop `eframe`/`egui*` from `Cargo.toml`;
    update `README`/help/docs; bump version.

Phosphor icons and `rfd` dialogs fold into whichever task first needs them, not
standalone tasks.

## Non-goals

- Preserving old egui_tiles `layout.toml` workspace layouts (channels/sources/
  scripts/recording sections *are* preserved).
- Tabbed tile groups in the workspace.
- Changing any data-backend behavior (ingest, store, record, config parsing,
  scripting engine).
