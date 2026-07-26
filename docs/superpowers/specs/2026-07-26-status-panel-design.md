# Status Panel Design

**Goal:** A new visualization panel that shows a channel's current discrete
state as a text badge, recoloring the whole badge when the state changes, with
user-configurable per-value colors and labels.

## Motivation

The existing panels leave a gap:

- `numeric` — big latest value, numeric-only, threshold coloring (value ≥ cutoff).
- `state_graph` — Grafana-style time bands for `Int`/`Bool`, per-value labels,
  fixed auto palette (no user color control), plotted over a time window.
- `log` — scrolling text lines.

None of them shows a single, glanceable "what state is this in right now"
readout for a `Text` channel, and none lets the user choose the color per state
value. This panel does exactly that.

## Scope

- Panel type name: **`status`** (chosen to avoid collision with `state_graph`).
- Accepted channel types: `Text`, `Int`, `Bool`.
- **Not** `Float` — continuous values do not form discrete states; use `gauge`
  or `numeric` for those.
- Single current-value display (no time window / trailing view).

## Value → match key

The current sample is reduced to a string match key:

| Sample     | Key          |
|------------|--------------|
| `Text(s)`  | `s`          |
| `Int(i)`   | `i.to_string()` e.g. `"2"` |
| `Bool(b)`  | `"true"` / `"false"` |
| `Float(_)` | none (type rejected before render) |

## Config model (`layout.toml`)

```toml
type = "status"
channel = "motor.state"
label = "Motor"                                      # optional panel label
states = [
  { match = "2", label = "FAULT", color = "#d62728" },
  { match = "1", label = "RUNNING", color = "#2ca02c" },
  { match = "0", label = "IDLE", color = "#7f7f7f" },
  { match = "true", label = "ON", color = "#2ca02c" },
]
```

- `channel` — channel name (string). Empty/unset → "Drop a channel here".
- `label` — optional custom panel label (reuses the shared `opt_label` /
  `serialize_label` helpers). Falls back to the channel name.
- `states` — array of entries, each:
  - `match` (string, required) — the raw value key to match, per the table above.
  - `label` (string, optional) — display text; defaults to `match` when omitted.
  - `color` (string, required) — `#rrggbb`; malformed entries are skipped on load
    (consistent with `ColorThresholds::from_config`).

An entry with no `match` or no parseable `color` is skipped. `states` is omitted
from serialization when empty.

## Matching and fallback

- The current value's key is looked up in `states` by **exact** string match.
- **Match found** → badge fill = entry `color`, badge text = entry `label`
  (or the raw value when `label` omitted).
- **No matching entry** → neutral gray badge (`Color32::from_gray(70)`), text =
  the raw value string.
- **No sample yet** (channel resolves but `latest` is `None`) → gray badge, text
  `"—"`.

First matching entry wins if two entries share a `match` key (config authoring
error; not enforced).

## Rendering

Mirrors the `gauge` badge style:

1. Resolve the value to show at the current time. In linked-zoom sync mode read
   at the shared window's end (`linked_window(ctx).map(|(_, end)| end)`), else
   `store.now_ns()` — same pattern as `numeric` and `gauge`.
2. `ui.allocate_exact_size` a badge rect (full available width, fixed height ~48
   px).
3. `painter.rect_filled(rect, 4.0, state_color)`.
4. Draw the label centered with `outlined_text`, foreground black/white chosen by
   luminance so it stays legible on any fill; outline is the opposite color.

### Shared helper refactor

`is_light(Color32) -> bool` (Rec. 601 luminance) currently lives private in
`gauge.rs`. Move it to `common.rs` as `pub(crate) fn is_light`, and have both
`gauge` and `status` call it. `outlined_text` is already shared in `common.rs`.
This is the only change to existing files besides panel registration. Gauge's
existing `light_bars_get_dark_text` test moves with it (or stays and imports the
relocated function) so behavior stays covered.

## Components

- **`src/viz/status.rs`** (new):
  - `pub const TYPE_NAME: &str = "status";`
  - `const ACCEPTED: &[SampleType] = &[Text, Int, Bool];`
  - `struct StateEntry { match_key: String, label: Option<String>, color: Color32 }`
  - `struct StateMap { entries: Vec<StateEntry> }` with:
    - `from_config(&toml::Table) -> StateMap`
    - `write_config(&self, &mut toml::Table)`
    - `lookup(&self, key: &str) -> Option<&StateEntry>`
    - `config_ui(&mut self, &mut egui::Ui)` — one row per entry
      `[match TextEdit][label TextEdit][color button][✕ remove]` plus a
      `+ state` button.
  - `struct StatusPanel { bound: Binding, label: Option<String>, states: StateMap }`
  - `fn ctor(&toml::Table, &ChannelRegistry) -> Result<Box<dyn VizPanel>>`
  - `fn sample_to_key(&Sample) -> Option<String>` (module fn, unit-tested).
  - `impl VizPanel` — `title`, `accepted_types`, `config_ui`, `render`,
    `serialize`, `drop_channel`, `refresh_bindings`.
- **`src/viz/mod.rs`** (modify): `pub mod status;` and
  `reg.register(status::TYPE_NAME, status::ctor);` in `with_builtins`.
- **`src/viz/common.rs`** (modify): add `pub(crate) fn is_light`.
- **`src/viz/gauge.rs`** (modify): drop local `is_light`, import from `common`.

`StatusPanel` follows the single-channel `Binding` pattern used by `gauge` /
`spectrum` (`bind`, `binding_error`, `binding_color`, `refresh_binding`).

## Error handling

- Unknown channel / wrong type → inline `binding_error` (red text), no badge.
- Malformed `states` entries → silently skipped at load (matches existing
  threshold parsing).
- Never panics on missing data — renders the "no sample" gray badge instead.

## Testing

Unit tests in `status.rs`:

- `sample_to_key` covers `Text` / `Int` / `Bool` → key, and `Float` → `None`.
- `StateMap::from_config` + `write_config` round-trip, including the optional
  `label` present and omitted, and that a malformed entry (missing color) is
  skipped.
- `StateMap::lookup` returns the matching entry and `None` for an unmapped key.
- `builds_serializes_round_trip` through `PanelRegistry` (matches the pattern in
  `gauge.rs`).
- `renders_headless_without_panic` for: valid `Text` channel with states, valid
  `Int` channel with states, unknown channel, a `Float` channel (rejected inline),
  and a resolved channel with no data.

`common.rs` keeps / gains a test for `is_light` (moved from gauge).

## Out of scope

- No time-window / historical band view (that is `state_graph`).
- No blink/animation on change.
- No regex or range matching — exact string keys only.
