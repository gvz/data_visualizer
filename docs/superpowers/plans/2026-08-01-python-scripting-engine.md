# Python Scripting Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users write numba-compiled Python scripts that read channels, do math, and publish new channels at full speed, selectable from a GUI and persisted in `config.toml`.

**Architecture:** A `ScriptEngine` (implements the existing `DataSource` trait) runs one background thread on a ~60 Hz timer. A trait boundary (`ScriptLoader` / `CompiledScript`) separates the pure-Rust scheduler — which gathers input windows, dedups by timestamp, and writes outputs — from the PyO3 layer that loads modules, eagerly compiles `compute` with numba, and marshals numpy arrays. The channel registry becomes `Arc`-shared so the engine thread can register output channels at runtime, exactly as the MQTT drop path already does.

**Tech Stack:** Rust, PyO3 0.22 (`auto-initialize`), rust-numpy 0.22, numba/numpy (runtime), egui, toml/toml_edit, cargo-deb.

## Global Constraints

- numba is a **hard requirement**. If Python or `import numba` fails, the whole feature is *absent* (disabled with a visible warning), never degraded to pure Python.
- Python never runs on the UI thread. The engine ticks on a background thread at ~60 Hz (16 ms).
- `compute` must be a `@numba.njit` function, **eagerly compiled at load** so the first tick is warm.
- `compute` signature: `compute(ts, vals)` where `ts` is `UniTuple(int64[:], N)` and `vals` is `UniTuple(float64[:], N)`, `N = len(INPUTS)`, indexed in `INPUTS` order. Each input carries its own timestamps; the engine never aligns or resamples.
- `compute` returns one `(ts, vals)` array pair per `OUTPUTS` entry (bare pair for a single output, tuple of pairs for several).
- Output channel types: `float`, `int`, `bool` only. Text inputs/outputs are out of scope.
- Dedup: for each output channel, append only samples whose timestamp is newer than the last written for that channel.
- Empty input window (any input) → engine skips the call; script stays "waiting for data".
- `[scripts]` config: `dir` (default `"scripts"`), `enabled` (list of script stems), `window_s` (default `10.0`). Persist via `toml_edit`, preserving every other section and its comments.
- Scripts are discovered by scanning `dir` for `*.py`; enablement is separate and persisted.
- GUI: a panel lists available scripts with checkboxes and a per-script status line; toggling loads/unloads live and rewrites `config.toml`.
- Packaging: Linux `.deb` `depends = "$auto, python3, python3-numba, python3-numpy"`; Windows bundles a relocatable CPython with numba/numpy and sets `PYTHONHOME` at startup.
- All PyO3 code sits behind a default-on cargo feature `scripting`; with the feature off the engine compiles to a stub that reports "disabled" and pulls in no Python deps.
- Commit messages: no `Co-Authored-By` / self-attribution lines.

## File Structure

- `src/script/mod.rs` — module exports; `ScriptState`, `ScriptStatus`, `SharedStatus`, `ScriptCommand`; `ScriptEngine` implementing `DataSource`; the tick loop.
- `src/script/config.rs` — `ScriptsConfig { dir, enabled, window_s }`: parse from a shared `config.toml` string and persist via `toml_edit`.
- `src/script/types.rs` — `OutputSpec`, `ScriptMeta`, `InputWindow`, `OutputBatch`, the `CompiledScript` and `ScriptLoader` traits, `LoadedScript`, and `validate_meta`.
- `src/script/runner.rs` — `ScriptRunner`: per-script scheduler state; registers outputs, resolves inputs, runs one tick with dedup. Pure Rust, no Python.
- `src/script/python.rs` — `#[cfg(feature = "scripting")]` `probe_numba`, `PyScriptLoader`, `PyScript`. The only file that touches PyO3.
- `src/script/panel.rs` — `draw_script_panel`: egui list + checkboxes + status; returns toggle actions.
- `src/lib.rs` — add `pub mod script;`.
- `src/main.rs` — build the shared `Arc<ChannelRegistry>`, construct `ScriptEngine`, grab its status/command handles, spawn it, pass handles into `DataVisApp`; set `PYTHONHOME` for the bundled interpreter.
- `src/app.rs` — hold `Arc<ChannelRegistry>`, the script status/command handles and available-script list; draw the script panel; persist `[scripts]` on toggle.
- `Cargo.toml` — `scripting` feature, optional `pyo3`/`numpy` deps, `[package.metadata.deb]` `depends`.
- `flake.nix` — add `python3` + numba/numpy to the dev shell so the default build links libpython and tests can import numba.

---

### Task 1: `[scripts]` config parse and persistence

**Files:**
- Create: `src/script/config.rs`
- Create: `src/script/mod.rs` (initially just `pub mod config;`)
- Modify: `src/lib.rs` (add `pub mod script;`)

**Interfaces:**
- Produces: `ScriptsConfig { dir: String, enabled: Vec<String>, window_s: f64 }`; `ScriptsConfig::from_toml_str(&str) -> anyhow::Result<ScriptsConfig>`; `ScriptsConfig::save(&self, &Path) -> anyhow::Result<()>`; `Default` (dir `"scripts"`, empty enabled, window_s `10.0`).

- [ ] **Step 1: Add the module to the crate**

In `src/lib.rs`, add after `pub mod record;`:
```rust
pub mod script;
```
Create `src/script/mod.rs`:
```rust
pub mod config;
```

- [ ] **Step 2: Write failing tests for parsing**

Create `src/script/config.rs`:
```rust
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

/// The `[scripts]` section of the shared config.toml.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptsConfig {
    /// Directory holding `*.py` scripts, relative to config.toml.
    pub dir: String,
    /// Script stems (filename without `.py`) that are active.
    pub enabled: Vec<String>,
    /// Seconds of history handed to each script per tick.
    pub window_s: f64,
}

impl Default for ScriptsConfig {
    fn default() -> Self {
        Self { dir: "scripts".to_string(), enabled: Vec::new(), window_s: 10.0 }
    }
}

#[derive(Deserialize)]
struct DocWrapper {
    scripts: Option<RawScripts>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScripts {
    dir: Option<String>,
    #[serde(default)]
    enabled: Vec<String>,
    window_s: Option<f64>,
}

impl ScriptsConfig {
    /// Parse the `[scripts]` table out of a full config.toml. Absent section or
    /// absent keys fall back to the defaults.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let doc: DocWrapper = toml::from_str(s).context("parsing [scripts]")?;
        let def = ScriptsConfig::default();
        Ok(match doc.scripts {
            None => def,
            Some(raw) => ScriptsConfig {
                dir: raw.dir.unwrap_or(def.dir),
                enabled: raw.enabled,
                window_s: raw.window_s.unwrap_or(def.window_s),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_yields_defaults() {
        let c = ScriptsConfig::from_toml_str("default_window_s = 5.0\n").unwrap();
        assert_eq!(c, ScriptsConfig::default());
    }

    #[test]
    fn parses_all_fields() {
        let c = ScriptsConfig::from_toml_str(
            "[scripts]\ndir = \"s\"\nenabled = [\"a\", \"b\"]\nwindow_s = 2.5\n",
        )
        .unwrap();
        assert_eq!(c.dir, "s");
        assert_eq!(c.enabled, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(c.window_s, 2.5);
    }

    #[test]
    fn missing_keys_fall_back() {
        let c = ScriptsConfig::from_toml_str("[scripts]\nenabled = [\"x\"]\n").unwrap();
        assert_eq!(c.dir, "scripts");
        assert_eq!(c.window_s, 10.0);
        assert_eq!(c.enabled, vec!["x".to_string()]);
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test --lib script::config`
Expected: PASS (parsing is fully implemented above).

- [ ] **Step 4: Write a failing test for persistence**

Add to the `tests` module in `src/script/config.rs`:
```rust
    #[test]
    fn save_rewrites_scripts_preserving_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# top comment\ndefault_window_s = 5.0\n\n[channels.\"a\"]\ntype = \"float\"\n",
        )
        .unwrap();

        let cfg = ScriptsConfig {
            dir: "scripts".into(),
            enabled: vec!["accel_mag".into()],
            window_s: 4.0,
        };
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        // Round-trips through the parser.
        let reparsed = ScriptsConfig::from_toml_str(&text).unwrap();
        assert_eq!(reparsed, cfg);
        // Other sections and comments survive.
        assert!(text.contains("# top comment"));
        assert!(text.contains("[channels.\"a\"]"));
        assert!(text.contains("default_window_s = 5.0"));
    }
```

