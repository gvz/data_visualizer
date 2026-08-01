# Configurable Script Bindings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let one `*.py` script be reused as several independently-bound instances, each with its own input and output channels, configured in `config.toml` and editable in the GUI.

**Architecture:** Replace the `[scripts].enabled` stem list with `[[scripts.instances]]` tables keyed by an explicit `id`. Each instance binds a script (file stem) to input channels and output channels; output names support `{inN}`/`{inN.stem}` templates expanded per instance. The background `ScriptEngine` builds one runner per enabled instance (keyed by id), tracks each instance's owned output channels for clean rebuilds, and peeks every discovered script's default metadata into a shared map the GUI editor reads.

**Tech Stack:** Rust, eframe/egui (GUI), PyO3 + numba (scripting, `scripting` feature, default-on), `toml` + `toml_edit` (config), `crossbeam-channel` (GUI→engine commands).

## Global Constraints

- The `scripting` cargo feature is DEFAULT-ON; the engine must still compile and the GUI still run under `--no-default-features` (no `scripting`), where the metadata map is empty and the panel shows the existing "scripting unavailable" note.
- `config.toml` is local session state — never commit changes to it except the shipped-default migration in the final task.
- Never add `Co-Authored-By` or any self-attribution to commits.
- Do not weaken the flake: `extra-substituters = []` and the `cache.numtide.com` ban stay.
- Output name templates: `{inN}` → the Nth resolved input's full channel name; `{inN.stem}` → its last `/`-separated segment. `N` is 0-based. No placeholder → literal.
- Instance identity is an explicit, required, unique `id`.
- Bad instances are skipped and surfaced as `Failed(<msg>)`; other instances still load and the app still starts.
- GUI input picker binds only to channels that currently exist in the registry.

---

### Task 1: Script binding helpers (template expansion + type parsing)

Pure, feature-independent helpers in `types.rs`, used later by the engine and by `python.rs`.

**Files:**
- Modify: `src/script/types.rs` (add two functions + tests)
- Modify: `src/script/python.rs` (route `output_sample_type` through the new parser)

**Interfaces:**
- Produces:
  - `pub fn parse_sample_type(ty: &str) -> Result<crate::types::SampleType, String>` — accepts `"float"|"int"|"bool"`, rejects anything else (including `"text"`) with `"output type '<ty>' is not one of float/int/bool"`.
  - `pub fn expand_output_name(template: &str, inputs: &[String]) -> Result<String, String>` — expands `{inN}` and `{inN.stem}`; unknown placeholder or out-of-range index → `Err`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/script/types.rs`:

```rust
#[test]
fn parse_sample_type_accepts_numeric_and_rejects_text() {
    use crate::types::SampleType;
    assert_eq!(parse_sample_type("float").unwrap(), SampleType::Float);
    assert_eq!(parse_sample_type("int").unwrap(), SampleType::Int);
    assert_eq!(parse_sample_type("bool").unwrap(), SampleType::Bool);
    assert!(parse_sample_type("text").unwrap_err().contains("float/int/bool"));
    assert!(parse_sample_type("nope").is_err());
}

#[test]
fn expand_output_name_literal_passthrough() {
    let inputs = vec!["load/ch0".to_string()];
    assert_eq!(expand_output_name("scripts.ch0_rms", &inputs).unwrap(), "scripts.ch0_rms");
}

#[test]
fn expand_output_name_full_and_stem() {
    let inputs = vec!["load/ch0".to_string()];
    assert_eq!(expand_output_name("{in0}", &inputs).unwrap(), "load/ch0");
    assert_eq!(expand_output_name("{in0.stem}.rms", &inputs).unwrap(), "ch0.rms");
}

#[test]
fn expand_output_name_multi_input_indices() {
    let inputs = vec!["a/x".to_string(), "b/y".to_string()];
    assert_eq!(expand_output_name("{in1.stem}-{in0.stem}", &inputs).unwrap(), "y-x");
}

#[test]
fn expand_output_name_unknown_placeholder_errors() {
    let inputs = vec!["a".to_string()];
    assert!(expand_output_name("{in5}", &inputs).is_err());       // index out of range
    assert!(expand_output_name("{bogus}", &inputs).is_err());     // unrecognized form
    assert!(expand_output_name("{in0.foo}", &inputs).is_err());   // unknown modifier
}

