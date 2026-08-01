# Configurable Script Bindings — Design

**Date:** 2026-08-01

## Goal

Let one `*.py` script be reused as several independently-bound **instances**,
each reading its own input channels and publishing its own output channels,
without copying the file. Today a script hardcodes `INPUTS`/`OUTPUTS` as module
globals, so reuse means duplicating the `.py` and hand-editing strings, and two
copies would fight over the same output channel name.

## Terminology

- **Script** — a `*.py` file (referred to by its stem). Declares default
  `INPUTS`/`OUTPUTS` and a `@numba.njit compute(ts, vals)`. Still runnable as-is.
- **Instance** — a configured binding of a script: a unique `id`, the input
  channels to feed it, the output channels it publishes, and an enabled flag.
  One script → many instances.

## Decisions (locked in brainstorming)

1. **Output naming** — templated from inputs. Each `OUTPUTS[*].name` may contain
   placeholders expanded per instance. Multi-output scripts keep a list; every
   name templates independently.
2. **Output override** — an instance may fully override its outputs
   (name/type/unit per output); otherwise it inherits the file's `OUTPUTS`
   (with templates expanded).
3. **Config schema** — **instances only**. The old `[scripts].enabled` string
   list is removed and replaced by `[[scripts.instances]]` tables. The shipped
   default `config.toml` is migrated.
4. **Instance identity** — an explicit, required, unique `id` field. Keys panel
   display, status, and config round-trip.
5. **Panel** — full instance editing in the GUI: add/remove instances, edit
   input/output bindings.
6. **Bad binding** — skipped, with a per-instance error surfaced in the panel.
   Other instances still load; the app still starts.
7. **Input picker** — a searchable combobox: dropdown of existing channels plus
   free-text that fuzzy-filters them. The bound name must verify as an existing
   channel (no phantom bindings).
8. **Edit apply** — explicit **Apply** button. Edits stage in the panel, then
   commit and reload the instance on Apply.
9. **Persistence** — session-only. Edits live in memory; a separate **Save**
   action writes `[[scripts.instances]]` back to `config.toml`.

## Template syntax

Placeholders in an output `name`, expanded Rust-side when an instance is built:

- `{inN}` → the Nth resolved input's full channel name (e.g. `load/ch0`).
- `{inN.stem}` → the last `/`-separated segment of the Nth input (`ch0`).
  Recommended for output names — a `/` in a name nests it in the channel tree.