Run: `cargo test --lib script::config::tests::save_rewrites`
Expected: FAIL — `no method named save`.

- [ ] **Step 5: Implement `save`**

Add to `impl ScriptsConfig` in `src/script/config.rs`:
```rust
    /// Rewrite only the `[scripts]` keys in an existing config.toml, preserving
    /// every other section and its comments (same approach as
    /// `LayoutConfig::save`).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use toml_edit::{value, Array, DocumentMut};

        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc: DocumentMut =
            existing.parse().context("parsing existing config.toml")?;

        let mut arr = Array::new();
        for name in &self.enabled {
            arr.push(name.as_str());
        }
        doc["scripts"]["dir"] = value(self.dir.as_str());
        doc["scripts"]["enabled"] = value(arr);
        doc["scripts"]["window_s"] = value(self.window_s);

        std::fs::write(path, doc.to_string())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib script::config`
Expected: PASS (all four tests).

- [ ] **Step 7: Commit**

```bash
git add src/lib.rs src/script/mod.rs src/script/config.rs
git commit -m "feat(script): parse and persist the [scripts] config section"
```

---

### Task 2: Script domain types and metadata validation

**Files:**
- Create: `src/script/types.rs`
- Modify: `src/script/mod.rs` (add `pub mod types;`)

**Interfaces:**
- Consumes: `crate::types::SampleType`.
- Produces:
  - `OutputSpec { name: String, sample_type: SampleType, unit: String }`
  - `ScriptMeta { inputs: Vec<String>, outputs: Vec<OutputSpec> }`
  - `InputWindow { ts: Vec<i64>, vals: Vec<f64> }`
  - `OutputBatch { ts: Vec<i64>, vals: Vec<f64> }`
  - `trait CompiledScript: Send { fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String>; }`
  - `struct LoadedScript { pub meta: ScriptMeta, pub compiled: Box<dyn CompiledScript> }`
  - `trait ScriptLoader: Send { fn load(&self, source: &str, name: &str) -> Result<LoadedScript, String>; }`
  - `fn validate_meta(meta: &ScriptMeta, channel_exists: impl Fn(&str) -> bool) -> Result<(), String>`

- [ ] **Step 1: Add the module**

In `src/script/mod.rs` add:
```rust
pub mod types;
```

- [ ] **Step 2: Write failing tests for validation**

Create `src/script/types.rs`:
```rust
use crate::types::SampleType;

/// One channel a script publishes.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSpec {
    pub name: String,
    pub sample_type: SampleType,
    pub unit: String,
}

/// A script's self-declared bindings, read from its `INPUTS`/`OUTPUTS` globals.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptMeta {
    pub inputs: Vec<String>,
    pub outputs: Vec<OutputSpec>,
}

/// One input channel's window: parallel timestamp and value arrays.
#[derive(Debug, Clone, PartialEq)]
pub struct InputWindow {
    pub ts: Vec<i64>,
    pub vals: Vec<f64>,
}

/// One output channel's samples for a tick: parallel timestamp and value arrays.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputBatch {
    pub ts: Vec<i64>,
    pub vals: Vec<f64>,
}

/// A loaded, compiled script's callable. Abstracted so the scheduler is
/// testable without a Python interpreter.
pub trait CompiledScript: Send {
    /// Run one tick. `inputs` is in `INPUTS` order; the return is in `OUTPUTS`
    /// order, one batch per declared output.
    fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String>;
}

/// A script's metadata plus its compiled callable.
pub struct LoadedScript {
    pub meta: ScriptMeta,
    pub compiled: Box<dyn CompiledScript>,
}

/// Loads a script's source into metadata + a compiled callable. Implemented by
/// the PyO3 layer; faked in tests.
pub trait ScriptLoader: Send {
    fn load(&self, source: &str, name: &str) -> Result<LoadedScript, String>;
}

/// Validate a script's declared bindings before registering its channels.
/// `channel_exists` reports whether a name is already a channel in the registry
/// (used to reject output-name collisions with non-script channels).
pub fn validate_meta(
    meta: &ScriptMeta,
    channel_exists: impl Fn(&str) -> bool,
) -> Result<(), String> {
    if meta.inputs.is_empty() {
        return Err("INPUTS must list at least one channel".to_string());
    }
    if meta.outputs.is_empty() {
        return Err("OUTPUTS must declare at least one channel".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for out in &meta.outputs {
        if !seen.insert(out.name.as_str()) {
            return Err(format!("duplicate output channel '{}'", out.name));
        }
        if channel_exists(&out.name) {
            return Err(format!(
                "output '{}' collides with an existing channel",
                out.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> OutputSpec {
        OutputSpec { name: name.into(), sample_type: SampleType::Float, unit: String::new() }
    }

    #[test]
    fn accepts_valid_meta() {
        let meta = ScriptMeta { inputs: vec!["a".into()], outputs: vec![spec("out")] };
        assert!(validate_meta(&meta, |_| false).is_ok());
    }

    #[test]
    fn rejects_empty_inputs() {
        let meta = ScriptMeta { inputs: vec![], outputs: vec![spec("out")] };
        assert!(validate_meta(&meta, |_| false).is_err());
    }

    #[test]
    fn rejects_empty_outputs() {
        let meta = ScriptMeta { inputs: vec!["a".into()], outputs: vec![] };
        assert!(validate_meta(&meta, |_| false).is_err());
    }

    #[test]
    fn rejects_duplicate_output_names() {
        let meta =
            ScriptMeta { inputs: vec!["a".into()], outputs: vec![spec("o"), spec("o")] };
        assert!(validate_meta(&meta, |_| false).is_err());
    }

    #[test]
    fn rejects_collision_with_existing_channel() {
        let meta = ScriptMeta { inputs: vec!["a".into()], outputs: vec![spec("taken")] };
        let err = validate_meta(&meta, |n| n == "taken").unwrap_err();
        assert!(err.contains("collides"));
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --lib script::types`
Expected: PASS (all five tests).

- [ ] **Step 4: Commit**

```bash
git add src/script/mod.rs src/script/types.rs
git commit -m "feat(script): domain types, loader/compiled traits, meta validation"
```

---

### Task 3: Scheduler core (`ScriptRunner`) with a fake compiled script

**Files:**
- Create: `src/script/runner.rs`
- Modify: `src/script/mod.rs` (add `pub mod runner;` and re-export `ScriptState`)

**Interfaces:**
- Consumes: `ScriptMeta`, `OutputSpec`, `InputWindow`, `OutputBatch`, `CompiledScript` (Task 2); `crate::store::ChannelStore`; `crate::config::ChannelRegistry`; `crate::types::{ChannelId, NumericVal, SampleType, TimeWindow, ChannelSnapshot}`.
- Produces:
  - `enum ScriptState { Healthy, Waiting(String), Failed(String) }`
  - `struct ScriptRunner`
  - `ScriptRunner::new(name: String, meta: ScriptMeta, compiled: Box<dyn CompiledScript>, store: &dyn ChannelStore, registry: &ChannelRegistry) -> ScriptRunner` — registers outputs.
  - `ScriptRunner::tick(&mut self, store: &dyn ChannelStore, registry: &ChannelRegistry, window: TimeWindow)`
  - `ScriptRunner::name(&self) -> &str`, `ScriptRunner::state(&self) -> &ScriptState`

- [ ] **Step 1: Add the module**

In `src/script/mod.rs`:
```rust
pub mod runner;

pub use runner::ScriptState;
```

- [ ] **Step 2: Write a failing test: element-wise output writes and dedups**