#[test]
fn expand_output_name_stem_of_unslashed_is_whole() {
    let inputs = vec!["plain".to_string()];
    assert_eq!(expand_output_name("{in0.stem}", &inputs).unwrap(), "plain");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p datavis --lib script::types`
Expected: FAIL — `parse_sample_type` / `expand_output_name` not found.

- [ ] **Step 3: Implement the helpers**

Add to `src/script/types.rs` (top, after the `use` line):

```rust
/// Parse an output channel's declared type. Feature-independent so the engine
/// (not behind `scripting`) can resolve instance output overrides. Text outputs
/// are rejected — scripts publish numeric channels only.
pub fn parse_sample_type(ty: &str) -> Result<crate::types::SampleType, String> {
    use crate::types::SampleType;
    match ty {
        "float" => Ok(SampleType::Float),
        "int" => Ok(SampleType::Int),
        "bool" => Ok(SampleType::Bool),
        other => Err(format!("output type '{other}' is not one of float/int/bool")),
    }
}

/// Expand `{inN}` / `{inN.stem}` placeholders in an output-name template against
/// an instance's resolved input channel names. `{inN}` yields the Nth input's
/// full name; `{inN.stem}` its last `/`-separated segment. Any other `{...}`
/// form, or an out-of-range index, is an error.
pub fn expand_output_name(template: &str, inputs: &[String]) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| format!("unterminated placeholder in '{template}'"))?
            + open;
        let token = &rest[open + 1..close]; // between the braces
        out.push_str(&expand_token(token, inputs)?);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn expand_token(token: &str, inputs: &[String]) -> Result<String, String> {
    let (idx_part, want_stem) = match token.strip_suffix(".stem") {
        Some(prefix) => (prefix, true),
        None => (token, false),
    };
    let idx_str = idx_part
        .strip_prefix("in")
        .ok_or_else(|| format!("unknown placeholder '{{{token}}}'"))?;
    let idx: usize = idx_str
        .parse()
        .map_err(|_| format!("unknown placeholder '{{{token}}}'"))?;
    let name = inputs
        .get(idx)
        .ok_or_else(|| format!("placeholder '{{{token}}}' has no input {idx}"))?;
    Ok(if want_stem {
        name.rsplit('/').next().unwrap_or(name).to_string()
    } else {
        name.clone()
    })
}
```

Then in `src/script/python.rs`, replace the body of `output_sample_type` (lines ~30-37) to delegate:

```rust
fn output_sample_type(ty: &str) -> Result<SampleType, String> {
    crate::script::types::parse_sample_type(ty)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p datavis --lib script::types` then `cargo build`
Expected: PASS; build clean.

- [ ] **Step 5: Commit**

```bash
git add src/script/types.rs src/script/python.rs
git commit -m "feat: script binding helpers for name templates and type parsing"
```

---

### Task 2: `peek_meta` — read a script's declared bindings without compiling

The GUI editor needs each script's input arity and default outputs to prefill fields. Compiling numba is expensive, so add a cheap metadata-only read to the loader.

**Files:**
- Modify: `src/script/types.rs` (add `peek_meta` to the `ScriptLoader` trait)
- Modify: `src/script/python.rs` (implement `peek_meta` for `PyScriptLoader`, factor out the meta-extraction it shares with `load`)
- Modify: `src/script/mod.rs` (add `peek_meta` to the `DoublerLoader` test loader)

**Interfaces:**
- Produces: `fn peek_meta(&self, source: &str, name: &str) -> Result<ScriptMeta, String>` on `trait ScriptLoader`. Returns `ScriptMeta` (inputs + outputs; output names are raw templates) without warming up / compiling `compute`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/script/python.rs` (this module is compiled only with the `scripting` feature, matching the other tests there):

```rust
#[test]
fn peek_meta_reads_bindings_without_compile() {
    let src = "INPUTS=[\"load/ch0\"]\nOUTPUTS=[{\"name\":\"{in0.stem}.rms\",\"type\":\"float\",\"unit\":\"g\"}]\n# no compute at all\n";
    let meta = PyScriptLoader.peek_meta(src, "m").unwrap();
    assert_eq!(meta.inputs, vec!["load/ch0".to_string()]);
    assert_eq!(meta.outputs.len(), 1);
    assert_eq!(meta.outputs[0].name, "{in0.stem}.rms"); // template kept verbatim
    assert_eq!(meta.outputs[0].unit, "g");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis --lib script::python::tests::peek_meta_reads_bindings_without_compile`
Expected: FAIL — `peek_meta` not a member of `ScriptLoader`.

- [ ] **Step 3: Implement**

In `src/script/types.rs`, add the method to the trait (default-less; every impl must provide it):

```rust
pub trait ScriptLoader: Send {
    fn load(&self, source: &str, name: &str) -> Result<LoadedScript, String>;

    /// Read a script's declared `INPUTS`/`OUTPUTS` without compiling `compute`.
    /// Output names are returned as their raw templates. Used by the GUI editor
    /// to prefill an instance's fields when a script is chosen.
    fn peek_meta(&self, source: &str, name: &str) -> Result<ScriptMeta, String>;
}
```

In `src/script/python.rs`, factor the meta extraction out of `load` and reuse it in `peek_meta`. Add a free function and call it from both:

```rust
/// Extract `INPUTS`/`OUTPUTS` from an executed module into a `ScriptMeta`.
fn extract_meta(module: &Bound<'_, PyModule>) -> Result<ScriptMeta, String> {
    let inputs: Vec<String> = module
        .getattr("INPUTS")
        .map_err(|_| "script is missing INPUTS".to_string())?
        .extract()
        .map_err(|e| format!("INPUTS must be a list of strings: {e}"))?;

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
        let unit: String = item.get_item("unit").and_then(|v| v.extract()).unwrap_or_default();
        outputs.push(OutputSpec { name: out_name, sample_type: output_sample_type(&ty)?, unit });
    }
    Ok(ScriptMeta { inputs, outputs })
}
```

Replace the inline INPUTS/OUTPUTS block in `load` (lines ~46-73) with `let meta = extract_meta(&module)?;` and use `meta.inputs.len()` for `warm_up` and `meta.outputs.len()` for `n_outputs`. Add `peek_meta`:

```rust
fn peek_meta(&self, source: &str, name: &str) -> Result<ScriptMeta, String> {
    Python::with_gil(|py| {
        let module = PyModule::from_code_bound(py, source, &format!("{name}.py"), name)
            .map_err(|e| e.to_string())?;
        extract_meta(&module)
    })
}
```

In `src/script/mod.rs`, add to `impl ScriptLoader for DoublerLoader`:

```rust
fn peek_meta(&self, _source: &str, _name: &str) -> Result<ScriptMeta, String> {
    Ok(ScriptMeta {
        inputs: vec!["in.a".into()],
        outputs: vec![OutputSpec {
            name: "in.a.double".into(),
            sample_type: SampleType::Float,
            unit: String::new(),
        }],
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p datavis --lib script::` then `cargo build`
Expected: PASS; build clean.

- [ ] **Step 5: Commit**

```bash
git add src/script/types.rs src/script/python.rs src/script/mod.rs
git commit -m "feat: peek_meta reads script bindings without compiling"
```

---

### Task 3: Config — `[[scripts.instances]]` schema (additive)

Add the instance types and parsing/saving. Keep the old `enabled` field for one task so `main.rs`/`app.rs` still compile; Task 4 removes it.

**Files:**
- Modify: `src/script/config.rs`

**Interfaces:**
- Produces:
  - `pub struct OutputBinding { pub name: String, pub ty: String, pub unit: String }`
  - `pub struct ScriptInstance { pub id: String, pub script: String, pub inputs: Option<Vec<String>>, pub outputs: Option<Vec<OutputBinding>>, pub enabled: bool }`
  - `ScriptsConfig` gains `pub instances: Vec<ScriptInstance>` (default empty). `save` also writes `[[scripts.instances]]`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/script/config.rs`:

```rust
#[test]
fn parses_instances_with_defaults() {
    let c = ScriptsConfig::from_toml_str(
        "[scripts]\ndir = \"s\"\nwindow_s = 4.0\n\n\
         [[scripts.instances]]\nid = \"a\"\nscript = \"sine_rms\"\n\n\
         [[scripts.instances]]\nid = \"b\"\nscript = \"sine_rms\"\n\
         inputs = [\"load/ch1\"]\nenabled = false\n\
         outputs = [{ name = \"scripts.b\", type = \"float\", unit = \"g\" }]\n",
    )
    .unwrap();
    assert_eq!(c.dir, "s");
    assert_eq!(c.window_s, 4.0);
    assert_eq!(c.instances.len(), 2);

    let a = &c.instances[0];
    assert_eq!(a.id, "a");
    assert_eq!(a.script, "sine_rms");
    assert_eq!(a.inputs, None);
    assert_eq!(a.outputs, None);
    assert!(a.enabled); // default true

    let b = &c.instances[1];
    assert_eq!(b.inputs, Some(vec!["load/ch1".to_string()]));
    assert!(!b.enabled);
    let ob = b.outputs.as_ref().unwrap();
    assert_eq!(ob[0].name, "scripts.b");
    assert_eq!(ob[0].ty, "float");
    assert_eq!(ob[0].unit, "g");
}

#[test]
fn save_round_trips_instances_preserving_other_sections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "# top\ndefault_window_s = 5.0\n\n[channels.\"x\"]\ntype = \"float\"\n")
        .unwrap();

    let cfg = ScriptsConfig {
        dir: "scripts".into(),
        window_s: 7.0,
        instances: vec![
            ScriptInstance {
                id: "ch0_rms".into(),
                script: "sine_rms".into(),
                inputs: Some(vec!["load/ch0".into()]),
                outputs: None,
                enabled: true,
            },
            ScriptInstance {
                id: "ch1_rms".into(),
                script: "sine_rms".into(),
                inputs: Some(vec!["load/ch1".into()]),
                outputs: Some(vec![OutputBinding {
                    name: "scripts.ch1_rms".into(),
                    ty: "float".into(),
                    unit: String::new(),
                }]),
                enabled: false,
            },
        ],
    };
    cfg.save(&path).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    let reparsed = ScriptsConfig::from_toml_str(&text).unwrap();
    assert_eq!(reparsed, cfg);
    assert!(text.contains("# top"));
    assert!(text.contains("[channels.\"x\"]"));
}
```

Delete the old `parses_all_fields` and `missing_keys_fall_back` assertions that reference `enabled` as `Vec<String>` — replace their intent with the tests above. (The `absent_section_yields_defaults` test stays.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p datavis --lib script::config`
Expected: FAIL — `instances` / `ScriptInstance` / `OutputBinding` not found.

