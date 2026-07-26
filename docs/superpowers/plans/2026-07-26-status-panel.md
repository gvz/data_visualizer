# Status Panel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `status` visualization panel that shows a channel's current discrete state as a colored text badge, recoloring per a user-configured value→color map.

**Architecture:** A new single-channel panel in `src/viz/status.rs` following the existing `Binding` pattern (like `gauge`/`spectrum`). A small `StateMap` value type holds the value→(label,color) entries with config parse/serialize/lookup and its own editor UI. Rendering reuses the shared `outlined_text` helper and an `is_light` luminance check relocated from `gauge.rs` into `common.rs`.

**Tech Stack:** Rust, egui/eframe, toml. Crate name is `datavis`.

## Global Constraints

- Panel type name is exactly `status`.
- Accepted channel types: `Text`, `Int`, `Bool`. `Float` is rejected (renders inline error).
- Never add `Co-Authored-By`/self-attribution to commits.
- Binding problems (unknown channel, wrong type) must render an inline error, never return `Err` from the ctor; `Err` is for malformed config only.
- Malformed `states` entries are silently skipped on load (consistent with `ColorThresholds::from_config`).
- Run tests with `cargo test -p datavis`. Run a single test with `cargo test -p datavis <name>`.

---

### Task 1: Relocate `is_light` into `common.rs`

Move the private luminance helper out of `gauge.rs` so the new panel can share it. Behavior-preserving refactor; the existing gauge test keeps it covered.

**Files:**
- Modify: `src/viz/common.rs` (add `pub(crate) fn is_light`)
- Modify: `src/viz/gauge.rs:46-49` (remove local `fn is_light`; import from common)

**Interfaces:**
- Produces: `pub(crate) fn is_light(c: eframe::egui::Color32) -> bool` in `crate::viz::common`.

- [ ] **Step 1: Confirm the existing gauge test passes before the move**

Run: `cargo test -p datavis light_bars_get_dark_text`
Expected: PASS (test currently exercises the local `is_light` in `gauge.rs`).

- [ ] **Step 2: Add `is_light` to `common.rs`**

Add this function to `src/viz/common.rs` (place it just above `outlined_text`, around line 260):

```rust
/// Perceived luminance via Rec. 601 weights; true when light enough that black
/// text reads better than white on top of it.
pub(crate) fn is_light(c: Color32) -> bool {
    let l = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    l > 140.0
}
```

- [ ] **Step 3: Remove the local copy in `gauge.rs` and import the shared one**

In `src/viz/gauge.rs`, delete these lines (currently 45-49):

```rust
/// Perceived luminance (0..1) via Rec. 601 weights; picks readable text color.
fn is_light(c: Color32) -> bool {
    let l = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    l > 140.0
}
```

Then add `is_light` to the existing `use crate::viz::common::{...}` import block at the top of `gauge.rs` (the block currently importing `bind, binding_error, label_config_row, ...`). The `Color32` still used elsewhere in `gauge.rs` stays imported from `eframe::egui`.

- [ ] **Step 4: Verify the crate builds and gauge tests still pass**