Create `src/script/runner.rs`:
```rust
use crate::config::ChannelRegistry;
use crate::script::types::{CompiledScript, InputWindow, OutputBatch, OutputSpec, ScriptMeta};
use crate::store::ChannelStore;
use crate::types::{ChannelId, ChannelSnapshot, NumericVal, SampleType, TimeWindow};

/// Per-script health, surfaced in the GUI.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptState {
    Healthy,
    Waiting(String),
    Failed(String),
}

/// One running script: its compiled callable plus the bookkeeping the engine
/// needs to route channels and dedup output samples.
pub struct ScriptRunner {
    name: String,
    meta: ScriptMeta,
    compiled: Box<dyn CompiledScript>,
    /// Output channel ids, aligned to `meta.outputs`.
    output_ids: Vec<ChannelId>,
    /// Input channel ids, aligned to `meta.inputs`; `None` until resolved.
    input_ids: Vec<Option<ChannelId>>,
    /// Last timestamp written per output, aligned to `meta.outputs`.
    last_written: Vec<i64>,
    state: ScriptState,
}

/// Convert a numeric snapshot into parallel (ts, f64 vals). Text snapshots
/// (never a script input) yield empty arrays.
fn snapshot_to_f64(snap: ChannelSnapshot) -> (Vec<i64>, Vec<f64>) {
    match snap {
        ChannelSnapshot::Float { ts, vals } => (ts, vals),
        ChannelSnapshot::Int { ts, vals } => {
            let f = vals.into_iter().map(|v| v as f64).collect();
            (ts, f)
        }
        ChannelSnapshot::Bool { ts, vals } => {
            let f = vals.into_iter().map(|v| v as f64).collect();
            (ts, f)
        }
        ChannelSnapshot::Text { .. } => (Vec::new(), Vec::new()),
    }
}

/// Cast a computed f64 to the channel's declared numeric type.
fn cast_to(sample_type: SampleType, v: f64) -> NumericVal {
    match sample_type {
        SampleType::Float => NumericVal::Float(v),
        SampleType::Int => NumericVal::Int(v as i64),
        SampleType::Bool => NumericVal::Bool(v != 0.0),
        // Text outputs are rejected at load; treat defensively as float.
        SampleType::Text => NumericVal::Float(v),
    }
}

impl ScriptRunner {
    /// Register each declared output as a runtime channel (same lockstep append
    /// the MQTT drop path uses) and build the runner. Callers must have already
    /// run `validate_meta` so output names are unique and collision-free.
    pub fn new(
        name: String,
        meta: ScriptMeta,
        compiled: Box<dyn CompiledScript>,
        store: &dyn ChannelStore,
        registry: &ChannelRegistry,
    ) -> Self {
        let mut output_ids = Vec::with_capacity(meta.outputs.len());
        for out in &meta.outputs {
            let id = registry.add_dynamic(&out.name, &out.name, out.sample_type);
            store.add_channel(registry.meta(id).clone());
            output_ids.push(id);
        }
        let input_ids = vec![None; meta.inputs.len()];
        let last_written = vec![i64::MIN; meta.outputs.len()];
        Self { name, meta, compiled, output_ids, input_ids, last_written, state: ScriptState::Healthy }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn state(&self) -> &ScriptState {
        &self.state
    }

    /// Run one tick: resolve inputs, gather windows, call the compiled script,
    /// and append new output samples (dedup by timestamp).
    pub fn tick(
        &mut self,
        store: &dyn ChannelStore,
        registry: &ChannelRegistry,
        window: TimeWindow,
    ) {
        if let ScriptState::Failed(_) = self.state {
            return; // A failed script stays parked until reloaded.
        }

        // Resolve any inputs not yet bound (a later-registered channel, e.g.
        // another script's output, resolves on a subsequent tick).
        for (i, name) in self.meta.inputs.iter().enumerate() {
            if self.input_ids[i].is_none() {
                self.input_ids[i] = registry.id(name);
            }
        }
        if let Some(k) = self.input_ids.iter().position(|o| o.is_none()) {
            self.state = ScriptState::Waiting(self.meta.inputs[k].clone());
            return;
        }

        // Gather each input's window.
        let mut windows = Vec::with_capacity(self.input_ids.len());
        for id in self.input_ids.iter().map(|o| o.unwrap()) {
            let (ts, vals) = snapshot_to_f64(store.snapshot(id, window));
            windows.push(InputWindow { ts, vals });
        }
        if windows.iter().any(|w| w.ts.is_empty()) {
            self.state = ScriptState::Waiting("data".to_string());
            return;
        }

        // Run and publish.
        match self.compiled.run(&windows) {
            Ok(batches) => {
                if batches.len() != self.meta.outputs.len() {
                    self.state = ScriptState::Failed(format!(
                        "compute returned {} outputs, expected {}",
                        batches.len(),
                        self.meta.outputs.len()
                    ));
                    return;
                }
                for (i, batch) in batches.iter().enumerate() {
                    if batch.ts.len() != batch.vals.len() {
                        self.state = ScriptState::Failed(format!(
                            "output '{}': ts/vals length mismatch ({} vs {})",
                            self.meta.outputs[i].name,
                            batch.ts.len(),
                            batch.vals.len()
                        ));
                        return;
                    }
                    let id = self.output_ids[i];
                    let sty = self.meta.outputs[i].sample_type;
                    for (&t, &v) in batch.ts.iter().zip(&batch.vals) {
                        if t > self.last_written[i] {
                            store.write_numeric(id, t, cast_to(sty, v));
                            self.last_written[i] = t;
                        }
                    }
                }
                self.state = ScriptState::Healthy;
            }
            Err(e) => self.state = ScriptState::Failed(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LiveStore;
    use crate::types::Sample;

    /// A fake compiled script that runs a Rust closure — lets us test the
    /// scheduler with no Python.
    struct FakeScript<F: FnMut(&[InputWindow]) -> Result<Vec<OutputBatch>, String> + Send>(F);
    impl<F: FnMut(&[InputWindow]) -> Result<Vec<OutputBatch>, String> + Send> CompiledScript
        for FakeScript<F>
    {
        fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String> {
            (self.0)(inputs)
        }
    }

    fn registry_with_input() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            "[channels.\"in.a\"]\ntype = \"float\"\nmax_rate = 100\nhistory_s = 1.0\n",
        )
        .unwrap()
    }

    fn out(name: &str) -> OutputSpec {
        OutputSpec { name: name.into(), sample_type: SampleType::Float, unit: String::new() }
    }

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    #[test]
    fn registers_output_and_writes_element_wise() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 10, NumericVal::Float(2.0));
        store.write_numeric(in_id, 20, NumericVal::Float(3.0));

        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("in.a.double")] };
        // Element-wise: double each value, keep timestamps.
        let compiled = Box::new(FakeScript(|inp: &[InputWindow]| {
            let w = &inp[0];
            Ok(vec![OutputBatch {
                ts: w.ts.clone(),
                vals: w.vals.iter().map(|v| v * 2.0).collect(),
            }])
        }));
        let mut runner = ScriptRunner::new("dbl".into(), meta, compiled, &store, &reg);

        runner.tick(&store, &reg, ALL);
        assert_eq!(runner.state(), &ScriptState::Healthy);

        let out_id = reg.id("in.a.double").unwrap();
        match store.snapshot(out_id, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![10, 20]);
                assert_eq!(vals, vec![4.0, 6.0]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn dedups_overlapping_windows() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|inp: &[InputWindow]| {
            let w = &inp[0];
            Ok(vec![OutputBatch { ts: w.ts.clone(), vals: w.vals.clone() }])
        }));
        let mut runner = ScriptRunner::new("id".into(), meta, compiled, &store, &reg);
        let out_id = reg.id("o").unwrap();

        store.write_numeric(in_id, 1, NumericVal::Float(1.0));
        runner.tick(&store, &reg, ALL); // writes ts=1
        store.write_numeric(in_id, 2, NumericVal::Float(2.0));
        runner.tick(&store, &reg, ALL); // window is {1,2}; only ts=2 is new

        match store.snapshot(out_id, ALL) {
            ChannelSnapshot::Float { ts, .. } => assert_eq!(ts, vec![1, 2]),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn waits_for_unregistered_input() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let meta = ScriptMeta { inputs: vec!["not.there".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|_: &[InputWindow]| Ok(vec![])));
        let mut runner = ScriptRunner::new("w".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);
        assert_eq!(runner.state(), &ScriptState::Waiting("not.there".to_string()));
    }

    #[test]
    fn waits_when_input_has_no_data() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|_: &[InputWindow]| Ok(vec![])));
        let mut runner = ScriptRunner::new("w".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);
        assert_eq!(runner.state(), &ScriptState::Waiting("data".to_string()));
    }

    #[test]
    fn reduction_writes_single_sample_and_casts_int() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 10, NumericVal::Float(3.0));
        store.write_numeric(in_id, 20, NumericVal::Float(5.0));

        let meta = ScriptMeta {
            inputs: vec!["in.a".into()],
            outputs: vec![OutputSpec {
                name: "count".into(),
                sample_type: SampleType::Int,
                unit: String::new(),
            }],
        };
        // Reduction: one sample at the latest ts, value = count of samples.
        let compiled = Box::new(FakeScript(|inp: &[InputWindow]| {
            let w = &inp[0];
            Ok(vec![OutputBatch { ts: vec![*w.ts.last().unwrap()], vals: vec![w.ts.len() as f64] }])
        }));
        let mut runner = ScriptRunner::new("c".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);

        let out_id = reg.id("count").unwrap();
        assert_eq!(store.latest(out_id), Some((20, Sample::Int(2))));
    }

    #[test]
    fn wrong_output_count_fails_script() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 1, NumericVal::Float(1.0));
        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|_: &[InputWindow]| Ok(vec![]))); // 0 != 1
        let mut runner = ScriptRunner::new("f".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);
        assert!(matches!(runner.state(), ScriptState::Failed(_)));
    }

    #[test]
    fn runtime_error_fails_script() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 1, NumericVal::Float(1.0));
        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|_: &[InputWindow]| Err("boom".to_string())));
        let mut runner = ScriptRunner::new("f".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);
        assert_eq!(runner.state(), &ScriptState::Failed("boom".to_string()));
    }
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --lib script::runner`
Expected: PASS (all seven tests).