- [ ] **Step 3: Implement**

In `src/script/config.rs`, add the types and extend parsing/saving. Keep `pub enabled: Vec<String>` on `ScriptsConfig` for now.

```rust
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputBinding {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptInstance {
    pub id: String,
    pub script: String,
    #[serde(default)]
    pub inputs: Option<Vec<String>>,
    #[serde(default)]
    pub outputs: Option<Vec<OutputBinding>>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}
```

Add `instances: Vec<ScriptInstance>` to `ScriptsConfig` and its `Default` (empty vec). Add `#[serde(default)] instances: Vec<ScriptInstance>` to `RawScripts`, and in `from_toml_str` set `instances: raw.instances`. In `save`, after the existing keys, write the array of tables:

```rust
use toml_edit::{ArrayOfTables, Table};

let mut tables = ArrayOfTables::new();
for inst in &self.instances {
    let mut t = Table::new();
    t["id"] = value(inst.id.as_str());
    t["script"] = value(inst.script.as_str());
    if let Some(inputs) = &inst.inputs {
        let mut arr = Array::new();
        for name in inputs {
            arr.push(name.as_str());
        }
        t["inputs"] = value(arr);
    }
    if let Some(outputs) = &inst.outputs {
        let mut outs = Array::new();
        for o in outputs {
            let mut it = toml_edit::InlineTable::new();
            it.insert("name", o.name.as_str().into());
            it.insert("type", o.ty.as_str().into());
            it.insert("unit", o.unit.as_str().into());
            outs.push(toml_edit::Value::InlineTable(it));
        }
        t["outputs"] = value(outs);
    }
    t["enabled"] = value(inst.enabled);
    tables.push(t);
}
doc["scripts"]["instances"] = toml_edit::Item::ArrayOfTables(tables);
```