`N` is a 0-based index into the instance's resolved inputs. A name with no
placeholder is used literally (today's behavior). An unknown placeholder
(`{in5}` on a 1-input script, or an unrecognized form) is a **build error** —
the instance is skipped and surfaced as `Failed`.

Expansion applies uniformly to file-default names and explicit instance
overrides.

New helper (in `src/script/types.rs`):

```rust
/// Expand `{inN}` / `{inN.stem}` placeholders in an output-name template
/// against the instance's resolved input channel names.
pub fn expand_output_name(template: &str, inputs: &[String]) -> Result<String, String>;
```

## Config schema

```toml
[scripts]
dir = "scripts"
window_s = 10.0

[[scripts.instances]]
id     = "ch0_rms"        # required, unique
script = "sine_rms"       # required, file stem under `dir`
inputs = ["load/ch0"]     # optional; omitted → file INPUTS
enabled = true            # optional; default true
# outputs omitted → file OUTPUTS, templates expanded against `inputs`

[[scripts.instances]]
id      = "ch1_rms"
script  = "sine_rms"      # same file, reused
inputs  = ["load/ch1"]
outputs = [{ name = "scripts.ch1_rms", type = "float", unit = "" }]  # explicit
```

Two instances of `sine_rms` on `load/ch0` and `load/ch1` publish four distinct,
non-colliding channels from one file.

### Types (`src/script/config.rs`)

```rust
pub struct OutputBinding {
    pub name: String,   // may contain templates
    pub ty: String,     // "float" | "int" | "bool"
    pub unit: String,
}

pub struct ScriptInstance {
    pub id: String,
    pub script: String,
    pub inputs: Option<Vec<String>>,        // None → file INPUTS
    pub outputs: Option<Vec<OutputBinding>>,// None → file OUTPUTS
    pub enabled: bool,                       // default true
}

pub struct ScriptsConfig {
    pub dir: String,
    pub window_s: f64,
    pub instances: Vec<ScriptInstance>,      // replaces `enabled`
}
```

`ScriptsConfig::save` rewrites `[scripts].dir`, `window_s`, and the
`[[scripts.instances]]` array, preserving all other sections and comments
(same `toml_edit` approach as today).

## Binding resolution (engine, per instance)

`ScriptEngine::build_runner` takes a `&ScriptInstance` and:

1. Reads source from `dir/<instance.script>.py` and loads it → `ScriptMeta`
   (file `INPUTS`/`OUTPUTS`; output names may be templates).
2. **Inputs** = `instance.inputs` if present, else file `meta.inputs`.
   **Arity check**: resolved input count must equal the file's `INPUTS` count
   (that count defines the `compute` arity). Mismatch → `Failed`.
3. **Outputs** = `instance.outputs` (mapped to `OutputSpec`, type parsed via the
   existing `output_sample_type`) if present, else file `meta.outputs`.
4. Expand each output name's template against the resolved inputs.
5. Run the existing `validate_meta` (non-empty, unique output names, no collision
   with a non-script channel).
6. Register outputs and build a `ScriptRunner` **keyed by `instance.id`**
   (`ScriptRunner.name` becomes the instance id; source is read via
   `instance.script`).

Failures at any step produce `ScriptState::Failed(msg)` for that id; other
instances are unaffected.

## Engine changes (`src/script/mod.rs`)

- `ScriptEngine.enabled: Vec<String>` → `instances: Vec<ScriptInstance>`.
- Runners keyed by instance id.
- `ScriptCommand` becomes:
  ```rust
  pub enum ScriptCommand {
      Upsert(ScriptInstance), // add or replace-in-place (add / edit-Apply / enable)
      Remove(String),         // by id
  }
  ```
  Enable/disable is an `Upsert` carrying the new `enabled` flag. On `Upsert`,
  drop any existing runner + failed entry for that id, then rebuild if enabled.
  On `Remove`, drop the runner + failed entry.
- **Owned-output tracking bug to fix:** `script_outputs` currently only grows.
  Track owned output names **per instance id** (`HashMap<String, Vec<String>>`)
  so that on `Remove`/rebuild the instance's names are released. Otherwise
  re-applying an edited instance would see its own previous output as an
  external collision and fail. Registration stays idempotent-by-name
  (`ScriptRunner::new` already reuses the slot when the name exists).
- `ScriptStatus.name` carries the instance id.

## Peeking script metadata for the editor

The panel needs a chosen script's arity and default outputs to prefill the
editor, without paying numba compilation. Add a cheap metadata read that
extracts `INPUTS`/`OUTPUTS` **without** `warm_up`/compile:

```rust
// src/script/python.rs (behind the `scripting` feature; trait method on ScriptLoader)
fn peek_meta(&self, source: &str, name: &str) -> Result<ScriptMeta, String>;
```

Returns input count and default `OutputSpec`s (names as raw templates). Used
only by the GUI editor to prefill fields.

## Panel (`src/script/panel.rs`) — full editor

The panel owns staging UI state and emits committed commands. It receives:

- available script stems (`discover_scripts`),
- a `peek_meta` callback to fetch a script's arity + default outputs,
- the current channel-name list (for the input combobox + existence check),
- the shared per-instance status.

Per-instance editor row:

- **id** (editable when adding; label once created), **enabled** checkbox,
  **script** dropdown.
- **Inputs** — one searchable combobox per input slot (slot count from the
  selected script's `INPUTS` arity). Dropdown lists existing channels; typing
  fuzzy-filters them. The committed value must resolve to an existing channel;
  an unresolved entry blocks Apply with an inline message.
- **Outputs** — one row per output: `name` (text, templates allowed) /
  `type` dropdown / `unit` text. Prefilled from the script's file defaults when
  the script is chosen.
- **Apply** — validates staged fields, then emits `Upsert(instance)`.
- **Remove** — emits `Remove(id)`.
- Per-instance status line (`● running` / `○ waiting for <ch>` / `✗ <msg>`),
  as today.

Panel-level controls: **Add instance** (new id + script → staged row) and
**Save to config** (writes current instances to `config.toml`).

```rust
pub enum PanelCommand {
    Upsert(ScriptInstance),
    Remove(String),
    SaveConfig,
}
```

### Fuzzy search

A small subsequence/substring matcher ranks the existing channel names against
the typed query; the combobox shows the filtered, ranked list. No new
dependency — a short scoring helper in the panel module.

## Error handling summary

| Condition                                   | Result                                  |
|---------------------------------------------|-----------------------------------------|
| Missing `.py` file for `script`             | instance `Failed`, others load          |
| Input arity ≠ file `INPUTS` count           | instance `Failed`                       |
| Unknown output `type`                       | instance `Failed`                       |
| Unknown/out-of-range template placeholder   | instance `Failed`                       |
| Duplicate output name within instance       | instance `Failed` (via `validate_meta`) |
| Output collides with a non-script channel   | instance `Failed` (via `validate_meta`) |
| Bound input not yet registered at runtime   | runner `Waiting(<ch>)` (existing); GUI verifies existence at edit time |

## Migration

- Rewrite the shipped `config.toml` `[scripts]` section from `enabled = [...]`
  to `[[scripts.instances]]` tables that reproduce the current behavior.
- Update `scripts/sine_squared.py` and `scripts/sine_rms.py` output names to use
  `{in0.stem}` templates so the default config demonstrates reuse. They remain
  valid standalone scripts (a template with a bound default input still
  expands).

## Testing highlights

- Config: parse `[[scripts.instances]]`; `save` round-trip preserving other
  sections/comments; defaults (omitted `inputs`/`outputs`/`enabled`).
- `expand_output_name`: `{in0}`, `{in0.stem}`, multi-input `{in1...}`, literal
  passthrough, unknown-placeholder error.
- Binding resolution: input override, output override, arity mismatch → `Failed`.
- Collision: two instances declaring the same output name → second `Failed`.
- Owned-output release: remove/rebuild an instance, re-Apply does not self-collide.
- Reuse: two instances of one script on two inputs → four distinct channels.
- `peek_meta`: returns arity + default outputs without compiling.
- Panel: fuzzy filter ranks channels; unresolved input blocks Apply; commands
  emitted on Apply/Remove/Save.

## Non-goals

- Editing `.py` source in the GUI.
- Binding to channels that do not yet exist (verification requires existence).
- Per-instance `window_s` (stays global; possible future work).
```