Run: `cargo test -p datavis --lib viz::gauge`
Expected: PASS — `light_bars_get_dark_text` (now calling the relocated `is_light` via `use super::*`) and the other gauge tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/viz/common.rs src/viz/gauge.rs
git commit -m "refactor: move is_light luminance helper into viz::common"
```

---

### Task 2: `sample_to_key` and `StateMap` data model

Create `src/viz/status.rs` with the pure logic (no `VizPanel` impl yet): the sample→key reduction and the `StateMap` config type. Fully unit-tested. The module must be reachable, so also declare it in `mod.rs` (registration comes in Task 3).

**Files:**
- Create: `src/viz/status.rs`
- Modify: `src/viz/mod.rs:17` (add `pub mod status;`)

**Interfaces:**
- Consumes: `crate::viz::common::{hex_to_color, color_to_hex}`, `crate::types::Sample`.
- Produces:
  - `pub(crate) fn sample_to_key(s: &Sample) -> Option<String>`
  - `pub(crate) struct StateEntry { pub match_key: String, pub label: Option<String>, pub color: Color32 }` with `fn display(&self) -> &str`
  - `pub(crate) struct StateMap { pub entries: Vec<StateEntry> }` with `from_config(&toml::Table) -> StateMap`, `write_config(&self, &mut toml::Table)`, `lookup(&self, &str) -> Option<&StateEntry>`, `config_ui(&mut self, &mut egui::Ui)`.

- [ ] **Step 1: Declare the module**

In `src/viz/mod.rs`, add `pub mod status;` immediately after the existing `pub mod state_graph;` line (line 17), keeping the block ordered.

- [ ] **Step 2: Write the failing tests**

Create `src/viz/status.rs` with only the imports and the test module (implementation added next). The test module must compile against the interfaces above, so it references items that don't exist yet — that's the failing state.

```rust
use eframe::egui::Color32;

use crate::types::Sample;
use crate::viz::common::{color_to_hex, hex_to_color};