Remove the `doc["scripts"]["enabled"] = ...` line (the `enabled` list is no longer persisted). Keep `dir` and `window_s`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p datavis --lib script::config` then `cargo build`
Expected: PASS; build clean (`enabled` field still exists, unused by save).

- [ ] **Step 5: Commit**

```bash
git add src/script/config.rs
git commit -m "feat: parse and save [[scripts.instances]] config tables"
```

---

### Task 4: Engine instance model + minimal instance panel

Rewire the engine to build one runner per `ScriptInstance` (keyed by id), with arity checks, output overrides, and template expansion; add `Upsert`/`Remove` commands and per-instance owned-output tracking; peek every discovered script's meta into a shared map; and update `main.rs` and a **minimal** read-only-plus-toggle panel so everything compiles. The full editor is Task 5. Remove the deprecated `enabled` field.

**Files:**
- Modify: `src/script/mod.rs` (engine, commands, metas map, runner build)
- Modify: `src/script/panel.rs` (minimal panel over instances)
- Modify: `src/script/config.rs` (drop `enabled` field)
- Modify: `src/main.rs` (construct engine from instances; pass metas map)
- Modify: `src/app.rs` (hold instances + metas; drive minimal panel)

**Interfaces:**
- Produces:
  - `pub enum ScriptCommand { Upsert(ScriptInstance), Remove(String) }`
  - `ScriptEngine::new(dir: PathBuf, instances: Vec<ScriptInstance>, window_s: f64, loader: Box<dyn ScriptLoader>, registry: Arc<ChannelRegistry>, probe: Box<dyn Fn() -> Result<(), String> + Send>) -> Self`
  - `pub fn script_metas(&self) -> SharedMetas` where `pub type SharedMetas = Arc<Mutex<std::collections::HashMap<String, ScriptMeta>>>` (stem → default meta).
  - `ScriptEngine::build_runner(&self, inst: &ScriptInstance, store: &dyn ChannelStore) -> Result<ScriptRunner, ScriptStatus>` resolving inputs (override or file default), checking arity, resolving outputs (override → `OutputSpec` via `parse_sample_type`, else file defaults), expanding templates, validating, registering, keyed by `inst.id`.
  - Minimal `draw_script_panel(ui, instances: &[ScriptInstance], status: &SharedStatus, disabled: &Option<String>) -> Vec<PanelCommand>` where `pub enum PanelCommand { Upsert(ScriptInstance), Remove(String), SaveConfig }`.
- Consumes: `expand_output_name`, `parse_sample_type` (Task 1); `peek_meta` (Task 2); `ScriptInstance`, `OutputBinding`, `ScriptsConfig.instances` (Task 3).

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/script/mod.rs`. The `DoublerLoader` already returns `inputs = ["in.a"]`, `outputs = [OutputSpec{name:"in.a.double",...}]` from `load`; give its `peek_meta` (added in Task 2) the same. These tests drive the engine through instances.

```rust
fn instance(id: &str, script: &str, inputs: Option<Vec<&str>>) -> crate::script::config::ScriptInstance {
    crate::script::config::ScriptInstance {
        id: id.into(),
        script: script.into(),
        inputs: inputs.map(|v| v.into_iter().map(String::from).collect()),
        outputs: None,
        enabled: true,
    }
}

#[test]
fn build_runner_uses_instance_id_and_input_override() {
    // Registry has two inputs; DoublerLoader declares arity 1 with a templated
    // default output. An instance overrides the input to "in.b".
    let reg = Arc::new(
        ChannelRegistry::from_toml_str(
            "[channels.\"in.a\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n\
             [channels.\"in.b\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n",
        )
        .unwrap(),
    );
    let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("dbl.py"), "# fake").unwrap();

    let engine = ScriptEngine::new(
        dir.path().to_path_buf(),
        vec![instance("first", "dbl", Some(vec!["in.b"]))],
        10.0,
        Box::new(TemplateDoublerLoader),
        reg.clone(),
        Box::new(|| Ok(())),
    );
    let runner = engine.build_runner(&instance("first", "dbl", Some(vec!["in.b"])), store.as_ref()).unwrap();
    assert_eq!(runner.name(), "first");                 // keyed by id, not stem
    assert!(reg.id("in.b.double").is_some());           // {in0}.double expanded from in.b
}

#[test]
fn build_runner_rejects_arity_mismatch() {
    let reg = Arc::new(
        ChannelRegistry::from_toml_str(
            "[channels.\"in.a\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n",
        )
        .unwrap(),
    );
    let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
    let dir = tempfile::tempdir().unwrap();
    let engine = ScriptEngine::new(
        dir.path().to_path_buf(),
        vec![],
        10.0,
        Box::new(TemplateDoublerLoader),
        reg,
        Box::new(|| Ok(())),
    );
    // Loader declares arity 1; instance binds two inputs.
    let bad = instance("x", "dbl", Some(vec!["in.a", "in.a"]));
    let err = engine.build_runner(&bad, store.as_ref()).unwrap_err();
    assert!(matches!(err.state, ScriptState::Failed(_)));
}

#[test]
fn two_instances_of_one_script_make_distinct_channels() {
    let reg = Arc::new(
        ChannelRegistry::from_toml_str(
            "[channels.\"in.a\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n\
             [channels.\"in.b\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n",
        )
        .unwrap(),
    );
    let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
    let dir = tempfile::tempdir().unwrap();
    let engine = ScriptEngine::new(
        dir.path().to_path_buf(),
        vec![],
        10.0,
        Box::new(TemplateDoublerLoader),
        reg.clone(),
        Box::new(|| Ok(())),
    );
    engine.build_runner(&instance("a", "dbl", Some(vec!["in.a"])), store.as_ref()).unwrap();
    engine.build_runner(&instance("b", "dbl", Some(vec!["in.b"])), store.as_ref()).unwrap();
    assert!(reg.id("in.a.double").is_some());
    assert!(reg.id("in.b.double").is_some());
}
```

Add a template-aware fake loader next to `DoublerLoader` (its default output name is a template so the engine expands it):

```rust
struct TemplateDoublerLoader;
impl ScriptLoader for TemplateDoublerLoader {
    fn load(&self, _s: &str, _n: &str) -> Result<LoadedScript, String> {
        Ok(LoadedScript {
            meta: ScriptMeta {
                inputs: vec!["in.a".into()],
                outputs: vec![OutputSpec { name: "{in0}.double".into(), sample_type: SampleType::Float, unit: String::new() }],
            },
            compiled: Box::new(Doubler),
        })
    }
    fn peek_meta(&self, _s: &str, _n: &str) -> Result<ScriptMeta, String> {
        self.load("", "").map(|l| l.meta)
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p datavis --lib script::mod 2>&1 | head -40`
Expected: FAIL — `ScriptEngine::new` arity, `build_runner` visibility, `ScriptInstance` not accepted.

- [ ] **Step 3: Implement the engine changes**

In `src/script/mod.rs`:

Add imports: `use std::collections::HashMap;`, `use crate::script::config::ScriptInstance;`, `use crate::script::types::{expand_output_name, parse_sample_type, OutputSpec, ScriptMeta};` (extend the existing `types` use).

Replace `ScriptCommand`:

```rust
/// GUI → engine live control.
pub enum ScriptCommand {
    /// Add a new instance or replace an existing one with the same id.
    Upsert(ScriptInstance),
    /// Remove the instance with this id.
    Remove(String),
}
```

Add the shared metas type and change the engine struct/fields:

```rust
/// Stem → default `ScriptMeta`, peeked from each discovered script for the GUI editor.
pub type SharedMetas = Arc<Mutex<HashMap<String, ScriptMeta>>>;
```

In `ScriptEngine`: replace `enabled: Vec<String>` with `instances: Vec<ScriptInstance>`; replace `script_outputs: Arc<Mutex<HashSet<String>>>` with per-instance ownership `owned_outputs: Arc<Mutex<HashMap<String, Vec<String>>>>` (id → its output names); add `metas: SharedMetas`. Update `new` to the signature in Interfaces, initializing `metas: Arc::new(Mutex::new(HashMap::new()))` and `owned_outputs` empty. Add:

```rust
pub fn script_metas(&self) -> SharedMetas {
    self.metas.clone()
}
```

Rewrite `build_runner` to take an instance:

```rust
fn build_runner(
    &self,
    inst: &ScriptInstance,
    store: &dyn ChannelStore,
) -> Result<ScriptRunner, ScriptStatus> {
    let fail = |e: String| ScriptStatus { name: inst.id.clone(), state: ScriptState::Failed(e) };
    let source = self.read_source(&inst.script).map_err(&fail)?;
    let loaded = self.loader.load(&source, &inst.script).map_err(&fail)?;

    // Resolve inputs: instance override, else the file's declared inputs.
    let inputs = inst.inputs.clone().unwrap_or_else(|| loaded.meta.inputs.clone());
    if inputs.len() != loaded.meta.inputs.len() {
        return Err(fail(format!(
            "instance binds {} inputs but script '{}' declares {}",
            inputs.len(), inst.script, loaded.meta.inputs.len()
        )));
    }

    // Resolve outputs: instance override (parsed), else the file's outputs.
    let raw_outputs: Vec<OutputSpec> = match &inst.outputs {
        Some(obs) => {
            let mut v = Vec::with_capacity(obs.len());
            for o in obs {
                v.push(OutputSpec {
                    name: o.name.clone(),
                    sample_type: parse_sample_type(&o.ty).map_err(&fail)?,
                    unit: o.unit.clone(),
                });
            }
            v
        }
        None => loaded.meta.outputs.clone(),
    };

    // Expand output-name templates against the resolved inputs.
    let mut outputs = Vec::with_capacity(raw_outputs.len());
    for o in raw_outputs {
        outputs.push(OutputSpec {
            name: expand_output_name(&o.name, &inputs).map_err(&fail)?,
            sample_type: o.sample_type,
            unit: o.unit,
        });
    }

    let meta = ScriptMeta { inputs, outputs };

    // Collision check: an output name already known and owned by NO instance is a
    // real clash with a non-script channel. Names this same id already owns are
    // fine (rebuild reuses the slot).
    let mut owned = self.owned_outputs.lock().unwrap();
    let owned_by_others: std::collections::HashSet<&str> = owned
        .iter()
        .filter(|(k, _)| k.as_str() != inst.id)
        .flat_map(|(_, v)| v.iter().map(String::as_str))
        .collect();
    let exists = |n: &str| self.registry.id(n).is_some() && !owned_by_others.contains(n);
    validate_meta(&meta, exists).map_err(&fail)?;
    owned.insert(inst.id.clone(), meta.outputs.iter().map(|o| o.name.clone()).collect());
    drop(owned);

    Ok(ScriptRunner::new(inst.id.clone(), meta, loaded.compiled, store, &self.registry))
}
```

Make `build_runner` `pub(crate)` (tests call it) or add `#[cfg(test)] pub`. Simplest: change `fn build_runner` to `pub fn build_runner`.

Rewrite `load_into` to take an instance and skip disabled ones:

```rust
fn load_into(
    &self,
    inst: &ScriptInstance,
    store: &dyn ChannelStore,
    runners: &mut Vec<ScriptRunner>,
    failed: &mut Vec<ScriptStatus>,
) {
    runners.retain(|r| r.name() != inst.id);
    failed.retain(|f| f.name != inst.id);
    self.owned_outputs.lock().unwrap().remove(&inst.id);
    if !inst.enabled {
        return;
    }
    match self.build_runner(inst, store) {
        Ok(runner) => runners.push(runner),
        Err(status) => failed.push(status),
    }
}
```

Add a helper to remove an instance entirely:

```rust
fn remove_instance(&self, id: &str, runners: &mut Vec<ScriptRunner>, failed: &mut Vec<ScriptStatus>) {
    runners.retain(|r| r.name() != id);
    failed.retain(|f| f.name != id);
    self.owned_outputs.lock().unwrap().remove(id);
}
```

In `run_loop`: after the probe passes, peek every discovered script into `self.metas`, then load initial instances, then handle the new commands:

```rust
// Peek each discovered script's default meta for the GUI editor.
{
    let mut metas = self.metas.lock().unwrap();
    for stem in discover_scripts(&self.dir) {
        if let Ok(src) = self.read_source(&stem) {
            if let Ok(meta) = self.loader.peek_meta(&src, &stem) {
                metas.insert(stem, meta);
            }
        }
    }
}

let mut runners: Vec<ScriptRunner> = Vec::new();
let mut failed: Vec<ScriptStatus> = Vec::new();
for inst in self.instances.clone() {
    self.load_into(&inst, store.as_ref(), &mut runners, &mut failed);
}
```

Replace the command match arms:

```rust
Ok(ScriptCommand::Upsert(inst)) => self.load_into(&inst, store.as_ref(), &mut runners, &mut failed),
Ok(ScriptCommand::Remove(id)) => self.remove_instance(&id, &mut runners, &mut failed),
```