- [ ] **Step 4: Commit**

```bash
git add src/script/mod.rs src/script/runner.rs
git commit -m "feat(script): scheduler core with output registration and timestamp dedup"
```

---

### Task 4: Cargo feature, PyO3 dependency, and numba capability probe

**Files:**
- Modify: `Cargo.toml` (feature + optional deps)
- Modify: `flake.nix` (python3 + numba in the dev shell)
- Create: `src/script/python.rs`
- Modify: `src/script/mod.rs` (add `#[cfg(feature = "scripting")] pub mod python;`)

**Interfaces:**
- Produces: `#[cfg(feature = "scripting")] pub fn probe_numba() -> Result<(), String>` in `src/script/python.rs`.

- [ ] **Step 1: Add the feature and optional deps**

In `Cargo.toml`, after the `[dependencies]` block add:
```toml
[features]
default = ["scripting"]
scripting = ["dep:pyo3", "dep:numpy"]
```
Add to `[dependencies]`:
```toml
pyo3 = { version = "0.22", features = ["auto-initialize"], optional = true }
numpy = { version = "0.22", optional = true }
```

- [ ] **Step 2: Make python3 available to the build/tests**

In `flake.nix`, add a Python with numba to the dev shell's packages (exact attribute path depends on the existing shell definition; add alongside the current `buildInputs`/`packages`):
```nix
(python3.withPackages (ps: with ps; [ numba numpy ]))
```
This provides `libpython` for the PyO3 link and `numba`/`numpy` for runtime tests. Do **not** add any substituter — the flake's `extra-substituters = []` policy stands.

- [ ] **Step 3: Write the module with a failing (compile-checked) probe test**

Create `src/script/python.rs`:
```rust
//! The only file that touches PyO3. Compiled behind the `scripting` feature.

use pyo3::prelude::*;

/// Probe whether the numeric stack is importable. numba is the gate: because it
/// depends on numpy, a successful `import numba` proves the whole stack is
/// present. Returns the Python error text on failure.
pub fn probe_numba() -> Result<(), String> {
    Python::with_gil(|py| {
        py.import_bound("numba").map(|_| ()).map_err(|e| e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_a_result() {
        // In a numba-equipped environment this is Ok; elsewhere it is Err with a
        // message. Either way it must not panic.
        match probe_numba() {
            Ok(()) => {}
            Err(msg) => assert!(!msg.is_empty()),
        }
    }
}
```
In `src/script/mod.rs` add:
```rust
#[cfg(feature = "scripting")]
pub mod python;
```

- [ ] **Step 4: Run the probe test**

Run: `cargo test --features scripting --lib script::python`
Expected: PASS. (In the numba dev shell, `probe_numba()` returns `Ok`.)

- [ ] **Step 5: Verify the crate still builds with scripting off**

Run: `cargo build --no-default-features`
Expected: builds with no `pyo3`/`numpy` compiled in (the `python` module is absent under `#[cfg]`).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock flake.nix src/script/mod.rs src/script/python.rs
git commit -m "feat(script): scripting cargo feature, pyo3 dep, numba capability probe"
```

---

### Task 5: PyO3 loader — read bindings, verify `@njit`, eager-compile

**Files:**
- Modify: `src/script/python.rs`

**Interfaces:**
- Consumes: `ScriptMeta`, `OutputSpec`, `LoadedScript`, `ScriptLoader`, `CompiledScript` (Task 2); `crate::types::SampleType`.
- Produces:
  - `struct PyScript { compute: Py<PyAny>, n_outputs: usize }` (a stub `CompiledScript` here; `run` is completed in Task 6).
  - `struct PyScriptLoader;` implementing `ScriptLoader`.

- [ ] **Step 1: Write a failing test that loads and compiles a valid script**

Add to `src/script/python.rs`:
```rust
use crate::script::types::{
    CompiledScript, InputWindow, LoadedScript, OutputBatch, OutputSpec, ScriptLoader, ScriptMeta,
};
use crate::types::SampleType;

/// A compiled numba script held as a Python callable.
pub struct PyScript {
    compute: Py<PyAny>,
    n_outputs: usize,
}

/// Loads Python source through numba. The gate probe must have already passed.
pub struct PyScriptLoader;

/// Map a declared output `type` string to a numeric `SampleType`. Text is
/// rejected — script outputs are numeric only.
fn output_sample_type(ty: &str) -> Result<SampleType, String> {
    match ty {
        "float" => Ok(SampleType::Float),
        "int" => Ok(SampleType::Int),
        "bool" => Ok(SampleType::Bool),
        other => Err(format!("output type '{other}' is not one of float/int/bool")),
    }
}

impl ScriptLoader for PyScriptLoader {
    fn load(&self, source: &str, name: &str) -> Result<LoadedScript, String> {
        Python::with_gil(|py| {
            // Exec the module source in a fresh namespace.
            let module = PyModule::from_code_bound(py, source, &format!("{name}.py"), name)
                .map_err(|e| e.to_string())?;

            // INPUTS: list[str].
            let inputs: Vec<String> = module
                .getattr("INPUTS")
                .map_err(|_| "script is missing INPUTS".to_string())?
                .extract()
                .map_err(|e| format!("INPUTS must be a list of strings: {e}"))?;

            // OUTPUTS: list[dict] with name/type and optional unit.
            let outputs_obj = module
                .getattr("OUTPUTS")
                .map_err(|_| "script is missing OUTPUTS".to_string())?;
            let mut outputs = Vec::new();
            for item in outputs_obj.iter().map_err(|e| e.to_string())? {
                let item = item.map_err(|e| e.to_string())?;
                let out_name: String = item
                    .get_item("name")
                    .and_then(|v| v.extract())
                    .map_err(|_| "each OUTPUTS entry needs a string 'name'".to_string())?;
                let ty: String = item
                    .get_item("type")
                    .and_then(|v| v.extract())
                    .map_err(|_| "each OUTPUTS entry needs a string 'type'".to_string())?;
                let unit: String = item
                    .get_item("unit")
                    .and_then(|v| v.extract())
                    .unwrap_or_default();
                outputs.push(OutputSpec { name: out_name, sample_type: output_sample_type(&ty)?, unit });
            }

            // compute must be a numba dispatcher (has a `.compile` method).
            let compute = module
                .getattr("compute")
                .map_err(|_| "script is missing a compute function".to_string())?;
            if !compute.hasattr("compile").map_err(|e| e.to_string())? {
                return Err("compute must be decorated @numba.njit".to_string());
            }

            // Eagerly compile now: force numba to specialise compute by calling
            // it once with length-1 dummy tuples matching the input arity. This
            // compiles to native code at load, so the first real tick is warm.
            let n = inputs.len();
            warm_up(py, &compute, n).map_err(|e| format!("numba compile failed: {e}"))?;

            let n_outputs = outputs.len();
            let meta = ScriptMeta { inputs, outputs };
            let compiled: Box<dyn CompiledScript> =
                Box::new(PyScript { compute: compute.unbind(), n_outputs });
            Ok(LoadedScript { meta, compiled })
        })
    }
}

/// Call `compute` once with length-1 dummy `(ts, vals)` tuples to force numba
/// to compile the specialisation for this input arity.
fn warm_up(py: Python<'_>, compute: &Bound<'_, PyAny>, n: usize) -> PyResult<()> {
    use numpy::PyArray1;
    use pyo3::types::PyTuple;

    let ts_arrays: Vec<_> = (0..n).map(|_| PyArray1::from_slice_bound(py, &[0i64])).collect();
    let val_arrays: Vec<_> = (0..n).map(|_| PyArray1::from_slice_bound(py, &[0.0f64])).collect();
    let ts_tuple = PyTuple::new_bound(py, &ts_arrays);
    let vals_tuple = PyTuple::new_bound(py, &val_arrays);
    compute.call1((ts_tuple, vals_tuple))?;
    Ok(())
}