// (implementation added in Step 4)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_key_per_type() {
        assert_eq!(sample_to_key(&Sample::Text("RUN".into())), Some("RUN".to_string()));
        assert_eq!(sample_to_key(&Sample::Int(2)), Some("2".to_string()));
        assert_eq!(sample_to_key(&Sample::Bool(true)), Some("true".to_string()));
        assert_eq!(sample_to_key(&Sample::Bool(false)), Some("false".to_string()));
        assert_eq!(sample_to_key(&Sample::Float(1.5)), None);
    }

    #[test]
    fn statemap_lookup_matches_exact_key() {
        let cfg: toml::Table = toml::from_str(
            r#"
[[states]]
match = "2"
label = "FAULT"
color = "#d62728"

[[states]]
match = "1"
color = "#2ca02c"
"#,
        )
        .unwrap();
        let m = StateMap::from_config(&cfg);
        assert_eq!(m.entries.len(), 2);
        // Entry with a label displays the label.
        let fault = m.lookup("2").unwrap();
        assert_eq!(fault.display(), "FAULT");
        assert_eq!(fault.color, Color32::from_rgb(0xd6, 0x27, 0x28));
        // Entry without a label displays the raw key.
        assert_eq!(m.lookup("1").unwrap().display(), "1");
        // Unmapped key.
        assert!(m.lookup("0").is_none());
    }

    #[test]
    fn malformed_entry_is_skipped() {
        // Missing `color` → skipped; missing `match` → skipped; good one kept.
        let cfg: toml::Table = toml::from_str(
            r#"
[[states]]
match = "1"

[[states]]
color = "#ffffff"

[[states]]
match = "2"
color = "#000000"
"#,
        )
        .unwrap();
        let m = StateMap::from_config(&cfg);
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].match_key, "2");
    }

    #[test]
    fn config_round_trips() {
        let src: toml::Table = toml::from_str(
            r#"
[[states]]
match = "2"
label = "FAULT"
color = "#d62728"

[[states]]
match = "1"
color = "#2ca02c"
"#,
        )
        .unwrap();
        let m = StateMap::from_config(&src);
        let mut out = toml::Table::new();
        m.write_config(&mut out);
        // Reparsing the written config yields an equal map.
        let m2 = StateMap::from_config(&out);
        assert_eq!(m2.entries.len(), 2);
        assert_eq!(m2.entries[0].match_key, "2");
        assert_eq!(m2.entries[0].label.as_deref(), Some("FAULT"));
        assert_eq!(m2.entries[0].color, Color32::from_rgb(0xd6, 0x27, 0x28));
        assert_eq!(m2.entries[1].label, None);
    }

    #[test]
    fn empty_map_writes_nothing() {
        let m = StateMap::default();
        let mut out = toml::Table::new();
        m.write_config(&mut out);
        assert!(out.get("states").is_none());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p datavis --lib viz::status`
Expected: FAIL to compile — `sample_to_key`, `StateMap`, `StateEntry` not found.

- [ ] **Step 4: Write the implementation**

Insert this above the `#[cfg(test)]` module in `src/viz/status.rs`:

```rust
/// The string match key for a sample: `Text` as-is, `Int`/`Bool` stringified.
/// `Float` has no discrete key (the type is rejected before render).
pub(crate) fn sample_to_key(s: &Sample) -> Option<String> {
    match s {
        Sample::Text(t) => Some(t.clone()),
        Sample::Int(i) => Some(i.to_string()),
        Sample::Bool(b) => Some(b.to_string()),
        Sample::Float(_) => None,
    }
}

/// One configured state: a raw-value key, its badge color, and an optional
/// display label (falls back to the key).
pub(crate) struct StateEntry {
    pub match_key: String,
    pub label: Option<String>,
    pub color: Color32,
}

impl StateEntry {
    /// Text shown on the badge for this entry.
    pub(crate) fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.match_key)
    }
}

/// User-configured value→(label,color) map for the status badge.
#[derive(Default)]
pub(crate) struct StateMap {
    pub entries: Vec<StateEntry>,
}

impl StateMap {
    /// Parse the `states` array of `{ match, label?, color }`. Entries missing
    /// `match` or a parseable `color` are skipped.
    pub(crate) fn from_config(cfg: &toml::Table) -> Self {
        let entries = cfg
            .get("states")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let t = item.as_table()?;
                        let match_key = t.get("match")?.as_str()?.to_string();
                        let color = hex_to_color(t.get("color")?.as_str()?)?;
                        let label = t
                            .get("label")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        Some(StateEntry { match_key, label, color })
                    })
                    .collect()
            })
            .unwrap_or_default();
        StateMap { entries }
    }

    /// Write the `states` array; omitted entirely when empty.
    pub(crate) fn write_config(&self, t: &mut toml::Table) {
        if self.entries.is_empty() {
            return;
        }
        let arr = self
            .entries
            .iter()
            .map(|e| {
                let mut tt = toml::Table::new();
                tt.insert("match".to_string(), toml::Value::String(e.match_key.clone()));
                if let Some(l) = &e.label {
                    tt.insert("label".to_string(), toml::Value::String(l.clone()));
                }
                tt.insert("color".to_string(), toml::Value::String(color_to_hex(e.color)));
                toml::Value::Table(tt)
            })
            .collect();
        t.insert("states".to_string(), toml::Value::Array(arr));
    }

    /// First entry whose key matches `key` exactly.
    pub(crate) fn lookup(&self, key: &str) -> Option<&StateEntry> {
        self.entries.iter().find(|e| e.match_key == key)
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p datavis --lib viz::status`
Expected: PASS — all five tests. (`config_ui` is added in Task 3 alongside its egui use; not needed for these tests.)

- [ ] **Step 6: Commit**

```bash
git add src/viz/status.rs src/viz/mod.rs
git commit -m "feat: status panel StateMap value-color model"
```

---

### Task 3: `StatusPanel`, config UI, and registration

Add the `VizPanel` implementation, the `StateMap` editor UI, register the panel type, and cover it with a serialize round-trip plus headless render tests.

**Files:**
- Modify: `src/viz/status.rs` (add imports, `StateMap::config_ui`, `StatusPanel`, `ctor`, `impl VizPanel`, panel tests)
- Modify: `src/viz/mod.rs:71` (register the ctor in `with_builtins`)

**Interfaces:**
- Consumes (from Task 2): `sample_to_key`, `StateMap`, `StateEntry`.
- Consumes (from `crate::viz::common`): `bind, binding_error, is_light, label_config_row, opt_label, opt_str, outlined_text, refresh_binding, serialize_label, linked_window, Binding, RebindCtx`.
- Produces: `pub const TYPE_NAME: &str = "status";`, `pub fn ctor(&toml::Table, &ChannelRegistry) -> anyhow::Result<Box<dyn VizPanel>>`.

- [ ] **Step 1: Register the panel type**

In `src/viz/mod.rs`, inside `PanelRegistry::with_builtins`, add after the `state_graph` registration line (currently line 71):

```rust
        reg.register(status::TYPE_NAME, status::ctor);
```

- [ ] **Step 2: Expand the imports in `status.rs`**

Replace the top `use` block of `src/viz/status.rs` (currently just `Color32`, `Sample`, `color_to_hex`, `hex_to_color`) with:

```rust
use eframe::egui::{self, Color32, FontId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{Sample, SampleType};
use crate::viz::common::{
    bind, binding_error, color_to_hex, hex_to_color, is_light, label_config_row, linked_window,
    opt_label, opt_str, outlined_text, refresh_binding, serialize_label, Binding, RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "status";

const ACCEPTED: &[SampleType] = &[SampleType::Text, SampleType::Int, SampleType::Bool];

/// Fallback badge fill for a value with no configured state entry.
const UNMAPPED_COLOR: Color32 = Color32::from_gray(70);
```

- [ ] **Step 3: Write the failing panel tests**

Add these tests to the existing `#[cfg(test)] mod tests` in `status.rs` (alongside the Task 2 tests, inside the same module):

```rust
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;
    use eframe::egui;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."motor.state"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"

[channels."motor.mode"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"

[channels."valve.state"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"

[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
"#,
        )
        .unwrap()
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "status"
channel = "motor.state"

[[states]]
match = "2"
label = "FAULT"
color = "#d62728"

[[states]]
match = "1"
color = "#2ca02c""#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
        assert_eq!(p.title(), "motor.state");
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let motor = channels.id("motor.state").unwrap();
        let mode = channels.id("motor.mode").unwrap();
        store.write_numeric(motor, 1, NumericVal::Int(2));
        store.write_text(mode, 1, "RUNNING".to_string());
        let reg = PanelRegistry::with_builtins();
        // int with states, text channel, unknown channel, float (rejected),
        // and `valve.state` (accepted type, no sample written → no-data badge)
        // must all render without panic.
        for src in [
            r#"type = "status"
channel = "motor.state"

[[states]]
match = "2"
label = "FAULT"
color = "#d62728""#,
            r#"type = "status"
channel = "motor.mode""#,
            r#"type = "status"
channel = "does.not.exist""#,
            r#"type = "status"
channel = "demo.sine""#,
            r#"type = "status"
channel = "valve.state""#,
        ] {
            let e: PanelEntry = toml::from_str(src).unwrap();
            let mut p = reg.build(&e, &channels).unwrap();
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    p.render(ui, &store);
                    p.config_ui(ui);
                });
            });
        }
    }
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p datavis --lib viz::status`
Expected: FAIL to compile — `config_ui`, `StatusPanel`, `ctor` / `TYPE_NAME` registration not found.

- [ ] **Step 5: Add `StateMap::config_ui`**

Append this method to the existing `impl StateMap` block in `status.rs`:

```rust
    /// Editable rows: [match][label][color][remove], plus an add button.
    pub(crate) fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("states (value \u{2192} color):");
        let mut remove = None;
        for (i, e) in self.entries.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut e.match_key)
                        .desired_width(80.0)
                        .hint_text("value"),
                );
                let mut label = e.label.clone().unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut label)
                            .desired_width(100.0)
                            .hint_text("label"),
                    )
                    .changed()
                {
                    e.label = if label.trim().is_empty() { None } else { Some(label) };
                }
                ui.color_edit_button_srgba(&mut e.color);
                if ui.button("\u{2715}").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.entries.remove(i);
        }
        if ui.button("+ state").clicked() {
            self.entries.push(StateEntry {
                match_key: String::new(),
                label: None,
                color: Color32::GRAY,
            });
        }
    }
```

- [ ] **Step 6: Add the panel struct, ctor, and `VizPanel` impl**

Add this to `status.rs` (below the `impl StateMap` block, above the test module):

```rust
/// Single-value badge showing a channel's current discrete state, recolored per
/// the configured value→color map.
pub struct StatusPanel {
    bound: Binding,
    label: Option<String>,
    states: StateMap,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = opt_str(cfg, "channel");
    Ok(Box::new(StatusPanel {
        bound: bind(&name, reg, ACCEPTED),
        label: opt_label(cfg),
        states: StateMap::from_config(cfg),
    }))
}

impl VizPanel for StatusPanel {
    fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.bound.name)
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        label_config_row(ui, &mut self.label, &self.bound.name);
        ui.separator();
        self.states.config_ui(ui);
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if self.bound.name.is_empty() {
            ui.label(egui::RichText::new("Drop a channel here").weak());
            return;
        }
        if binding_error(ui, &self.bound, TYPE_NAME) {
            return;
        }
        let id = self.bound.id.expect("checked by binding_error");
        // In sync mode read the value at the shared zoom window's end, so the
        // badge matches the zoomed waveform's right edge; else the latest value.
        let at = linked_window(ui.ctx())
            .map(|(_, end)| end)
            .unwrap_or_else(|| store.now_ns());
        let sample = store.latest_at(id, at).map(|(_, s)| s);
        let key = sample.as_ref().and_then(sample_to_key);
        // Matched entry → its label+color; unmapped value → raw text on gray;
        // no sample → dash on gray.
        let (text, color) = match &key {
            Some(k) => match self.states.lookup(k) {
                Some(e) => (e.display().to_string(), e.color),
                None => (k.clone(), UNMAPPED_COLOR),
            },
            None => ("\u{2014}".to_string(), UNMAPPED_COLOR),
        };

        let desired = egui::vec2(ui.available_width().max(80.0), 48.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, color);
        // Contrast text against any fill: black on light, white on dark, outlined
        // with the opposite so it stays legible.
        let (fg, outline) = if is_light(color) {
            (Color32::BLACK, Color32::WHITE)
        } else {
            (Color32::WHITE, Color32::BLACK)
        };
        outlined_text(painter, rect.center(), &text, FontId::proportional(20.0), fg, outline);
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        serialize_label(&mut t, &self.label);
        self.states.write_config(&mut t);
        t
    }

    fn drop_channel(&mut self, name: &str, reg: &ChannelRegistry) {
        self.bound = bind(name, reg, ACCEPTED);
    }

    fn refresh_bindings(&mut self, ctx: &RebindCtx) {
        refresh_binding(&mut self.bound, ACCEPTED, ctx);
    }
}
```

- [ ] **Step 7: Run the status tests to verify they pass**

Run: `cargo test -p datavis --lib viz::status`
Expected: PASS — the Task 2 data-model tests plus `builds_serializes_round_trip` and `renders_headless_without_panic`.

- [ ] **Step 8: Run the full suite to confirm nothing regressed**

Run: `cargo test -p datavis`
Expected: PASS — all tests, including the `mod.rs` registry tests that now see a `status` type.

- [ ] **Step 9: Commit**

```bash
git add src/viz/status.rs src/viz/mod.rs
git commit -m "feat: status panel with configurable per-value state colors"
```

---

## Notes for the implementer

- The crate is `datavis`; the `src` directory is an additional working root, so panel modules live at `src/viz/`.
- Panels are registered only in `PanelRegistry::with_builtins`; there is no separate plugin manifest.
- `latest_at(id, end_ns)` returns `Option<(i64, Sample)>` and correctly yields `Sample::Text` for text channels — no special-casing needed to read the string.
- `hex_to_color` returns `None` on malformed input (used to skip bad entries); `color_to_hex` drops alpha. Both already live in `common.rs`.