In `src/script/config.rs`, delete `pub enabled: Vec<String>` from `ScriptsConfig`, its `Default`, and the `enabled` field from `RawScripts` and `from_toml_str`.

Now the minimal panel. Replace `src/script/panel.rs` entirely:

```rust
use eframe::egui;

use crate::script::config::ScriptInstance;
use crate::script::runner::ScriptState;
use crate::script::{ScriptStatus, SharedStatus};

/// A committed panel action for the app to forward to the engine / config.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelCommand {
    Upsert(ScriptInstance),
    Remove(String),
    SaveConfig,
}

/// Minimal instance list: per-instance enable toggle + status. The full editor
/// (add/remove, input/output binding) lands in a later task.
pub fn draw_script_panel(
    ui: &mut egui::Ui,
    instances: &[ScriptInstance],
    status: &SharedStatus,
    disabled: &Option<String>,
) -> Vec<PanelCommand> {
    let mut cmds = Vec::new();
    ui.heading("Scripts");
    if let Some(reason) = disabled {
        ui.colored_label(egui::Color32::from_rgb(0xB0, 0x60, 0x00), reason);
        return cmds;
    }
    let states = status.lock().unwrap().clone();
    for inst in instances {
        let mut on = inst.enabled;
        if ui.checkbox(&mut on, &inst.id).changed() {
            cmds.push(PanelCommand::Upsert(ScriptInstance { enabled: on, ..inst.clone() }));
        }
        if let Some(s) = states.iter().find(|s| s.name == inst.id) {
            ui.small(status_line(s));
        }
    }
    cmds
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

In `src/main.rs`: pass `scripts_cfg.instances.clone()` to `ScriptEngine::new` instead of `scripts_cfg.enabled.clone()`; capture `let script_metas = engine.script_metas();` and thread it to the app; pass `scripts_cfg.instances` (not `.enabled`) to `DataVisApp::new`. In the `#[cfg(not(feature = "scripting"))]` branch, build `let script_metas: datavis::script::SharedMetas = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));` and include it in the tuple.

In `src/app.rs`: rename field `script_enabled: Vec<String>` → `script_instances: Vec<ScriptInstance>`; add `script_metas: SharedMetas` and `channels`-based name access already present. Update `DataVisApp::new` params to match `main.rs`. Replace the panel call site (lines ~727-748) with the minimal panel and command handling:

```rust
{
    let disabled = self.script_disabled.lock().unwrap().clone();
    let cmds = crate::script::panel::draw_script_panel(
        ui,
        &self.script_instances,
        &self.script_status,
        &disabled,
    );
    for cmd in cmds {
        self.apply_panel_command(cmd);
    }
}
```

Add:

```rust
fn apply_panel_command(&mut self, cmd: crate::script::panel::PanelCommand) {
    use crate::script::panel::PanelCommand;
    use crate::script::ScriptCommand;
    match cmd {
        PanelCommand::Upsert(inst) => {
            match self.script_instances.iter_mut().find(|i| i.id == inst.id) {
                Some(slot) => *slot = inst.clone(),
                None => self.script_instances.push(inst.clone()),
            }
            let _ = self.script_commands.send(ScriptCommand::Upsert(inst));
        }
        PanelCommand::Remove(id) => {
            self.script_instances.retain(|i| i.id != id);
            let _ = self.script_commands.send(ScriptCommand::Remove(id));
        }
        PanelCommand::SaveConfig => self.persist_scripts(),
    }
}
```

Rewrite `persist_scripts` to save instances:

```rust
fn persist_scripts(&mut self) {
    let existing = std::fs::read_to_string(&self.layout_path).unwrap_or_default();
    let mut cfg = crate::script::config::ScriptsConfig::from_toml_str(&existing).unwrap_or_default();
    cfg.instances = self.script_instances.clone();
    self.status = match cfg.save(&self.layout_path) {
        Ok(()) => "scripts saved".to_string(),
        Err(e) => format!("scripts save failed: {e}"),
    };
    self.status_clear_at = Some(Instant::now() + Duration::from_secs(2));
}
```

Add the needed imports to `app.rs`: `use crate::script::config::ScriptInstance;` and `use crate::script::SharedMetas;`.

- [ ] **Step 4: Run the full suite**

Run: `cargo test` then `cargo build`
Expected: PASS (all prior tests + the three new engine tests); build clean for default features.
Run: `cargo build --no-default-features` — Expected: clean (non-scripting branch compiles).

- [ ] **Step 5: Commit**

```bash
git add src/script/mod.rs src/script/panel.rs src/script/config.rs src/main.rs src/app.rs
git commit -m "feat: engine runs configured script instances keyed by id"
```

---

### Task 5: Full GUI instance editor

Replace the minimal panel with a full editor: add/remove instances, a searchable channel combobox per input slot (fuzzy filter over existing channels, existence-verified), per-output name/type/unit fields prefilled from the script's peeked meta, staged edits with an Apply button, and a Save-to-config button.

**Files:**
- Modify: `src/script/panel.rs` (editor UI + fuzzy matcher + staging state)
- Modify: `src/app.rs` (own the panel staging state; pass channel names + metas)

**Interfaces:**
- Consumes: `SharedMetas` (Task 4), `ScriptInstance`/`OutputBinding` (Task 3), the registry channel-name list (`self.channels.iter_ids().map(|id| self.channels.meta(id).name.clone())`).
- Produces:
  - `pub struct ScriptPanelState { /* staged editor rows, add-form fields */ }` with `Default`.
  - `pub fn draw_script_panel(ui, state: &mut ScriptPanelState, instances: &[ScriptInstance], metas: &HashMap<String, ScriptMeta>, channel_names: &[String], status: &SharedStatus, disabled: &Option<String>) -> Vec<PanelCommand>`
  - `pub fn fuzzy_rank<'a>(query: &str, candidates: &'a [String]) -> Vec<&'a String>` — subsequence match, case-insensitive; empty query returns all in original order.