// Placeholder impl so the crate compiles; Task 6 replaces `run`.
impl CompiledScript for PyScript {
    fn run(&mut self, _inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String> {
        let _ = self.n_outputs;
        Err("PyScript::run not yet implemented".to_string())
    }
}
```

Add this test to the `tests` module:
```rust
    const ELEMENTWISE: &str = r#"
import numpy as np
import numba

INPUTS  = ["a", "b"]
OUTPUTS = [{"name": "sum", "type": "float", "unit": "x"}]

@numba.njit
def compute(ts, vals):
    return (ts[0], vals[0] + vals[1])
"#;

    fn skip_without_numba() -> bool {
        if probe_numba().is_err() {
            eprintln!("skipping: numba not available");
            true
        } else {
            false
        }
    }

    #[test]
    fn loads_meta_and_compiles() {
        if skip_without_numba() {
            return;
        }
        let loaded = PyScriptLoader.load(ELEMENTWISE, "elementwise").unwrap();
        assert_eq!(loaded.meta.inputs, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(loaded.meta.outputs.len(), 1);
        assert_eq!(loaded.meta.outputs[0].name, "sum");
        assert_eq!(loaded.meta.outputs[0].sample_type, SampleType::Float);
        assert_eq!(loaded.meta.outputs[0].unit, "x");
    }

    #[test]
    fn rejects_non_njit_compute() {
        if skip_without_numba() {
            return;
        }
        let src = "INPUTS=[\"a\"]\nOUTPUTS=[{\"name\":\"o\",\"type\":\"float\"}]\ndef compute(ts, vals):\n    return (ts[0], vals[0])\n";
        let err = PyScriptLoader.load(src, "plain").unwrap_err();
        assert!(err.contains("numba.njit"), "got: {err}");
    }
```

- [ ] **Step 2: Run the loader tests**

Run: `cargo test --features scripting --lib script::python`
Expected: PASS (in the numba dev shell). If numba is unavailable the numba-gated tests print "skipping" and pass.

- [ ] **Step 3: Commit**

```bash
git add src/script/python.rs
git commit -m "feat(script): PyO3 loader reads bindings, verifies njit, eager-compiles"
```

---

### Task 6: PyO3 `PyScript::run` — marshal arrays, call, parse output pairs

**Files:**
- Modify: `src/script/python.rs`

**Interfaces:**
- Consumes: `InputWindow`, `OutputBatch` (Task 2). Replaces the placeholder `CompiledScript for PyScript`.
- Produces: a working `PyScript::run` that returns `Vec<OutputBatch>` in `OUTPUTS` order.

- [ ] **Step 1: Write a failing round-trip test**

Add to the `tests` module in `src/script/python.rs`:
```rust
    #[test]
    fn runs_elementwise_and_returns_pairs() {
        if skip_without_numba() {
            return;
        }
        let mut loaded = PyScriptLoader.load(ELEMENTWISE, "elementwise").unwrap();
        let inputs = vec![
            InputWindow { ts: vec![1, 2], vals: vec![10.0, 20.0] },
            InputWindow { ts: vec![1, 2], vals: vec![1.0, 2.0] },
        ];
        let out = loaded.compiled.run(&inputs).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ts, vec![1, 2]);
        assert_eq!(out[0].vals, vec![11.0, 22.0]);
    }

    const REDUCTION: &str = r#"
import numpy as np
import numba

INPUTS  = ["a"]
OUTPUTS = [{"name": "rms", "type": "float"}]

@numba.njit
def compute(ts, vals):
    v = vals[0]
    return (ts[0][-1:], np.array([np.sqrt(np.mean(v**2))]))
"#;

    #[test]
    fn runs_reduction_single_sample() {
        if skip_without_numba() {
            return;
        }
        let mut loaded = PyScriptLoader.load(REDUCTION, "reduction").unwrap();
        let inputs = vec![InputWindow { ts: vec![1, 2], vals: vec![3.0, 4.0] }];
        let out = loaded.compiled.run(&inputs).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ts, vec![2]);
        assert_eq!(out[0].vals.len(), 1);
        assert!((out[0].vals[0] - (12.5f64).sqrt()).abs() < 1e-9);
    }
```

- [ ] **Step 2: Replace the placeholder `run`**

Replace the placeholder `impl CompiledScript for PyScript` in `src/script/python.rs` with:
```rust
impl CompiledScript for PyScript {
    fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String> {
        use numpy::{PyArray1, PyArrayMethods};
        use pyo3::types::PyTuple;

        Python::with_gil(|py| {
            // Build ts and vals tuples of numpy arrays, in INPUTS order.
            let ts_arrays: Vec<_> =
                inputs.iter().map(|w| PyArray1::from_slice_bound(py, &w.ts)).collect();
            let val_arrays: Vec<_> =
                inputs.iter().map(|w| PyArray1::from_slice_bound(py, &w.vals)).collect();
            let ts_tuple = PyTuple::new_bound(py, &ts_arrays);
            let vals_tuple = PyTuple::new_bound(py, &val_arrays);

            let result = self
                .compute
                .bind(py)
                .call1((ts_tuple, vals_tuple))
                .map_err(|e| e.to_string())?;

            // Normalise the return into a list of (ts, vals) pairs. A single
            // output may be returned as a bare 2-tuple; several as a tuple of
            // pairs. Distinguish by inspecting the first element.
            let pairs: Vec<Bound<'_, PyAny>> = if self.n_outputs == 1 {
                // Could be (ts, vals) directly, or ((ts, vals),) — handle both.
                let tup = result.downcast::<PyTuple>().map_err(|_| {
                    "compute must return a (ts, vals) tuple".to_string()
                })?;
                if tup.len() == 2 && tup.get_item(0).map_or(false, |x| is_array(&x)) {
                    vec![result.clone()]
                } else {
                    tup.iter().collect()
                }
            } else {
                let tup = result.downcast::<PyTuple>().map_err(|_| {
                    "compute must return a tuple of (ts, vals) pairs".to_string()
                })?;
                tup.iter().collect()
            };

            if pairs.len() != self.n_outputs {
                return Err(format!(
                    "compute returned {} outputs, expected {}",
                    pairs.len(),
                    self.n_outputs
                ));
            }

            let mut batches = Vec::with_capacity(pairs.len());
            for pair in pairs {
                let pair = pair
                    .downcast::<PyTuple>()
                    .map_err(|_| "each output must be a (ts, vals) tuple".to_string())?;
                if pair.len() != 2 {
                    return Err("each output must be a (ts, vals) pair".to_string());
                }
                let ts = extract_i64(&pair.get_item(0).map_err(|e| e.to_string())?)?;
                let vals = extract_f64(&pair.get_item(1).map_err(|e| e.to_string())?)?;
                batches.push(OutputBatch { ts, vals });
            }
            Ok(batches)
        })
    }
}

/// True if the object is a numpy ndarray (used to disambiguate the single-output
/// bare-pair return from a tuple-of-pairs return).
fn is_array(obj: &Bound<'_, PyAny>) -> bool {
    obj.hasattr("dtype").unwrap_or(false) && obj.hasattr("shape").unwrap_or(false)
}

/// Extract an int64 array, coercing a float array via truncation.
fn extract_i64(obj: &Bound<'_, PyAny>) -> Result<Vec<i64>, String> {
    use numpy::{PyArray1, PyArrayMethods};
    if let Ok(a) = obj.downcast::<PyArray1<i64>>() {
        return Ok(a.to_vec().map_err(|e| e.to_string())?);
    }
    if let Ok(a) = obj.downcast::<PyArray1<f64>>() {
        return Ok(a.to_vec().map_err(|e| e.to_string())?.into_iter().map(|v| v as i64).collect());
    }
    Err("output ts must be an int64 or float64 array".to_string())
}

/// Extract a float64 array, widening an int64 array.
fn extract_f64(obj: &Bound<'_, PyAny>) -> Result<Vec<f64>, String> {
    use numpy::{PyArray1, PyArrayMethods};
    if let Ok(a) = obj.downcast::<PyArray1<f64>>() {
        return Ok(a.to_vec().map_err(|e| e.to_string())?);
    }
    if let Ok(a) = obj.downcast::<PyArray1<i64>>() {
        return Ok(a.to_vec().map_err(|e| e.to_string())?.into_iter().map(|v| v as f64).collect());
    }
    Err("output vals must be a float64 or int64 array".to_string())
}
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --features scripting --lib script::python`
Expected: PASS — both `runs_elementwise_and_returns_pairs` and `runs_reduction_single_sample` (in the numba dev shell).

- [ ] **Step 4: Commit**

```bash
git add src/script/python.rs
git commit -m "feat(script): PyScript::run marshals numpy tuples and parses output pairs"
```

---

### Task 7: `ScriptEngine` as a `DataSource` with the background tick loop

**Files:**
- Modify: `src/script/mod.rs`

**Interfaces:**
- Consumes: `ScriptRunner`, `ScriptState` (Task 3); `ScriptLoader`, `validate_meta` (Task 2); `crate::config::ChannelRegistry`; `crate::store::ChannelStore`; `crate::ingest::{DataSource, SourceHandle, CONNECTING, LIVE, TIMEOUT}`; `crate::types::TimeWindow`.
- Produces:
  - `struct ScriptStatus { pub name: String, pub state: ScriptState }`
  - `type SharedStatus = Arc<Mutex<Vec<ScriptStatus>>>`
  - `enum ScriptCommand { Enable(String), Disable(String) }`
  - `struct ScriptEngine`
  - `ScriptEngine::new(dir: PathBuf, enabled: Vec<String>, window_s: f64, loader: Box<dyn ScriptLoader>, registry: Arc<ChannelRegistry>, probe: Box<dyn Fn() -> Result<(), String> + Send>) -> ScriptEngine`
  - `ScriptEngine::status(&self) -> SharedStatus`, `ScriptEngine::commands(&self) -> Sender<ScriptCommand>`, `ScriptEngine::disabled_reason(&self) -> Arc<Mutex<Option<String>>>`
  - `impl DataSource for ScriptEngine` (name `"scripts"`).

- [ ] **Step 1: Write the failing test using a fake loader (no Python)**

This test references the not-yet-defined engine API, so it fails to compile
(RED) until Step 2 implements it. Add to `src/script/mod.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::types::{
        CompiledScript, InputWindow, LoadedScript, OutputBatch, OutputSpec, ScriptMeta,
    };
    use crate::store::LiveStore;
    use crate::types::{ChannelSnapshot, NumericVal, SampleType};

    struct DoublerLoader;
    struct Doubler;
    impl CompiledScript for Doubler {
        fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String> {
            let w = &inputs[0];
            Ok(vec![OutputBatch { ts: w.ts.clone(), vals: w.vals.iter().map(|v| v * 2.0).collect() }])
        }
    }
    impl ScriptLoader for DoublerLoader {
        fn load(&self, _source: &str, _name: &str) -> Result<LoadedScript, String> {
            Ok(LoadedScript {
                meta: ScriptMeta {
                    inputs: vec!["in.a".into()],
                    outputs: vec![OutputSpec {
                        name: "in.a.double".into(),
                        sample_type: SampleType::Float,
                        unit: String::new(),
                    }],
                },
                compiled: Box::new(Doubler),
            })
        }
    }

    #[test]
    fn engine_ticks_and_writes_outputs() {
        let reg = Arc::new(
            ChannelRegistry::from_toml_str(
                "[channels.\"in.a\"]\ntype = \"float\"\nmax_rate = 100\nhistory_s = 1.0\n",
            )
            .unwrap(),
        );
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 10, NumericVal::Float(2.0));

        // A temp dir with a placeholder file (the fake loader ignores contents).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dbl.py"), "# fake").unwrap();

        let engine = ScriptEngine::new(
            dir.path().to_path_buf(),
            vec!["dbl".into()],
            10.0,
            Box::new(DoublerLoader),
            reg.clone(),
            Box::new(|| Ok(())),
        );
        let status = engine.status();
        let handle = Box::new(engine).spawn(store.clone());
        assert_eq!(handle.name, "scripts");

        // Give the loop a few ticks to load and run.
        let out_id = loop_until(|| reg.id("in.a.double"), 2000);
        let deadline = std::time::Instant::now() + Duration::from_millis(2000);
        loop {
            if let ChannelSnapshot::Float { vals, .. } = store.snapshot(out_id, super::TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX }) {
                if vals == vec![4.0] {
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "output never written");
            std::thread::sleep(Duration::from_millis(20));
        }
        let s = status.lock().unwrap();
        assert_eq!(s.iter().find(|x| x.name == "dbl").map(|x| &x.state), Some(&ScriptState::Healthy));
    }

    fn loop_until<T>(mut f: impl FnMut() -> Option<T>, ms: u64) -> T {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(std::time::Instant::now() < deadline, "condition never met");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
```

- [ ] **Step 2: Implement the full engine (types, state, loop, `DataSource`)**

Add to `src/script/mod.rs`, above the `#[cfg(test)] mod tests`:
```rust
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use crate::config::ChannelRegistry;
use crate::ingest::{DataSource, SourceHandle, CONNECTING, LIVE, TIMEOUT};
use crate::script::runner::{ScriptRunner, ScriptState};
use crate::script::types::{validate_meta, ScriptLoader};
use crate::store::ChannelStore;
use crate::types::TimeWindow;

/// One script's status for the GUI.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptStatus {
    pub name: String,
    pub state: ScriptState,
}

/// Shared per-script status list, updated by the engine thread each tick.
pub type SharedStatus = Arc<Mutex<Vec<ScriptStatus>>>;

/// GUI → engine live control.
pub enum ScriptCommand {
    Enable(String),
    Disable(String),
}

/// Background engine that loads, compiles, and ticks scripts.
pub struct ScriptEngine {
    dir: PathBuf,
    enabled: Vec<String>,
    window_s: f64,
    loader: Box<dyn ScriptLoader>,
    registry: Arc<ChannelRegistry>,
    status: SharedStatus,
    commands: (Sender<ScriptCommand>, Receiver<ScriptCommand>),
    disabled: Arc<Mutex<Option<String>>>,
    /// Output names this engine has registered, to distinguish a re-enable
    /// (reuse the slot) from a real collision with a non-script channel.
    script_outputs: Arc<Mutex<HashSet<String>>>,
    /// Capability probe run before ticking. Real engine uses numba; tests pass
    /// `|| Ok(())` or `|| Err(..)`.
    probe: Box<dyn Fn() -> Result<(), String> + Send>,
}

impl ScriptEngine {
    pub fn new(
        dir: PathBuf,
        enabled: Vec<String>,
        window_s: f64,
        loader: Box<dyn ScriptLoader>,
        registry: Arc<ChannelRegistry>,
        probe: Box<dyn Fn() -> Result<(), String> + Send>,
    ) -> Self {
        Self {
            dir,
            enabled,
            window_s,
            loader,
            registry,
            status: Arc::new(Mutex::new(Vec::new())),
            commands: crossbeam_channel::unbounded(),
            disabled: Arc::new(Mutex::new(None)),
            script_outputs: Arc::new(Mutex::new(HashSet::new())),
            probe,
        }
    }

    pub fn status(&self) -> SharedStatus {
        self.status.clone()
    }

    pub fn commands(&self) -> Sender<ScriptCommand> {
        self.commands.0.clone()
    }

    pub fn disabled_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.disabled.clone()
    }

    /// Read a script's source from `dir/<name>.py`.
    fn read_source(&self, name: &str) -> Result<String, String> {
        let path = self.dir.join(format!("{name}.py"));
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))
    }

    /// Load, validate, register, and build a runner for one script.
    fn build_runner(
        &self,
        name: &str,
        store: &dyn ChannelStore,
    ) -> Result<ScriptRunner, ScriptStatus> {
        let fail = |e: String| ScriptStatus { name: name.into(), state: ScriptState::Failed(e) };
        let source = self.read_source(name).map_err(&fail)?;
        let loaded = self.loader.load(&source, name).map_err(&fail)?;

        // Collision check: an output name already known and NOT one of ours is a
        // real clash with a non-script channel.
        let mut owned = self.script_outputs.lock().unwrap();
        let exists = |n: &str| self.registry.id(n).is_some() && !owned.contains(n);
        validate_meta(&loaded.meta, exists).map_err(&fail)?;
        for o in &loaded.meta.outputs {
            owned.insert(o.name.clone());
        }
        drop(owned);

        Ok(ScriptRunner::new(
            name.to_string(),
            loaded.meta,
            loaded.compiled,
            store,
            &self.registry,
        ))
    }

    /// Publish the current runners' states plus any load failures into the
    /// shared status list the GUI reads.
    fn publish_status(status: &SharedStatus, runners: &[ScriptRunner], failed: &[ScriptStatus]) {
        let mut out: Vec<ScriptStatus> = runners
            .iter()
            .map(|r| ScriptStatus { name: r.name().to_string(), state: r.state().clone() })
            .collect();
        out.extend_from_slice(failed);
        *status.lock().unwrap() = out;
    }

    fn run_loop(self, store: Arc<dyn ChannelStore>, conn_state: Arc<AtomicU8>) {
        // Capability gate: without numba the whole feature is disabled.
        if let Err(e) = (self.probe)() {
            *self.disabled.lock().unwrap() = Some(format!("scripting unavailable: {e}"));
            conn_state.store(TIMEOUT, Ordering::Relaxed);
            return;
        }

        let mut runners: Vec<ScriptRunner> = Vec::new();
        // Scripts that failed to load — kept for the GUI as Failed entries.
        let mut failed: Vec<ScriptStatus> = Vec::new();

        let load = |name: &str, runners: &mut Vec<ScriptRunner>, failed: &mut Vec<ScriptStatus>| {
            if runners.iter().any(|r| r.name() == name) {
                return; // already loaded
            }
            failed.retain(|f| f.name != name);
            match self.build_runner(name, store.as_ref()) {
                Ok(runner) => runners.push(runner),
                Err(status) => failed.push(status),
            }
        };

        for name in self.enabled.clone() {
            load(&name, &mut runners, &mut failed);
        }
        conn_state.store(LIVE, Ordering::Relaxed);

        let tick = Duration::from_millis(16);
        loop {
            // Drain one command, blocking up to a tick so toggles are prompt.
            match self.commands.1.recv_timeout(tick) {
                Ok(ScriptCommand::Enable(name)) => load(&name, &mut runners, &mut failed),
                Ok(ScriptCommand::Disable(name)) => {
                    runners.retain(|r| r.name() != name);
                    failed.retain(|f| f.name != name);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }

            let now = store.now_ns();
            let window = TimeWindow::last((self.window_s * 1e9) as i64, now);
            for r in &mut runners {
                r.tick(store.as_ref(), &self.registry, window);
            }
            Self::publish_status(&self.status, &runners, &failed);
        }
    }
}

impl DataSource for ScriptEngine {
    fn name(&self) -> &str {
        "scripts"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));
        let record_sender = Arc::new(Mutex::new(None));
        let state_for_thread = conn_state.clone();
        let engine = *self;
        std::thread::spawn(move || engine.run_loop(store, state_for_thread));
        SourceHandle {
            name: "scripts".to_string(),
            conn_state,
            record_sender,
            discovery: None,
            schema_bytes: None,
        }
    }
}
```

> The `load` closure borrows `self` immutably and takes `runners`/`failed` as
> `&mut` parameters, so it never conflicts with the loop's other shared uses of
> `self`. It is a plain closure (not stored), created once and reused.


- [ ] **Step 3: Run the engine test**

Run: `cargo test --lib script::tests::engine_ticks_and_writes_outputs`
Expected: PASS. The fake loader/probe means no Python is needed.

- [ ] **Step 4: Add a disabled-engine test**

Add to the `tests` module:
```rust
    #[test]
    fn failed_probe_disables_engine() {
        let reg = Arc::new(ChannelRegistry::from_toml_str("default_window_s = 5.0\n").unwrap());
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let dir = tempfile::tempdir().unwrap();
        let engine = ScriptEngine::new(
            dir.path().to_path_buf(),
            vec![],
            10.0,
            Box::new(DoublerLoader),
            reg,
            Box::new(|| Err("no numba".into())),
        );
        let disabled = engine.disabled_reason();
        let _ = Box::new(engine).spawn(store);
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        loop {
            if disabled.lock().unwrap().is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "engine never reported disabled");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(disabled.lock().unwrap().as_ref().unwrap().contains("no numba"));
    }
```

Run: `cargo test --lib script::tests::failed_probe_disables_engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/script/mod.rs
git commit -m "feat(script): ScriptEngine DataSource with background tick loop and live toggles"
```

---

### Task 8: Wire the engine into the app; GUI script panel; persist on toggle

**Files:**
- Modify: `src/main.rs` (shared `Arc<ChannelRegistry>`, construct + spawn engine, pass handles)
- Modify: `src/app.rs` (hold registry Arc + script handles + available list; draw panel; persist)
- Create: `src/script/panel.rs`
- Modify: `src/script/mod.rs` (add `pub mod panel;`)

**Interfaces:**
- Consumes: `ScriptEngine`, `SharedStatus`, `ScriptCommand`, `ScriptState` (Task 7); `ScriptsConfig` (Task 1); `PyScriptLoader`, `probe_numba` (Tasks 4–6).
- Produces: `draw_script_panel(ui: &mut egui::Ui, available: &[String], enabled: &[String], status: &SharedStatus, disabled: &Option<String>) -> Vec<PanelToggle>` where `PanelToggle { name: String, enable: bool }`.

- [ ] **Step 1: Make the channel registry `Arc`-shared**

In `src/main.rs`, change `let (channels, layout) = …` handling so `channels` becomes `Arc<ChannelRegistry>`:
```rust
let channels = Arc::new(channels);
```
immediately after it is loaded, and update the call sites that take `&ChannelRegistry` to pass `channels.as_ref()` (e.g. `LiveStore::from_registry(channels.as_ref())`, `ZmqSource::build(config, channels.as_ref())`, `MqttSource::new(cfg, channels.as_ref())`, `WsSource::new(cfg, channels.as_ref())`, `Workspace::from_config(&layout, &registry, channels.as_ref())`, `datavis::demo::spawn_demo(store.clone(), channels.as_ref(), demo_freq)`).
In `src/app.rs`, change the `channels` field type to `Arc<ChannelRegistry>` and `DataVisApp::new`'s `channels: ChannelRegistry` parameter to `channels: Arc<ChannelRegistry>`. All existing `self.channels.<method>()` calls work unchanged through `Arc` deref. `ChannelTree::build(&channels)` becomes `ChannelTree::build(channels.as_ref())`.

Run: `cargo build`
Expected: builds (pure type-threading change; no behaviour change).

- [ ] **Step 2: Draw the script panel (failing test on toggle diffing)**

Create `src/script/panel.rs`:
```rust
use eframe::egui;

use crate::script::runner::ScriptState;
use crate::script::{ScriptStatus, SharedStatus};

/// A requested enable/disable from a checkbox click.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelToggle {
    pub name: String,
    pub enable: bool,
}

/// Draw the script list. `available` is every `*.py` stem found in the scripts
/// dir; `enabled` is the currently-active set. Returns any checkbox toggles.
pub fn draw_script_panel(
    ui: &mut egui::Ui,
    available: &[String],
    enabled: &[String],
    status: &SharedStatus,
    disabled: &Option<String>,
) -> Vec<PanelToggle> {
    let mut toggles = Vec::new();
    ui.heading("Scripts");
    if let Some(reason) = disabled {
        ui.colored_label(egui::Color32::from_rgb(0xB0, 0x60, 0x00), reason);
        return toggles;
    }
    let states = status.lock().unwrap().clone();
    for name in available {
        let mut on = enabled.iter().any(|e| e == name);
        if ui.checkbox(&mut on, name).changed() {
            toggles.push(PanelToggle { name: name.clone(), enable: on });
        }
        if let Some(s) = states.iter().find(|s| &s.name == name) {
            ui.small(status_line(s));
        }
    }
    toggles
}

fn status_line(s: &ScriptStatus) -> String {
    match &s.state {
        ScriptState::Healthy => "  ● running".to_string(),
        ScriptState::Waiting(what) => format!("  ○ waiting for {what}"),
        ScriptState::Failed(msg) => format!("  ✗ {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_formats_each_state() {
        let mk = |st| ScriptStatus { name: "x".into(), state: st };
        assert!(status_line(&mk(ScriptState::Healthy)).contains("running"));
        assert!(status_line(&mk(ScriptState::Waiting("in.a".into()))).contains("in.a"));
        assert!(status_line(&mk(ScriptState::Failed("boom".into()))).contains("boom"));
    }
}
```
Add `pub mod panel;` to `src/script/mod.rs`.

Run: `cargo test --lib script::panel`
Expected: PASS.

- [ ] **Step 3: Construct and spawn the engine in `main.rs`**

In `src/main.rs`, after the other sources are pushed and before building `PanelRegistry`, add:
```rust
// Scripting engine: load the [scripts] section, discover *.py in its dir,
// and spawn the numba-backed engine (disabled gracefully if numba is absent).
let scripts_cfg = datavis::script::config::ScriptsConfig::from_toml_str(
    &std::fs::read_to_string(&layout_path).unwrap_or_default(),
)
.unwrap_or_default();
let scripts_dir = layout_path
    .parent()
    .unwrap_or_else(|| Path::new("."))
    .join(&scripts_cfg.dir);
let available_scripts = datavis::script::discover_scripts(&scripts_dir);

let engine = datavis::script::ScriptEngine::new(
    scripts_dir.clone(),
    scripts_cfg.enabled.clone(),
    scripts_cfg.window_s,
    Box::new(datavis::script::python::PyScriptLoader),
    channels.clone(),
    Box::new(datavis::script::python::probe_numba),
);
let script_status = engine.status();
let script_commands = engine.commands();
let script_disabled = engine.disabled_reason();
sources.push(Box::new(engine).spawn(store.clone()));
```
Add a small discovery helper to `src/script/mod.rs`:
```rust
/// Every `*.py` stem in `dir` (sorted). Missing dir → empty.
pub fn discover_scripts(dir: &std::path::Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("py") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names
}
```

- [ ] **Step 4: Pass script handles into `DataVisApp` and store them**

Extend `DataVisApp::new` with parameters `available_scripts: Vec<String>`, `script_enabled: Vec<String>`, `script_status: crate::script::SharedStatus`, `script_commands: crossbeam_channel::Sender<crate::script::ScriptCommand>`, `script_disabled: Arc<Mutex<Option<String>>>`, and add matching fields. Pass them from `main.rs`. Initialise the app fields directly from the parameters.

Run: `cargo build`
Expected: builds.

- [ ] **Step 5: Render the panel and apply toggles with persistence**

In `src/app.rs`, inside the left side panel rendering (near the existing channel-picker `SidePanel`), draw the script panel and handle toggles:
```rust
{
    let disabled = self.script_disabled.lock().unwrap().clone();
    let toggles = crate::script::panel::draw_script_panel(
        ui,
        &self.available_scripts,
        &self.script_enabled,
        &self.script_status,
        &disabled,
    );
    for t in toggles {
        if t.enable {
            if !self.script_enabled.contains(&t.name) {
                self.script_enabled.push(t.name.clone());
            }
            let _ = self.script_commands.send(crate::script::ScriptCommand::Enable(t.name.clone()));
        } else {
            self.script_enabled.retain(|n| n != &t.name);
            let _ = self.script_commands.send(crate::script::ScriptCommand::Disable(t.name.clone()));
        }
        self.persist_scripts();
    }
}
```
Add the persistence helper to `impl DataVisApp`:
```rust
/// Write the current enabled-script set back to config.toml, preserving the
/// dir and window_s already on disk.
fn persist_scripts(&mut self) {
    let existing = std::fs::read_to_string(&self.layout_path).unwrap_or_default();
    let mut cfg = crate::script::config::ScriptsConfig::from_toml_str(&existing).unwrap_or_default();
    cfg.enabled = self.script_enabled.clone();
    self.status = match cfg.save(&self.layout_path) {
        Ok(()) => "scripts saved".to_string(),
        Err(e) => format!("scripts save failed: {e}"),
    };
    self.status_clear_at = Some(Instant::now() + Duration::from_secs(2));
}
```

- [ ] **Step 6: Manually verify the panel end-to-end**

Run (in the numba dev shell):
```bash
mkdir -p scripts
cat > scripts/accel_mag.py <<'PY'
import numpy as np
import numba
INPUTS  = ["demo.sine"]
OUTPUTS = [{"name": "demo.sine.abs", "type": "float"}]
@numba.njit
def compute(ts, vals):
    return (ts[0], np.abs(vals[0]))
PY
cargo run --features scripting -- --demo
```
Expected: the left panel shows a **Scripts** section listing `accel_mag`; ticking it makes `demo.sine.abs` appear as a droppable channel and the status line reads "● running". Untick → config.toml gains `[scripts] enabled = []`.

- [ ] **Step 7: Run the whole suite and commit**

Run: `cargo test --features scripting`
Expected: PASS.
```bash
git add src/main.rs src/app.rs src/script/mod.rs src/script/panel.rs
git commit -m "feat(script): wire engine into app, add script panel and config persistence"
```

---

### Task 9: Packaging — Linux `.deb` deps and Windows bundling

**Files:**
- Modify: `Cargo.toml` (`[package.metadata.deb]` depends)
- Modify: `src/main.rs` (resolve bundled `PYTHONHOME` on Windows)
- Create: `docs/packaging-python.md` (Windows bundling steps for CI)

**Interfaces:**
- Consumes: nothing new. Produces shippable artifacts that include the Python runtime.

- [ ] **Step 1: Declare the Linux runtime dependencies**

In `Cargo.toml`, add to `[package.metadata.deb]`:
```toml
depends = "$auto, python3, python3-numba, python3-numpy"
```

- [ ] **Step 2: Resolve the bundled interpreter on Windows at startup**

In `src/main.rs`, before `eframe::run_native`, add:
```rust
// On Windows the interpreter ships beside the exe under `python/`. Point the
// embedded runtime at it before any Python is initialised. On Linux the system
// python3 (a .deb dependency) is used, so nothing is set.
#[cfg(all(windows, feature = "scripting"))]
{
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let py_home = dir.join("python");
            if py_home.is_dir() {
                std::env::set_var("PYTHONHOME", &py_home);
            }
        }
    }
}
```

- [ ] **Step 3: Document the Windows bundle build**

Create `docs/packaging-python.md`:
```markdown
# Bundling Python for the Windows release

The Windows portable zip ships a self-contained CPython with numba/numpy so end
users install nothing. The Linux `.deb` instead depends on `python3-numba`
(resolved by apt) and bundles nothing.

## Steps (run on Windows CI, matching the release toolchain)

1. Download a relocatable CPython from python-build-standalone
   (`cpython-3.x.y+*-x86_64-pc-windows-msvc-*.tar.zst`) and extract it to
   `dist/python/`.
2. Pre-install the numeric stack into that interpreter:
   ```
   dist/python/python.exe -m pip install --no-warn-script-location numba numpy
   ```
3. Build datavis against that interpreter so PyO3 links its `pythonXY.dll`:
   ```
   set PYO3_PYTHON=%CD%\dist\python\python.exe
   cargo build --release --features scripting
   ```
4. Assemble the zip: `datavis.exe`, the matching `pythonXY.dll` beside it, and
   the `python/` tree. At runtime `main.rs` sets `PYTHONHOME` to `python/`.

Keep the interpreter version pinned in CI. Pin the numba/numpy wheels by hash
(a `requirements.txt` with hashes) so the bundle is reproducible — cargo-vet
does not cover Python wheels.
```

- [ ] **Step 4: Verify the Linux dependency metadata**

Run: `cargo deb --no-build --version 2>/dev/null || cargo deb -- -h >/dev/null; grep -n "python3-numba" Cargo.toml`
Expected: `Cargo.toml` shows the `depends` line. (Full `.deb` build happens in CI containers.)

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/main.rs docs/packaging-python.md
git commit -m "feat(script): package Python — deb deps on Linux, bundled interpreter on Windows"
```

---

## Notes for the implementer

- **PyO3 / rust-numpy API drift:** the exact method names (`import_bound`, `from_slice_bound`, `PyModule::from_code_bound`, `downcast::<PyArray1<T>>`, `to_vec`) target pyo3 0.22 / numpy 0.22. If the resolved versions differ, adjust to the equivalent calls — the shapes (build tuple of arrays, call, downcast return) stay the same.
- **Eager compile via warm-up call:** the spec's load flow says "call `compute.compile(signature)`". This plan compiles eagerly by instead calling `compute` once with length-1 dummy `(ts, vals)` tuples (`warm_up`, Task 5). This is deliberate: numba compiles a specialisation on first call, so the warm-up leaves the function native before the first real tick — the spec's intent (warm from the first sample) — without constructing numba type objects across FFI, which is brittle. If a future numba exposes a clean typed-`.compile` path from Rust, switching is a drop-in change with the same load-time guarantee. A script whose `compute` cannot run on length-1 inputs fails at load with the numba error, which is surfaced as a load failure like any other.
- **numba-gated tests** call `skip_without_numba()` and no-op when numba is absent, so `cargo test` passes in a bare environment; run them in the numba dev shell to actually exercise the Python path.
- **Concurrency safety** of runtime channel registration (`add_dynamic` + `store.add_channel` on the engine thread while the UI reads) is guaranteed by the existing append-only `boxcar::Vec` + `RwLock` design documented in `dynamic_channel.rs`.