- [ ] **Step 1: Write the failing tests**

The egui rendering isn't unit-tested, but the pure helpers are. Add to `src/script/panel.rs` tests:

```rust
#[test]
fn fuzzy_rank_empty_query_returns_all() {
    let c = vec!["load/ch0".to_string(), "load/ch1".to_string()];
    assert_eq!(fuzzy_rank("", &c), vec![&c[0], &c[1]]);
}

#[test]
fn fuzzy_rank_subsequence_case_insensitive() {
    let c = vec!["load/ch0".to_string(), "sys/temp".to_string(), "load/ch1".to_string()];
    let got = fuzzy_rank("lc1", &c);
    assert_eq!(got, vec![&c[2]]); // only "load/ch1" has l..c..1 as a subsequence
}

#[test]
fn fuzzy_rank_ranks_shorter_match_span_first() {
    let c = vec!["aXXXb".to_string(), "ab".to_string()];
    let got = fuzzy_rank("ab", &c);
    assert_eq!(got, vec![&c[1], &c[0]]); // tighter span ranks first
}

#[test]
fn staged_instance_round_trips_to_command() {
    // A staged row with a bound input and default output converts to an Upsert
    // carrying the same values.
    let row = StagedInstance {
        id: "r".into(),
        script: "sine_rms".into(),
        inputs: vec!["load/ch0".into()],
        outputs: vec![OutputBinding { name: "{in0.stem}.rms".into(), ty: "float".into(), unit: String::new() }],
        enabled: true,
    };
    let inst = row.to_instance();
    assert_eq!(inst.id, "r");
    assert_eq!(inst.inputs, Some(vec!["load/ch0".to_string()]));
    assert_eq!(inst.outputs.as_ref().unwrap()[0].name, "{in0.stem}.rms");
}

#[test]
fn input_unresolved_when_not_in_channel_list() {
    let names = vec!["load/ch0".to_string()];
    assert!(input_is_valid("load/ch0", &names));
    assert!(!input_is_valid("load/ch9", &names));
    assert!(!input_is_valid("", &names));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p datavis --lib script::panel`
Expected: FAIL — `fuzzy_rank`, `StagedInstance`, `input_is_valid` not found.

- [ ] **Step 3: Implement the editor**

Rewrite `src/script/panel.rs`. Keep `PanelCommand` and `status_line` from Task 4. Add:

```rust
use std::collections::HashMap;
use crate::script::config::OutputBinding;
use crate::script::types::ScriptMeta;

/// A row being edited in the panel before Apply commits it.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedInstance {
    pub id: String,
    pub script: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<OutputBinding>,
    pub enabled: bool,
}

impl StagedInstance {
    pub fn to_instance(&self) -> crate::script::config::ScriptInstance {
        crate::script::config::ScriptInstance {
            id: self.id.clone(),
            script: self.script.clone(),
            inputs: Some(self.inputs.clone()),
            outputs: Some(self.outputs.clone()),
            enabled: self.enabled,
        }
    }
}

/// Panel-persistent editor state: one staged row per instance id, plus the
/// add-instance form.
#[derive(Default)]
pub struct ScriptPanelState {
    pub staged: HashMap<String, StagedInstance>, // keyed by instance id
    pub new_id: String,
    pub new_script: String,
    /// Per-input free-text filter buffers, keyed by "<id>#<slot>".
    pub input_query: HashMap<String, String>,
}

/// Case-insensitive subsequence match; ranks tighter match spans first. Empty
/// query returns every candidate in original order.
pub fn fuzzy_rank<'a>(query: &str, candidates: &'a [String]) -> Vec<&'a String> {
    if query.is_empty() {
        return candidates.iter().collect();
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut scored: Vec<(usize, &String)> = Vec::new();
    for cand in candidates {
        if let Some(span) = match_span(&q, cand) {
            scored.push((span, cand));
        }
    }
    scored.sort_by_key(|(span, _)| *span);
    scored.into_iter().map(|(_, c)| c).collect()
}

/// If `q` is a subsequence of `cand` (case-insensitive), return the character
/// span from first to last matched position (smaller = tighter). Else None.
fn match_span(q: &[char], cand: &str) -> Option<usize> {
    let hay: Vec<char> = cand.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut first = None;
    let mut last = 0;
    for (i, ch) in hay.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            if first.is_none() {
                first = Some(i);
            }
            last = i;
            qi += 1;
        }
    }
    if qi == q.len() {
        Some(last - first.unwrap() + 1)
    } else {
        None
    }
}

/// A bound input is valid only if it names a channel that currently exists.
pub fn input_is_valid(name: &str, channel_names: &[String]) -> bool {
    !name.is_empty() && channel_names.iter().any(|n| n == name)
}
```

Now the render function. Replace `draw_script_panel` with the editor. For each existing instance, ensure a staged row exists (`state.staged.entry(id).or_insert_with(|| from the instance)`); render id (label), enabled checkbox, a script `egui::ComboBox` over `metas.keys()`, one searchable input combobox per slot (slot count = `metas[script].inputs.len()`, fallback to current staged len), output rows, and an **Apply** button (enabled only when every input `input_is_valid`) that pushes `PanelCommand::Upsert(row.to_instance())`, plus a **Remove** button pushing `PanelCommand::Remove(id)`. Below the list: an add-instance form (`new_id`, `new_script` combo) with an **Add** button that seeds a staged row (prefilling outputs from `metas[new_script]` via `OutputBinding { name: o.name.clone(), ty: type_str(o.sample_type), unit: o.unit.clone() }`), and a **Save to config** button pushing `PanelCommand::SaveConfig`.

Searchable input combobox pattern (per slot `slot`, key `k = format!("{id}#{slot}")`):

```rust
let query = state.input_query.entry(k.clone()).or_default();
egui::ComboBox::from_id_source(&k)
    .selected_text(if row.inputs[slot].is_empty() { "<pick channel>".to_string() } else { row.inputs[slot].clone() })
    .show_ui(ui, |ui| {
        ui.text_edit_singleline(query);
        for name in fuzzy_rank(query, channel_names) {
            if ui.selectable_label(&row.inputs[slot] == name, name).clicked() {
                row.inputs[slot] = name.clone();
            }
        }
    });
if !input_is_valid(&row.inputs[slot], channel_names) {
    ui.colored_label(egui::Color32::from_rgb(0xB0, 0x60, 0x00), "unknown channel");
}
```

Add a `type_str(SampleType) -> &'static str` helper (`Float=>"float"`, etc.; `Text=>"float"` defensively) for prefilling output type strings.

In `src/app.rs`: add field `script_panel_state: crate::script::panel::ScriptPanelState` (init `Default::default()` in `new`). Build the channel-name list each frame before drawing:

```rust
let channel_names: Vec<String> =
    self.channels.iter_ids().map(|id| self.channels.meta(id).name.clone()).collect();
let metas = self.script_metas.lock().unwrap().clone();
let disabled = self.script_disabled.lock().unwrap().clone();
let cmds = crate::script::panel::draw_script_panel(
    ui,
    &mut self.script_panel_state,
    &self.script_instances,
    &metas,
    &channel_names,
    &self.script_status,
    &disabled,
);
for cmd in cmds {
    self.apply_panel_command(cmd);
}
```

(`apply_panel_command` and `persist_scripts` are unchanged from Task 4.)

- [ ] **Step 4: Run the suite + a manual smoke**

Run: `cargo test` — Expected: PASS (panel helper tests + all prior).
Run: `cargo build && cargo build --no-default-features` — Expected: both clean.
Manual: `target/debug/datavis` (or with a `--demo`/`--ws-listen` source), open the Scripts panel, add an instance of `sine_rms` bound to a live channel, Apply, confirm its output channel appears in the tree and the status shows `● running`; Save and confirm `[[scripts.instances]]` is written.

- [ ] **Step 5: Commit**

```bash
git add src/script/panel.rs src/app.rs
git commit -m "feat: GUI editor for script instances with searchable channel picker"
```

---

### Task 6: Migrate shipped defaults and demo scripts

Convert the shipped `config.toml` to the instance schema and retarget the demo scripts' output names to templates so the defaults demonstrate reuse.

**Files:**
- Modify: `config.toml` (shipped default — the one committed in the repo)
- Modify: `scripts/sine_squared.py`, `scripts/sine_rms.py`

**Interfaces:** none (data + script text only).

- [ ] **Step 1: Retarget the demo scripts to templates**

In `scripts/sine_squared.py` change the `OUTPUTS` line to:

```python
OUTPUTS = [{"name": "scripts.{in0.stem}_squared", "type": "float", "unit": ""}]
```

In `scripts/sine_rms.py`:

```python
OUTPUTS = [{"name": "scripts.{in0.stem}_rms", "type": "float", "unit": ""}]
```

(Both keep their `INPUTS = ["load/ch0"]`, so the default binding expands to `scripts.ch0_squared` / `scripts.ch0_rms` — identical to today's names.)

- [ ] **Step 2: Migrate the shipped `config.toml` `[scripts]` section**

Locate the committed `config.toml` (`git ls-files config.toml`; if it is git-ignored and only a sample exists, apply the same edit to the sample). Replace the `[scripts]` section's `enabled = [...]` with instance tables reproducing current behavior, e.g.:

```toml
[scripts]
dir = "scripts"
window_s = 10.0

[[scripts.instances]]
id = "ch0_squared"
script = "sine_squared"
enabled = true

[[scripts.instances]]
id = "ch0_rms"
script = "sine_rms"
inputs = ["load/ch1"]
enabled = true
```

The second instance binds a different input to demonstrate reuse; its output expands to `scripts.ch1_rms`.

- [ ] **Step 3: Run it**

Run: `cargo build && target/debug/datavis --demo` (or the project's usual demo invocation).
Expected: no `channel id out of range` panic; the Scripts panel lists `ch0_squared` and `ch0_rms`; channels `scripts.ch0_squared` and `scripts.ch1_rms` appear in the tree.

- [ ] **Step 4: Run the full suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add config.toml scripts/sine_squared.py scripts/sine_rms.py
git commit -m "chore: migrate shipped config and demo scripts to instance bindings"
```

---

## Self-Review

**Spec coverage:**
- Template syntax `{inN}`/`{inN.stem}` → Task 1 (`expand_output_name`).
- Instances-only config, `OutputBinding`/`ScriptInstance` → Task 3; `enabled` removed → Task 4.
- Binding resolution (input override, output override, arity, template expand, validate, key by id) → Task 4 (`build_runner`).
- `Upsert`/`Remove` + per-instance owned-output release → Task 4.
- `peek_meta` + shared metas map → Tasks 2 and 4.
- Full GUI editor, searchable existence-verified picker, staged Apply, Save → Task 5.
- Bad-instance skip + surface → Task 4 (`load_into`/`build_runner` return `Failed`, others load).
- Migration of shipped config + demo scripts → Task 6.
- `scripting`-off compile path → Tasks 4 and 5 (`--no-default-features` build steps).

**Placeholder scan:** No TBD/TODO; every code step carries real code.

**Type consistency:** `ScriptInstance` fields (`id`, `script`, `inputs: Option<Vec<String>>`, `outputs: Option<Vec<OutputBinding>>`, `enabled`) are identical across Tasks 3–5. `OutputBinding { name, ty, unit }` consistent. `PanelCommand::{Upsert(ScriptInstance), Remove(String), SaveConfig}` consistent between Tasks 4–5. `ScriptCommand::{Upsert, Remove}` consistent. `expand_output_name`/`parse_sample_type` signatures match their call sites. `SharedMetas = Arc<Mutex<HashMap<String, ScriptMeta>>>` consistent between engine and app.

**Non-goals honored:** no `.py` source editing in GUI; no binding to non-existent channels; `window_s` stays global.
