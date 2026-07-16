# Data Visualizer — Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Set up the `datavis` cargo project and build the foundation layer: core types, config loading (`channels.toml` + `layout.toml`), the lockless `ChannelStore`, the `VizPanel`/`PanelRegistry` interfaces with one working panel, and a runnable eframe app fed by a demo data source.

**Architecture:** Single binary crate (`lib` + thin `main`) per the spec (`docs/superpowers/specs/2026-07-15-datavis-design.md`). This plan builds every module boundary and public interface the later subsystems plug into; ZMQ ingest, the remaining panels, egui_tiles layout engine, recorder, and replay each get their own follow-up plan and slot into the interfaces defined here.

**Tech Stack:** Rust stable, eframe/egui, serde + toml, anyhow. (Deferred to later plans: `zmq`, `prost-reflect`, `zarrs`, `rustfft`, `egui_tiles`, `crossbeam`.)

## Global Constraints

- Timestamps everywhere: **`i64` nanoseconds since Unix epoch (UTC)**.
- Hot path (`SoaRing::push`, `write_numeric`): **no locks, no heap allocation, no panics**.
- Numeric ring buffers are **typed SoA** (parallel ts/value arrays), single producer / multi reader, lock-free. Text channels use a separate `Mutex<VecDeque>` path.
- `VizPanel` code must only see `&dyn ChannelStore` — never a concrete store, never ingest/recording types.
- Two config files with different lifecycles: `channels.toml` (stable, startup) and `layout.toml` (UI-managed).
- Target platforms Linux + Windows: no unix-only APIs.
- Dependencies in this plan: `eframe`, `serde` (derive), `toml`, `anyhow` — nothing else.
- Wrong-type data bound/written anywhere must **never panic** — count it or render an inline error.
- Commit messages: plain description only. **No Co-Authored-By, no AI attribution, no emoji.**
- All tests must run headless via `cargo test` (egui via `Context::run`, no display server). Only `cargo run` needs a display.

## Module Map (crate layout after this plan)

```
data_visualizer/
├── Cargo.toml                 (package name: datavis)
├── channels.toml              example/demo channel config
├── layout.toml                example/demo layout
├── docs/…                     spec + plans (already present)
└── src/
    ├── main.rs                arg parsing, config load, eframe launch (thin)
    ├── lib.rs                 module declarations only
    ├── types.rs               ChannelId, Sample, SampleType, NumericVal,
    │                          TimeWindow, ChannelMeta, ChannelSnapshot, now_ns
    ├── config/
    │   ├── mod.rs             re-exports
    │   ├── channels.rs        ChannelConfig, ChannelRegistry (channels.toml)
    │   └── layout.rs          LayoutConfig, ScreenConfig, PanelEntry (layout.toml)
    ├── store/
    │   ├── mod.rs             ChannelStore trait + re-exports
    │   ├── ring.rs            SoaRing<T> lock-free SPMC ring
    │   ├── text.rs            TextBuf (mutex text path)
    │   └── live.rs            LiveStore (impl ChannelStore)
    ├── viz/
    │   ├── mod.rs             VizPanel trait, PanelCtor, PanelRegistry
    │   └── numeric.rs         NumericPanel (first concrete panel)
    ├── demo.rs                demo data generator thread (dev tool, kept)
    └── app.rs                 DataVisApp (eframe::App)
```

**Future plans plug in here (do NOT create these now):**

| Follow-up plan | Adds | Consumes from this plan |
|---|---|---|
| ingest | `src/ingest/` ZMQ + prost-reflect + EU scaling | `ChannelStore::write_*`, `ChannelConfig` (topic/proto_path/ts_path/eu_*) |
| viz panels + layout engine | `viz/waveform.rs` etc., egui_tiles wiring, cursors | `VizPanel`, `PanelRegistry::register`, `ChannelSnapshot`, `LayoutConfig` |
| recorder + replay | `src/recorder/`, `src/replay/` (PlaybackStore) | record queue fed beside `write_*`; `ChannelStore` trait for PlaybackStore |

---

### Task 1: Project scaffold

**Files:**
- Create: `Cargo.toml` (via cargo), `src/main.rs`, `src/lib.rs`, `.gitignore`

**Interfaces:**
- Consumes: nothing (git repo with `docs/` already exists at `/home/oni/work/data_visualizer`)
- Produces: compiling `datavis` crate with `eframe`, `serde`, `toml`, `anyhow` deps; empty `lib.rs`

- [ ] **Step 1: Initialize crate**

Run in `/home/oni/work/data_visualizer`:

```bash
cargo init --name datavis
```

Expected: creates `Cargo.toml`, `src/main.rs`, `.gitignore` (containing `/target`).

- [ ] **Step 2: Add dependencies**

```bash
cargo add eframe toml anyhow
cargo add serde --features derive
```

Expected: all four appear in `Cargo.toml [dependencies]`. Then run `cargo tree -i egui | head -n 3` — exactly one `egui` version (the one eframe re-exports).

- [ ] **Step 3: Create lib/bin split**

Create `src/lib.rs`:

```rust
// Module declarations are added task by task as modules are implemented.
```

Replace `src/main.rs`:

```rust
fn main() {
    println!("datavis foundation");
}
```

- [ ] **Step 4: Verify build**

Run: `cargo build`
Expected: compiles with no errors (warnings about unused deps are fine).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/ .gitignore
git commit -m "chore: scaffold datavis crate with eframe, serde, toml, anyhow"
```

---

### Task 2: Core types (`src/types.rs`)

**Files:**
- Create: `src/types.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)]` in `src/types.rs`

**Interfaces:**
- Consumes: nothing
- Produces (used by every later task):
  - `struct ChannelId(pub u32)` — `Copy, Eq, Hash, Ord`
  - `enum SampleType { Float, Int, Bool, Text }` — `Copy, Eq`, serde lowercase
  - `enum Sample { Float(f64), Int(i64), Bool(bool), Text(String) }` with `fn sample_type(&self) -> SampleType`
  - `enum NumericVal { Float(f64), Int(i64), Bool(bool) }` — `Copy`, with `fn as_f64(&self) -> f64` and `fn sample_type(&self) -> SampleType`
  - `struct TimeWindow { pub start_ns: i64, pub end_ns: i64 }` with `fn contains(&self, ts: i64) -> bool` (start inclusive, end exclusive) and `fn last(duration_ns: i64, now_ns: i64) -> TimeWindow`
  - `struct ChannelMeta { pub name: String, pub sample_type: SampleType, pub unit: String, pub color: String, pub max_rate: u32, pub history_s: f64, pub max_lines: usize }` — `Clone`
  - `enum ChannelSnapshot { Float { ts: Vec<i64>, vals: Vec<f64> }, Int { ts: Vec<i64>, vals: Vec<i64> }, Bool { ts: Vec<i64>, vals: Vec<u8> }, Text { lines: Vec<(i64, String)> } }` with `fn len(&self) -> usize`, `fn is_empty(&self) -> bool`
  - `fn now_ns() -> i64` — wall clock as ns since Unix epoch

- [ ] **Step 1: Write the failing tests**

Create `src/types.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_reports_its_type() {
        assert_eq!(Sample::Float(1.0).sample_type(), SampleType::Float);
        assert_eq!(Sample::Int(3).sample_type(), SampleType::Int);
        assert_eq!(Sample::Bool(true).sample_type(), SampleType::Bool);
        assert_eq!(Sample::Text("x".into()).sample_type(), SampleType::Text);
    }

    #[test]
    fn numeric_val_as_f64() {
        assert_eq!(NumericVal::Float(1.5).as_f64(), 1.5);
        assert_eq!(NumericVal::Int(3).as_f64(), 3.0);
        assert_eq!(NumericVal::Bool(true).as_f64(), 1.0);
        assert_eq!(NumericVal::Bool(false).as_f64(), 0.0);
    }

    #[test]
    fn time_window_start_inclusive_end_exclusive() {
        let w = TimeWindow { start_ns: 10, end_ns: 20 };
        assert!(w.contains(10));
        assert!(w.contains(19));
        assert!(!w.contains(20));
        assert!(!w.contains(9));
        assert_eq!(TimeWindow::last(5, 20), TimeWindow { start_ns: 15, end_ns: 20 });
    }

    #[test]
    fn sample_type_deserializes_lowercase_from_toml() {
        #[derive(serde::Deserialize)]
        struct W { t: SampleType }
        let w: W = toml::from_str(r#"t = "float""#).unwrap();
        assert_eq!(w.t, SampleType::Float);
        let w: W = toml::from_str(r#"t = "text""#).unwrap();
        assert_eq!(w.t, SampleType::Text);
    }

    #[test]
    fn snapshot_len() {
        let s = ChannelSnapshot::Float { ts: vec![1, 2], vals: vec![0.1, 0.2] };
        assert_eq!(s.len(), 2);
        assert!(!s.is_empty());
        let s = ChannelSnapshot::Text { lines: vec![] };
        assert!(s.is_empty());
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod types;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib types`
Expected: compile FAILS (`Sample` etc. not defined).

- [ ] **Step 3: Write the implementation**

Prepend to `src/types.rs` (above the test module):

```rust
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Index into the channel table built from channels.toml. Stable for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SampleType {
    Float,
    Int,
    Bool,
    Text,
}

/// Logical value at API boundaries. Never stored per-slot in the ring.
#[derive(Debug, Clone, PartialEq)]
pub enum Sample {
    Float(f64),
    Int(i64),
    Bool(bool),
    Text(String),
}

impl Sample {
    pub fn sample_type(&self) -> SampleType {
        match self {
            Sample::Float(_) => SampleType::Float,
            Sample::Int(_) => SampleType::Int,
            Sample::Bool(_) => SampleType::Bool,
            Sample::Text(_) => SampleType::Text,
        }
    }
}

/// Copy-only numeric value for the ingest hot path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumericVal {
    Float(f64),
    Int(i64),
    Bool(bool),
}

impl NumericVal {
    pub fn as_f64(&self) -> f64 {
        match self {
            NumericVal::Float(v) => *v,
            NumericVal::Int(v) => *v as f64,
            NumericVal::Bool(b) => u8::from(*b) as f64,
        }
    }

    pub fn sample_type(&self) -> SampleType {
        match self {
            NumericVal::Float(_) => SampleType::Float,
            NumericVal::Int(_) => SampleType::Int,
            NumericVal::Bool(_) => SampleType::Bool,
        }
    }
}

/// Half-open time range [start_ns, end_ns) in ns since Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    pub start_ns: i64,
    pub end_ns: i64,
}

impl TimeWindow {
    pub fn contains(&self, ts: i64) -> bool {
        ts >= self.start_ns && ts < self.end_ns
    }

    /// Window covering the last `duration_ns` ending at `now_ns`.
    pub fn last(duration_ns: i64, now_ns: i64) -> Self {
        Self { start_ns: now_ns - duration_ns, end_ns: now_ns }
    }
}

/// Display-side channel metadata (EU scale/offset stay in ChannelConfig —
/// they are consumed on ingest, panels only ever see scaled values).
#[derive(Debug, Clone)]
pub struct ChannelMeta {
    pub name: String,
    pub sample_type: SampleType,
    pub unit: String,
    pub color: String,
    pub max_rate: u32,
    pub history_s: f64,
    pub max_lines: usize,
}

/// Owned copy of a channel's samples within a window, SoA layout.
#[derive(Debug, Clone)]
pub enum ChannelSnapshot {
    Float { ts: Vec<i64>, vals: Vec<f64> },
    Int { ts: Vec<i64>, vals: Vec<i64> },
    Bool { ts: Vec<i64>, vals: Vec<u8> },
    Text { lines: Vec<(i64, String)> },
}

impl ChannelSnapshot {
    pub fn len(&self) -> usize {
        match self {
            ChannelSnapshot::Float { ts, .. } => ts.len(),
            ChannelSnapshot::Int { ts, .. } => ts.len(),
            ChannelSnapshot::Bool { ts, .. } => ts.len(),
            ChannelSnapshot::Text { lines } => lines.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Wall clock as i64 ns since Unix epoch.
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos() as i64
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib types`
Expected: 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/types.rs src/lib.rs
git commit -m "feat: core types (ChannelId, Sample, TimeWindow, ChannelMeta, ChannelSnapshot)"
```

---

### Task 3: Channel config (`src/config/channels.rs`)

**Files:**
- Create: `src/config/mod.rs`, `src/config/channels.rs`
- Modify: `src/lib.rs`
- Test: inline in `src/config/channels.rs`

**Interfaces:**
- Consumes: `types::{ChannelId, ChannelMeta, SampleType}`
- Produces:
  - `struct ChannelConfig { pub topic: String, pub proto_path: String, pub ts_path: String, pub sample_type: SampleType, pub unit: String, pub color: String, pub max_rate: u32, pub history_s: f64, pub eu_scale: f64, pub eu_offset: f64, pub max_lines: usize }`
  - `struct ChannelRegistry` with:
    - `fn from_toml_str(s: &str) -> anyhow::Result<ChannelRegistry>`
    - `fn load(path: &std::path::Path) -> anyhow::Result<ChannelRegistry>`
    - `fn id(&self, name: &str) -> Option<ChannelId>`
    - `fn meta(&self, id: ChannelId) -> &ChannelMeta`
    - `fn config(&self, id: ChannelId) -> &ChannelConfig`
    - `fn len(&self) -> usize` / `fn is_empty(&self) -> bool`
    - `fn iter_ids(&self) -> impl Iterator<Item = ChannelId> + '_`
  - `ChannelId`s are assigned 0..n over channel names in **sorted order** (deterministic across runs).

- [ ] **Step 1: Write the failing tests**

Create `src/config/mod.rs`:

```rust
pub mod channels;

pub use channels::{ChannelConfig, ChannelRegistry};
```

Add to `src/lib.rs`:

```rust
pub mod config;
```

Create `src/config/channels.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SampleType;

    const EXAMPLE: &str = r##"
[channels."sensor.acceleration.x"]
topic      = "accel"
proto_path = "AccelBatch.samples.x"
ts_path    = "AccelBatch.samples.t_ns"
type       = "float"
unit       = "m/s²"
color      = "#ff0000"
max_rate   = 100000
history_s  = 10.0
eu_scale   = 2.5
eu_offset  = -1.0

[channels."motor.state"]
topic      = "status"
proto_path = "StatusBatch.samples.state"
ts_path    = "StatusBatch.samples.t_ns"
type       = "int"
max_rate   = 1000
history_s  = 30.0

[channels."system.log"]
topic      = "log"
proto_path = "LogBatch.samples.message"
ts_path    = "LogBatch.samples.t_ns"
type       = "text"
max_lines  = 500
"##;

    #[test]
    fn parses_example_and_assigns_sorted_ids() {
        let reg = ChannelRegistry::from_toml_str(EXAMPLE).unwrap();
        assert_eq!(reg.len(), 3);
        // sorted name order: motor.state, sensor.acceleration.x, system.log
        let motor = reg.id("motor.state").unwrap();
        let accel = reg.id("sensor.acceleration.x").unwrap();
        let log = reg.id("system.log").unwrap();
        assert!(motor < accel && accel < log);
        assert_eq!(reg.id("nope"), None);
    }

    #[test]
    fn meta_and_config_expose_fields() {
        let reg = ChannelRegistry::from_toml_str(EXAMPLE).unwrap();
        let accel = reg.id("sensor.acceleration.x").unwrap();
        let meta = reg.meta(accel);
        assert_eq!(meta.name, "sensor.acceleration.x");
        assert_eq!(meta.sample_type, SampleType::Float);
        assert_eq!(meta.unit, "m/s²");
        assert_eq!(meta.max_rate, 100_000);
        let cfg = reg.config(accel);
        assert_eq!(cfg.topic, "accel");
        assert_eq!(cfg.eu_scale, 2.5);
        assert_eq!(cfg.eu_offset, -1.0);
    }

    #[test]
    fn defaults_applied() {
        let reg = ChannelRegistry::from_toml_str(EXAMPLE).unwrap();
        let motor = reg.id("motor.state").unwrap();
        let cfg = reg.config(motor);
        assert_eq!(cfg.eu_scale, 1.0);
        assert_eq!(cfg.eu_offset, 0.0);
        assert_eq!(cfg.unit, "");
        let log = reg.id("system.log").unwrap();
        assert_eq!(reg.config(log).max_lines, 500);
        assert_eq!(reg.meta(log).max_lines, 500);
    }

    #[test]
    fn unknown_type_is_error() {
        let bad = r#"
[channels."a"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "complex"
"#;
        assert!(ChannelRegistry::from_toml_str(bad).is_err());
    }

    #[test]
    fn unknown_field_is_error() {
        let bad = r#"
[channels."a"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
typo_field = 1
"#;
        assert!(ChannelRegistry::from_toml_str(bad).is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::channels`
Expected: compile FAILS (`ChannelRegistry` not defined).

- [ ] **Step 3: Write the implementation**

Prepend to `src/config/channels.rs`:

```rust
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::types::{ChannelId, ChannelMeta, SampleType};

/// One channel entry from channels.toml. eu_scale/eu_offset are consumed by
/// ingest; everything display-relevant is mirrored into ChannelMeta.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub topic: String,
    pub proto_path: String,
    pub ts_path: String,
    #[serde(rename = "type")]
    pub sample_type: SampleType,
    #[serde(default)]
    pub unit: String,
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default = "default_max_rate")]
    pub max_rate: u32,
    #[serde(default = "default_history_s")]
    pub history_s: f64,
    #[serde(default = "default_eu_scale")]
    pub eu_scale: f64,
    #[serde(default)]
    pub eu_offset: f64,
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
}

fn default_color() -> String {
    "#cccccc".to_string()
}
fn default_max_rate() -> u32 {
    1000
}
fn default_history_s() -> f64 {
    10.0
}
fn default_eu_scale() -> f64 {
    1.0
}
fn default_max_lines() -> usize {
    500
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelsFile {
    // BTreeMap: sorted names → deterministic ChannelId assignment.
    channels: BTreeMap<String, ChannelConfig>,
}

/// Immutable channel table built once at startup from channels.toml.
#[derive(Debug)]
pub struct ChannelRegistry {
    ids: HashMap<String, ChannelId>,
    configs: Vec<ChannelConfig>,
    metas: Vec<ChannelMeta>,
}

impl ChannelRegistry {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let file: ChannelsFile = toml::from_str(s).context("parsing channels.toml")?;
        let mut ids = HashMap::new();
        let mut configs = Vec::new();
        let mut metas = Vec::new();
        for (i, (name, cfg)) in file.channels.into_iter().enumerate() {
            ids.insert(name.clone(), ChannelId(i as u32));
            metas.push(ChannelMeta {
                name,
                sample_type: cfg.sample_type,
                unit: cfg.unit.clone(),
                color: cfg.color.clone(),
                max_rate: cfg.max_rate,
                history_s: cfg.history_s,
                max_lines: cfg.max_lines,
            });
            configs.push(cfg);
        }
        Ok(Self { ids, configs, metas })
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml_str(&s)
    }

    pub fn id(&self, name: &str) -> Option<ChannelId> {
        self.ids.get(name).copied()
    }

    pub fn meta(&self, id: ChannelId) -> &ChannelMeta {
        &self.metas[id.0 as usize]
    }

    pub fn config(&self, id: ChannelId) -> &ChannelConfig {
        &self.configs[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.metas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.metas.is_empty()
    }

    pub fn iter_ids(&self) -> impl Iterator<Item = ChannelId> + '_ {
        (0..self.metas.len() as u32).map(ChannelId)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config::channels`
Expected: 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config/ src/lib.rs
git commit -m "feat: channels.toml parsing and ChannelRegistry"
```

---

### Task 4: Layout config (`src/config/layout.rs`)

**Files:**
- Create: `src/config/layout.rs`
- Modify: `src/config/mod.rs`
- Test: inline in `src/config/layout.rs`

**Interfaces:**
- Consumes: nothing from other modules (pure toml structures)
- Produces:
  - `struct LayoutConfig { pub screens: std::collections::BTreeMap<String, ScreenConfig> }` with `from_toml_str`, `to_toml_string`, `load(&Path)`, `save(&Path)` (all `anyhow::Result`)
  - `struct ScreenConfig { pub panels: Vec<PanelEntry> }`
  - `struct PanelEntry { pub panel_type: String /* toml key "type" */, pub config: toml::Table /* all other keys, flattened */ }`

- [ ] **Step 1: Write the failing tests**

Create `src/config/layout.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[screens.main]
[[screens.main.panels]]
type = "waveform"
channels = ["sensor.accel.x", "sensor.accel.y"]
time_window_s = 5.0
cursors = true

[[screens.main.panels]]
type = "log"
channels = ["system.log"]
max_lines = 500
"#;

    #[test]
    fn parses_screens_and_panels() {
        let l = LayoutConfig::from_toml_str(EXAMPLE).unwrap();
        assert_eq!(l.screens.len(), 1);
        let main = &l.screens["main"];
        assert_eq!(main.panels.len(), 2);
        assert_eq!(main.panels[0].panel_type, "waveform");
        assert_eq!(
            main.panels[0].config["time_window_s"],
            toml::Value::Float(5.0)
        );
        assert_eq!(main.panels[0].config["cursors"], toml::Value::Boolean(true));
        assert!(!main.panels[0].config.contains_key("type"));
        assert_eq!(main.panels[1].panel_type, "log");
    }

    #[test]
    fn round_trips() {
        let l = LayoutConfig::from_toml_str(EXAMPLE).unwrap();
        let s = l.to_toml_string().unwrap();
        let l2 = LayoutConfig::from_toml_str(&s).unwrap();
        assert_eq!(l, l2);
    }

    #[test]
    fn empty_screen_is_ok() {
        let l = LayoutConfig::from_toml_str("[screens.empty]\n").unwrap();
        assert!(l.screens["empty"].panels.is_empty());
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join("datavis_layout_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layout.toml");
        let l = LayoutConfig::from_toml_str(EXAMPLE).unwrap();
        l.save(&path).unwrap();
        let l2 = LayoutConfig::load(&path).unwrap();
        assert_eq!(l, l2);
    }
}
```

Add to `src/config/mod.rs`:

```rust
pub mod layout;

pub use layout::{LayoutConfig, PanelEntry, ScreenConfig};
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::layout`
Expected: compile FAILS (`LayoutConfig` not defined).

- [ ] **Step 3: Write the implementation**

Prepend to `src/config/layout.rs`:

```rust
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// layout.toml — screens with panel lists. Panel-specific settings stay an
/// opaque toml::Table here; the viz PanelRegistry interprets them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LayoutConfig {
    #[serde(default)]
    pub screens: BTreeMap<String, ScreenConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScreenConfig {
    #[serde(default)]
    pub panels: Vec<PanelEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelEntry {
    #[serde(rename = "type")]
    pub panel_type: String,
    #[serde(flatten)]
    pub config: toml::Table,
}

impl LayoutConfig {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        toml::from_str(s).context("parsing layout.toml")
    }

    pub fn to_toml_string(&self) -> anyhow::Result<String> {
        toml::to_string_pretty(self).context("serializing layout")
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml_str(&s)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        std::fs::write(path, self.to_toml_string()?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config::layout`
Expected: 4 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config/
git commit -m "feat: layout.toml parsing with opaque per-panel config tables"
```

---

### Task 5: Lock-free SoA ring buffer (`src/store/ring.rs`)

**Files:**
- Create: `src/store/mod.rs` (module decl only for now), `src/store/ring.rs`
- Modify: `src/lib.rs`
- Test: inline in `src/store/ring.rs`

**Interfaces:**
- Consumes: `types::TimeWindow`
- Produces:
  - `struct SoaRing<T: Copy + Default + Send>` with:
    - `fn new(min_capacity: usize) -> SoaRing<T>` — rounds capacity up to a power of two, min 16
    - `fn push(&self, ts: i64, val: T)` — **single producer only**, lock-free, no alloc
    - `fn read_window(&self, window: TimeWindow, out_ts: &mut Vec<i64>, out_vals: &mut Vec<T>)` — any thread; clears + fills outputs; retries if lapped by producer
    - `fn latest(&self) -> Option<(i64, T)>`
    - `fn capacity(&self) -> usize`, `fn visible_capacity(&self) -> usize` (capacity minus overwrite-guard margin = cap/8)
  - Requirement: timestamps must be pushed in non-decreasing order (ingest guarantees per-channel order); `read_window` binary-searches on that assumption.

- [ ] **Step 1: Write the failing tests**

Create `src/store/mod.rs`:

```rust
pub mod ring;

pub use ring::SoaRing;
```

Add to `src/lib.rs`:

```rust
pub mod store;
```

Create `src/store/ring.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TimeWindow;
    use std::sync::Arc;

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    #[test]
    fn empty_ring_reads_nothing() {
        let r: SoaRing<f64> = SoaRing::new(16);
        assert_eq!(r.latest(), None);
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        r.read_window(ALL, &mut ts, &mut vals);
        assert!(ts.is_empty() && vals.is_empty());
    }

    #[test]
    fn push_then_read_all_in_order() {
        let r: SoaRing<f64> = SoaRing::new(256);
        for i in 0..100i64 {
            r.push(i, i as f64);
        }
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        r.read_window(ALL, &mut ts, &mut vals);
        assert_eq!(ts.len(), 100);
        assert_eq!(ts[0], 0);
        assert_eq!(ts[99], 99);
        assert_eq!(vals[99], 99.0);
    }

    #[test]
    fn window_selects_subrange() {
        let r: SoaRing<i64> = SoaRing::new(256);
        for i in 0..100i64 {
            r.push(i, i * 10);
        }
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        r.read_window(TimeWindow { start_ns: 25, end_ns: 75 }, &mut ts, &mut vals);
        assert_eq!(ts.first(), Some(&25));
        assert_eq!(ts.last(), Some(&74)); // end exclusive
        assert_eq!(vals.first(), Some(&250));
    }

    #[test]
    fn wraparound_keeps_newest_visible_capacity() {
        let r: SoaRing<i64> = SoaRing::new(64); // cap 64, margin 8 → visible 56
        assert_eq!(r.capacity(), 64);
        assert_eq!(r.visible_capacity(), 56);
        for i in 0..300i64 {
            r.push(i, i);
        }
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        r.read_window(ALL, &mut ts, &mut vals);
        assert_eq!(ts.len(), 56);
        assert_eq!(*ts.last().unwrap(), 299);
        assert_eq!(*ts.first().unwrap(), 299 - 55);
        for w in ts.windows(2) {
            assert_eq!(w[1], w[0] + 1);
        }
    }

    #[test]
    fn latest_returns_last_pushed() {
        let r: SoaRing<f64> = SoaRing::new(16);
        r.push(5, 1.25);
        r.push(9, 2.5);
        assert_eq!(r.latest(), Some((9, 2.5)));
    }

    #[test]
    fn concurrent_producer_reader_no_torn_reads() {
        let ring = Arc::new(SoaRing::<f64>::new(4096));
        let producer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                for i in 0..1_000_000i64 {
                    ring.push(i, i as f64);
                }
            })
        };
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        while !producer.is_finished() {
            ring.read_window(ALL, &mut ts, &mut vals);
            for (i, (&t, &v)) in ts.iter().zip(vals.iter()).enumerate() {
                assert_eq!(v, t as f64, "torn ts/val pair at index {i}");
                if i > 0 {
                    assert!(t == ts[i - 1] + 1, "gap or reorder inside snapshot");
                }
            }
        }
        producer.join().unwrap();
        ring.read_window(ALL, &mut ts, &mut vals);
        assert_eq!(*ts.last().unwrap(), 999_999);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib store::ring`
Expected: compile FAILS (`SoaRing` not defined).

- [ ] **Step 3: Write the implementation**

Prepend to `src/store/ring.rs`:

```rust
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::TimeWindow;

/// Lock-free single-producer / multi-reader ring with timestamps and values
/// in parallel arrays (SoA). `head` counts total samples ever pushed; slot
/// index is `seq & (cap - 1)`.
///
/// Readers copy optimistically and validate against `head` afterwards
/// (seqlock style): if the producer wrote far enough to have overwritten any
/// slot the reader touched, the reader discards and retries. The newest
/// `cap/8` slots are reserved as an overwrite guard so a reader is never
/// chasing the producer's write position slot-by-slot.
pub struct SoaRing<T: Copy> {
    cap: usize,
    margin: u64,
    ts: Box<[UnsafeCell<i64>]>,
    vals: Box<[UnsafeCell<T>]>,
    head: AtomicU64,
}

// Safety: readers only dereference slots they subsequently validate against
// `head`; invalid (possibly torn) copies are discarded before use.
unsafe impl<T: Copy + Send> Send for SoaRing<T> {}
unsafe impl<T: Copy + Send> Sync for SoaRing<T> {}

impl<T: Copy + Default> SoaRing<T> {
    /// `min_capacity` rounds up to a power of two, minimum 16.
    pub fn new(min_capacity: usize) -> Self {
        let cap = min_capacity.max(16).next_power_of_two();
        let ts = (0..cap)
            .map(|_| UnsafeCell::new(0i64))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let vals = (0..cap)
            .map(|_| UnsafeCell::new(T::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { cap, margin: (cap / 8) as u64, ts, vals, head: AtomicU64::new(0) }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Samples a reader is guaranteed to be able to snapshot.
    pub fn visible_capacity(&self) -> usize {
        self.cap - self.margin as usize
    }

    #[inline]
    fn slot(&self, seq: u64) -> usize {
        (seq as usize) & (self.cap - 1)
    }

    /// Single producer only. Lock-free, allocation-free.
    pub fn push(&self, ts: i64, val: T) {
        let head = self.head.load(Ordering::Relaxed);
        let idx = self.slot(head);
        unsafe {
            *self.ts[idx].get() = ts;
            *self.vals[idx].get() = val;
        }
        self.head.store(head + 1, Ordering::Release);
    }

    pub fn latest(&self) -> Option<(i64, T)> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            if head == 0 {
                return None;
            }
            let idx = self.slot(head - 1);
            let ts = unsafe { *self.ts[idx].get() };
            let val = unsafe { *self.vals[idx].get() };
            let head2 = self.head.load(Ordering::Acquire);
            // Slot (head-1) is dirtied once the producer starts writing
            // seq (head-1)+cap, which implies head2 >= head-1+cap.
            if head2 < (head - 1) + self.cap as u64 {
                return Some((ts, val));
            }
        }
    }

    /// Copies all samples with ts in [window.start_ns, window.end_ns) into
    /// the output vectors (cleared first), oldest first. Assumes timestamps
    /// were pushed in non-decreasing order.
    pub fn read_window(&self, window: TimeWindow, out_ts: &mut Vec<i64>, out_vals: &mut Vec<T>) {
        loop {
            out_ts.clear();
            out_vals.clear();
            let head = self.head.load(Ordering::Acquire);
            if head == 0 {
                return;
            }
            let valid_lo = head.saturating_sub(self.cap as u64 - self.margin);
            // Binary search: first seq in [valid_lo, head) with ts >= start.
            let (mut lo, mut hi) = (valid_lo, head);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let ts = unsafe { *self.ts[self.slot(mid)].get() };
                if ts < window.start_ns {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            for seq in lo..head {
                let ts = unsafe { *self.ts[self.slot(seq)].get() };
                if ts >= window.end_ns {
                    break;
                }
                out_ts.push(ts);
                out_vals.push(unsafe { *self.vals[self.slot(seq)].get() });
            }
            let head2 = self.head.load(Ordering::Acquire);
            // Every slot we touched has seq >= valid_lo. Those slots stay
            // clean as long as the producer has not begun seq valid_lo+cap.
            if head2 < valid_lo + self.cap as u64 {
                return;
            }
            // Lapped mid-read — discard and retry.
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib store::ring`
Expected: 6 tests PASS (concurrent test may take a few seconds).

- [ ] **Step 5: Run the concurrent test repeatedly to shake out races**

Run: `for i in $(seq 10); do cargo test --lib store::ring::tests::concurrent -- --nocapture || break; done`
Expected: 10× PASS, no assertion failures.

- [ ] **Step 6: Commit**

```bash
git add src/store/ src/lib.rs
git commit -m "feat: lock-free SoA ring buffer with seqlock-style readers"
```

---

### Task 6: ChannelStore trait, text path, LiveStore (`src/store/`)

**Files:**
- Create: `src/store/text.rs`, `src/store/live.rs`
- Modify: `src/store/mod.rs`
- Test: inline in `src/store/text.rs` and `src/store/live.rs`

**Interfaces:**
- Consumes: `SoaRing<T>` (Task 5), `ChannelRegistry` (Task 3), types (Task 2)
- Produces:
  - The central trait (in `src/store/mod.rs`) — **exact signatures, later plans implement/consume this**:
    ```rust
    pub trait ChannelStore: Send + Sync {
        fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal);
        fn write_text(&self, channel: ChannelId, ts: i64, line: String);
        fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot;
        fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)>;
        fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta;
    }
    ```
  - `struct TextBuf` — `fn new(max_lines: usize)`, `fn push(&self, ts: i64, line: String)`, `fn window(&self, w: TimeWindow) -> Vec<(i64, String)>`, `fn latest(&self) -> Option<(i64, String)>`
  - `struct LiveStore` — `fn from_registry(reg: &ChannelRegistry) -> LiveStore` (ring capacity = `max_rate × history_s × 1.2`, so the guard margin never eats configured history), `fn type_errors(&self) -> u64`; implements `ChannelStore`. Type-mismatched writes are counted and dropped, never panic.

- [ ] **Step 1: Write the failing TextBuf tests**

Create `src/store/text.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TimeWindow;

    #[test]
    fn push_window_latest() {
        let t = TextBuf::new(10);
        t.push(1, "a".into());
        t.push(5, "b".into());
        t.push(9, "c".into());
        assert_eq!(t.latest(), Some((9, "c".to_string())));
        let w = t.window(TimeWindow { start_ns: 2, end_ns: 9 });
        assert_eq!(w, vec![(5, "b".to_string())]);
    }

    #[test]
    fn bounded_drops_oldest() {
        let t = TextBuf::new(3);
        for i in 0..5i64 {
            t.push(i, format!("line{i}"));
        }
        let w = t.window(TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].0, 2);
        assert_eq!(w[2].0, 4);
    }
}
```

Add to `src/store/mod.rs`:

```rust
pub mod text;

pub use text::TextBuf;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib store::text`
Expected: compile FAILS.

- [ ] **Step 3: Implement TextBuf**

Prepend to `src/store/text.rs`:

```rust
use std::collections::VecDeque;
use std::sync::Mutex;

use crate::types::TimeWindow;

/// Bounded text-channel buffer. Low rate by design — a mutex is fine here
/// and keeps String allocation off the numeric hot path.
pub struct TextBuf {
    max_lines: usize,
    lines: Mutex<VecDeque<(i64, String)>>,
}

impl TextBuf {
    pub fn new(max_lines: usize) -> Self {
        Self { max_lines: max_lines.max(1), lines: Mutex::new(VecDeque::new()) }
    }

    pub fn push(&self, ts: i64, line: String) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() == self.max_lines {
            lines.pop_front();
        }
        lines.push_back((ts, line));
    }

    pub fn window(&self, w: TimeWindow) -> Vec<(i64, String)> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(ts, _)| w.contains(*ts))
            .cloned()
            .collect()
    }

    pub fn latest(&self) -> Option<(i64, String)> {
        self.lines.lock().unwrap().back().cloned()
    }
}
```

- [ ] **Step 4: Run TextBuf tests**

Run: `cargo test --lib store::text`
Expected: 2 tests PASS.

- [ ] **Step 5: Write the failing LiveStore tests**

Create `src/store/live.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::types::{NumericVal, Sample, SampleType, TimeWindow};

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."a.float"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
max_rate = 100
history_s = 1.0

[channels."b.int"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"
max_rate = 100
history_s = 1.0

[channels."c.bool"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "bool"
max_rate = 100
history_s = 1.0

[channels."d.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
max_lines = 10
"#,
        )
        .unwrap()
    }

    #[test]
    fn write_and_snapshot_each_type() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let (fa, ib, bc, td) = (
            reg.id("a.float").unwrap(),
            reg.id("b.int").unwrap(),
            reg.id("c.bool").unwrap(),
            reg.id("d.log").unwrap(),
        );
        store.write_numeric(fa, 1, NumericVal::Float(1.5));
        store.write_numeric(ib, 2, NumericVal::Int(-7));
        store.write_numeric(bc, 3, NumericVal::Bool(true));
        store.write_text(td, 4, "hello".into());

        match store.snapshot(fa, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1]);
                assert_eq!(vals, vec![1.5]);
            }
            other => panic!("wrong snapshot variant: {other:?}"),
        }
        match store.snapshot(bc, ALL) {
            ChannelSnapshot::Bool { vals, .. } => assert_eq!(vals, vec![1u8]),
            other => panic!("wrong snapshot variant: {other:?}"),
        }
        match store.snapshot(td, ALL) {
            ChannelSnapshot::Text { lines } => {
                assert_eq!(lines, vec![(4, "hello".to_string())])
            }
            other => panic!("wrong snapshot variant: {other:?}"),
        }
        assert_eq!(store.latest(ib), Some((2, Sample::Int(-7))));
        assert_eq!(store.latest(bc), Some((3, Sample::Bool(true))));
        assert_eq!(store.channel_meta(fa).sample_type, SampleType::Float);
    }

    #[test]
    fn type_mismatch_is_counted_not_panicking() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let fa = reg.id("a.float").unwrap();
        let td = reg.id("d.log").unwrap();
        store.write_numeric(fa, 1, NumericVal::Int(3)); // Int into Float channel
        store.write_numeric(td, 2, NumericVal::Float(1.0)); // numeric into text
        store.write_text(fa, 3, "oops".into()); // text into numeric
        assert_eq!(store.type_errors(), 3);
        assert!(store.snapshot(fa, ALL).is_empty());
        assert_eq!(store.latest(fa), None);
    }

    #[test]
    fn ring_sized_from_config_with_headroom() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let fa = reg.id("a.float").unwrap();
        // max_rate 100 × history 1.0 s × 1.2 = 120 → cap 128, visible 112 ≥ 100
        for i in 0..200i64 {
            store.write_numeric(fa, i, NumericVal::Float(i as f64));
        }
        let snap = store.snapshot(fa, ALL);
        assert!(snap.len() >= 100, "visible history below configured depth");
    }
}
```

- [ ] **Step 6: Run tests to verify they fail**

Run: `cargo test --lib store::live`
Expected: compile FAILS (`LiveStore` not defined).

- [ ] **Step 7: Implement the trait and LiveStore**

Replace `src/store/mod.rs` with:

```rust
pub mod live;
pub mod ring;
pub mod text;

pub use live::LiveStore;
pub use ring::SoaRing;
pub use text::TextBuf;

use crate::types::{ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, TimeWindow};

/// The one interface viz panels see. Implemented by LiveStore (this plan)
/// and PlaybackStore (replay plan). Writers: ingest thread (live) or the
/// replay engine. Readers: main thread panels.
pub trait ChannelStore: Send + Sync {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal);
    fn write_text(&self, channel: ChannelId, ts: i64, line: String);
    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot;
    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)>;
    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta;
}
```

Prepend to `src/store/live.rs`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::ChannelRegistry;
use crate::store::{ChannelStore, SoaRing, TextBuf};
use crate::types::{
    ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, SampleType, TimeWindow,
};

enum ChannelData {
    Float(SoaRing<f64>),
    Int(SoaRing<i64>),
    Bool(SoaRing<u8>),
    Text(TextBuf),
}

struct ChannelSlot {
    meta: ChannelMeta,
    data: ChannelData,
}

/// Live store: one typed slot per configured channel, indexed by ChannelId.
pub struct LiveStore {
    channels: Vec<ChannelSlot>,
    type_errors: AtomicU64,
}

impl LiveStore {
    pub fn from_registry(reg: &ChannelRegistry) -> Self {
        let channels = reg
            .iter_ids()
            .map(|id| {
                let meta = reg.meta(id).clone();
                let cfg = reg.config(id);
                // 1.2× headroom so the ring's cap/8 reader guard margin
                // never cuts into the configured history depth.
                let cap = (cfg.max_rate as f64 * cfg.history_s * 1.2).ceil() as usize;
                let data = match meta.sample_type {
                    SampleType::Float => ChannelData::Float(SoaRing::new(cap)),
                    SampleType::Int => ChannelData::Int(SoaRing::new(cap)),
                    SampleType::Bool => ChannelData::Bool(SoaRing::new(cap)),
                    SampleType::Text => ChannelData::Text(TextBuf::new(cfg.max_lines)),
                };
                ChannelSlot { meta, data }
            })
            .collect();
        Self { channels, type_errors: AtomicU64::new(0) }
    }

    /// Count of writes dropped because value type didn't match channel type.
    pub fn type_errors(&self) -> u64 {
        self.type_errors.load(Ordering::Relaxed)
    }

    fn slot(&self, id: ChannelId) -> &ChannelSlot {
        &self.channels[id.0 as usize]
    }

    fn count_type_error(&self) {
        self.type_errors.fetch_add(1, Ordering::Relaxed);
    }
}

impl ChannelStore for LiveStore {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal) {
        match (&self.slot(channel).data, val) {
            (ChannelData::Float(r), NumericVal::Float(v)) => r.push(ts, v),
            (ChannelData::Int(r), NumericVal::Int(v)) => r.push(ts, v),
            (ChannelData::Bool(r), NumericVal::Bool(v)) => r.push(ts, u8::from(v)),
            _ => self.count_type_error(),
        }
    }

    fn write_text(&self, channel: ChannelId, ts: i64, line: String) {
        match &self.slot(channel).data {
            ChannelData::Text(t) => t.push(ts, line),
            _ => self.count_type_error(),
        }
    }

    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot {
        match &self.slot(channel).data {
            ChannelData::Float(r) => {
                let (mut ts, mut vals) = (Vec::new(), Vec::new());
                r.read_window(window, &mut ts, &mut vals);
                ChannelSnapshot::Float { ts, vals }
            }
            ChannelData::Int(r) => {
                let (mut ts, mut vals) = (Vec::new(), Vec::new());
                r.read_window(window, &mut ts, &mut vals);
                ChannelSnapshot::Int { ts, vals }
            }
            ChannelData::Bool(r) => {
                let (mut ts, mut vals) = (Vec::new(), Vec::new());
                r.read_window(window, &mut ts, &mut vals);
                ChannelSnapshot::Bool { ts, vals }
            }
            ChannelData::Text(t) => ChannelSnapshot::Text { lines: t.window(window) },
        }
    }

    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)> {
        match &self.slot(channel).data {
            ChannelData::Float(r) => r.latest().map(|(t, v)| (t, Sample::Float(v))),
            ChannelData::Int(r) => r.latest().map(|(t, v)| (t, Sample::Int(v))),
            ChannelData::Bool(r) => r.latest().map(|(t, v)| (t, Sample::Bool(v != 0))),
            ChannelData::Text(t) => t.latest().map(|(ts, l)| (ts, Sample::Text(l))),
        }
    }

    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta {
        &self.slot(channel).meta
    }
}
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test --lib store`
Expected: ring (6) + text (2) + live (3) tests PASS.

- [ ] **Step 9: Commit**

```bash
git add src/store/
git commit -m "feat: ChannelStore trait, text path, LiveStore over typed rings"
```

---

### Task 7: VizPanel trait, PanelRegistry, NumericPanel (`src/viz/`)

**Files:**
- Create: `src/viz/mod.rs`, `src/viz/numeric.rs`
- Modify: `src/lib.rs`
- Test: inline in `src/viz/mod.rs` and `src/viz/numeric.rs`

**Interfaces:**
- Consumes: `ChannelStore` trait, `LiveStore` (tests), `ChannelRegistry`, `PanelEntry`, types
- Produces (the panel plan builds every other panel against exactly this):
  ```rust
  pub trait VizPanel {
      fn title(&self) -> &str;
      fn accepted_types(&self) -> &[SampleType];
      fn config_ui(&mut self, ui: &mut egui::Ui);
      fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore);
      fn serialize(&self) -> toml::Table; // panel-specific keys only, no "type"
  }

  pub type PanelCtor =
      fn(&toml::Table, &ChannelRegistry) -> anyhow::Result<Box<dyn VizPanel>>;

  pub struct PanelRegistry { /* ctors: HashMap<&'static str, PanelCtor> */ }
  // fn with_builtins() -> PanelRegistry            (registers "numeric")
  // fn register(&mut self, name: &'static str, ctor: PanelCtor)
  // fn build(&self, entry: &PanelEntry, channels: &ChannelRegistry)
  //     -> anyhow::Result<Box<dyn VizPanel>>
  ```
  - `viz::numeric::TYPE_NAME: &str = "numeric"`; config keys: `channel` (required string). Unknown channel name or wrong channel type → panel constructs fine and renders an inline error (never panics, never fails the whole layout).
  - egui is used via the `eframe::egui` re-export everywhere (`use eframe::egui;`).

- [ ] **Step 1: Write the failing tests**

Create `src/viz/mod.rs` with declarations + tests (trait/registry code comes in Step 3):

```rust
pub mod numeric;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use eframe::egui;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
unit = "V"
max_rate = 100
history_s = 1.0

[channels."demo.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap()
    }

    fn entry(toml_src: &str) -> PanelEntry {
        toml::from_str(toml_src).unwrap()
    }

    #[test]
    fn builds_numeric_panel_from_entry() {
        let channels = registry();
        let panels = PanelRegistry::with_builtins();
        let e = entry(r#"type = "numeric"
channel = "demo.sine""#);
        let p = panels.build(&e, &channels).unwrap();
        assert_eq!(p.title(), "demo.sine");
    }

    #[test]
    fn unknown_panel_type_is_error() {
        let channels = registry();
        let panels = PanelRegistry::with_builtins();
        let e = entry(r#"type = "hologram"
channel = "demo.sine""#);
        assert!(panels.build(&e, &channels).is_err());
    }

    #[test]
    fn serialize_round_trips_through_registry() {
        let channels = registry();
        let panels = PanelRegistry::with_builtins();
        let e = entry(r#"type = "numeric"
channel = "demo.sine""#);
        let p = panels.build(&e, &channels).unwrap();
        let cfg = p.serialize();
        assert_eq!(cfg, e.config); // same panel-specific keys back out
        let e2 = PanelEntry { panel_type: "numeric".into(), config: cfg };
        assert!(panels.build(&e2, &channels).is_ok());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        store.write_numeric(id, 1, NumericVal::Float(3.25));

        let panels = PanelRegistry::with_builtins();
        // valid binding, missing channel, and wrong-type binding must all
        // render an inline result/error — never panic.
        let sources = [
            r#"type = "numeric"
channel = "demo.sine""#,
            r#"type = "numeric"
channel = "does.not.exist""#,
            r#"type = "numeric"
channel = "demo.log""#,
        ];
        for src in sources {
            let mut p = panels.build(&entry(src), &channels).unwrap();
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    p.render(ui, &store);
                    p.config_ui(ui);
                });
            });
        }
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod viz;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib viz`
Expected: compile FAILS (`PanelRegistry`, `VizPanel` not defined).

- [ ] **Step 3: Implement trait + registry**

Prepend to `src/viz/mod.rs` (above `pub mod numeric;`):

```rust
use std::collections::HashMap;

use anyhow::anyhow;
use eframe::egui;

use crate::config::{ChannelRegistry, PanelEntry};
use crate::store::ChannelStore;
use crate::types::SampleType;

/// A visualization panel. Panels only see the ChannelStore trait — live vs
/// replay is transparent here.
pub trait VizPanel {
    fn title(&self) -> &str;
    fn accepted_types(&self) -> &[SampleType];
    /// Panel settings UI (shown in a config popup/side area).
    fn config_ui(&mut self, ui: &mut egui::Ui);
    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore);
    /// Panel-specific config keys for layout.toml. Must NOT include "type" —
    /// PanelEntry carries that.
    fn serialize(&self) -> toml::Table;
}

/// Constructor: panel-specific toml table + channel registry (for resolving
/// channel names to ids) → boxed panel. Binding problems (unknown channel,
/// wrong type) must produce a panel that renders an inline error, not Err —
/// Err is for malformed config only (e.g. missing required key).
pub type PanelCtor =
    fn(&toml::Table, &ChannelRegistry) -> anyhow::Result<Box<dyn VizPanel>>;

/// Maps layout.toml `type` strings to constructors. Later plans call
/// `register` for each new panel type.
pub struct PanelRegistry {
    ctors: HashMap<&'static str, PanelCtor>,
}

impl PanelRegistry {
    pub fn with_builtins() -> Self {
        let mut reg = Self { ctors: HashMap::new() };
        reg.register(numeric::TYPE_NAME, numeric::ctor);
        reg
    }

    pub fn register(&mut self, name: &'static str, ctor: PanelCtor) {
        self.ctors.insert(name, ctor);
    }

    pub fn build(
        &self,
        entry: &PanelEntry,
        channels: &ChannelRegistry,
    ) -> anyhow::Result<Box<dyn VizPanel>> {
        let ctor = self
            .ctors
            .get(entry.panel_type.as_str())
            .ok_or_else(|| anyhow!("unknown panel type `{}`", entry.panel_type))?;
        ctor(&entry.config, channels)
    }
}
```

- [ ] **Step 4: Implement NumericPanel**

Create `src/viz/numeric.rs`:

```rust
use anyhow::anyhow;
use eframe::egui;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, Sample, SampleType};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "numeric";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int, SampleType::Bool];

/// Large latest-value display with unit label.
pub struct NumericPanel {
    channel_name: String,
    channel: Option<ChannelId>,
    type_ok: bool,
    unit: String,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let channel_name = cfg
        .get("channel")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("numeric panel: missing string key `channel`"))?
        .to_string();
    let channel = reg.id(&channel_name);
    let (type_ok, unit) = match channel {
        Some(id) => {
            let meta = reg.meta(id);
            (ACCEPTED.contains(&meta.sample_type), meta.unit.clone())
        }
        None => (true, String::new()),
    };
    Ok(Box::new(NumericPanel { channel_name, channel, type_ok, unit }))
}

impl VizPanel for NumericPanel {
    fn title(&self) -> &str {
        &self.channel_name
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("channel: {}", self.channel_name));
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        let Some(id) = self.channel else {
            ui.colored_label(
                egui::Color32::RED,
                format!("unknown channel `{}`", self.channel_name),
            );
            return;
        };
        if !self.type_ok {
            ui.colored_label(
                egui::Color32::RED,
                format!(
                    "channel `{}` has a type not supported by the numeric panel",
                    self.channel_name
                ),
            );
            return;
        }
        let text = match store.latest(id) {
            Some((_, Sample::Float(v))) => format!("{v:.3}"),
            Some((_, Sample::Int(v))) => v.to_string(),
            Some((_, Sample::Bool(b))) => if b { "ON" } else { "OFF" }.to_string(),
            Some((_, Sample::Text(_))) | None => "—".to_string(),
        };
        ui.label(egui::RichText::new(format!("{text} {}", self.unit)).size(32.0));
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channel".to_string(),
            toml::Value::String(self.channel_name.clone()),
        );
        t
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib viz`
Expected: 4 tests PASS (headless — no display needed).

- [ ] **Step 6: Commit**

```bash
git add src/viz/ src/lib.rs
git commit -m "feat: VizPanel trait, PanelRegistry factory, numeric panel"
```

---

### Task 8: Demo source, app shell, runnable binary

**Files:**
- Create: `src/demo.rs`, `src/app.rs`, `channels.toml`, `layout.toml`
- Modify: `src/lib.rs`, `src/main.rs`
- Test: inline in `src/demo.rs` + manual `cargo run`

**Interfaces:**
- Consumes: everything above
- Produces:
  - `demo::spawn_demo(store: std::sync::Arc<LiveStore>, reg: &ChannelRegistry) -> std::thread::JoinHandle<()>` — writes ~1 kHz synthetic data (sine to float channels, counter to int, slow toggle to bool, occasional log lines to text) until process exit. Dev tool; stays in the codebase (later plans use it for panel work without a ZMQ publisher).
  - `app::DataVisApp::new(store: std::sync::Arc<dyn ChannelStore>, screen_name: String, panels: Vec<Box<dyn VizPanel>>) -> DataVisApp`, implementing `eframe::App` — toolbar (screen name + LIVE badge) and a vertical stack of panels. The ingest/layout-engine plans replace the stack with egui_tiles and make the store swappable for replay.
  - Repo-root `channels.toml` / `layout.toml` demo configs (loaded from CWD; `--demo` flag enables the generator).

- [ ] **Step 1: Write the failing demo test**

Create `src/demo.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::store::{ChannelStore, LiveStore};
    use std::sync::Arc;

    #[test]
    fn demo_feeds_all_channel_types() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
max_rate = 2000
history_s = 1.0

[channels."demo.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap();
        let store = Arc::new(LiveStore::from_registry(&reg));
        let _handle = spawn_demo(store.clone(), &reg);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let sine = reg.id("demo.sine").unwrap();
        let log = reg.id("demo.log").unwrap();
        assert!(store.latest(sine).is_some(), "no float data produced");
        assert!(store.latest(log).is_some(), "no text data produced");
    }
}
```

Add to `src/lib.rs` (final state of the file):

```rust
pub mod app;
pub mod config;
pub mod demo;
pub mod store;
pub mod types;
pub mod viz;
```

(`app` doesn't exist yet — create `src/app.rs` as an empty file for now so the crate compiles; it is filled in Step 4.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib demo`
Expected: compile FAILS (`spawn_demo` not defined).

- [ ] **Step 3: Implement the demo source**

Prepend to `src/demo.rs`:

```rust
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::ChannelRegistry;
use crate::store::{ChannelStore, LiveStore};
use crate::types::{now_ns, ChannelId, NumericVal, SampleType};

/// Dev-only synthetic data source (~1 kHz): sine → float channels,
/// wrapping counter → int, slow toggle → bool, periodic lines → text.
/// Runs until process exit.
pub fn spawn_demo(store: Arc<LiveStore>, reg: &ChannelRegistry) -> JoinHandle<()> {
    let targets: Vec<(ChannelId, SampleType)> = reg
        .iter_ids()
        .map(|id| (id, reg.meta(id).sample_type))
        .collect();
    std::thread::spawn(move || {
        let start = now_ns();
        let mut tick: u64 = 0;
        loop {
            let ts = now_ns();
            let t = (ts - start) as f64 / 1e9;
            for &(id, sample_type) in &targets {
                match sample_type {
                    SampleType::Float => {
                        let v = (2.0 * std::f64::consts::PI * t).sin() * 10.0;
                        store.write_numeric(id, ts, NumericVal::Float(v));
                    }
                    SampleType::Int => {
                        store.write_numeric(id, ts, NumericVal::Int((tick % 100) as i64));
                    }
                    SampleType::Bool => {
                        store.write_numeric(id, ts, NumericVal::Bool((tick / 500) % 2 == 0));
                    }
                    SampleType::Text => {
                        if tick % 250 == 0 {
                            store.write_text(id, ts, format!("demo log line {tick}"));
                        }
                    }
                }
            }
            tick += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
    })
}
```

- [ ] **Step 4: Implement the app shell**

Fill `src/app.rs`:

```rust
use std::sync::Arc;

use eframe::egui;

use crate::store::ChannelStore;
use crate::viz::VizPanel;

/// Top-level eframe app. Foundation version: single screen, panels stacked
/// vertically. The layout-engine plan replaces the stack with egui_tiles;
/// the replay plan makes `store` swappable (live ↔ playback).
pub struct DataVisApp {
    store: Arc<dyn ChannelStore>,
    screen_name: String,
    panels: Vec<Box<dyn VizPanel>>,
}

impl DataVisApp {
    pub fn new(
        store: Arc<dyn ChannelStore>,
        screen_name: String,
        panels: Vec<Box<dyn VizPanel>>,
    ) -> Self {
        Self { store, screen_name, panels }
    }
}

impl eframe::App for DataVisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Live data keeps coming whether or not there is input — repaint
        // continuously instead of waiting for events.
        ctx.request_repaint();

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("screen: {}", self.screen_name));
                ui.separator();
                ui.colored_label(egui::Color32::LIGHT_GREEN, "LIVE");
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for panel in &mut self.panels {
                    ui.group(|ui| {
                        ui.heading(panel.title());
                        panel.render(ui, self.store.as_ref());
                    });
                }
            });
        });
    }
}
```

- [ ] **Step 5: Wire up main and example configs**

Replace `src/main.rs`:

```rust
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context};

use datavis::app::DataVisApp;
use datavis::config::{ChannelRegistry, LayoutConfig};
use datavis::store::{ChannelStore, LiveStore};
use datavis::viz::PanelRegistry;

fn main() -> anyhow::Result<()> {
    let demo = std::env::args().any(|a| a == "--demo");

    let channels = ChannelRegistry::load(Path::new("channels.toml"))?;
    let layout = LayoutConfig::load(Path::new("layout.toml"))?;

    let store = Arc::new(LiveStore::from_registry(&channels));
    if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
    }

    let panel_registry = PanelRegistry::with_builtins();
    let (screen_name, screen) = layout
        .screens
        .iter()
        .next()
        .context("layout.toml defines no screens")?;
    let panels = screen
        .panels
        .iter()
        .map(|entry| panel_registry.build(entry, &channels))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let dyn_store: Arc<dyn ChannelStore> = store;
    let app = DataVisApp::new(dyn_store, screen_name.clone(), panels);

    eframe::run_native(
        "datavis",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}
```

(If the installed eframe version predates the `Result` app-creator closure, the closure body is `Box::new(app)` without `Ok(...)` — follow the compiler.)

Create `channels.toml` (repo root):

```toml
[channels."demo.sine"]
topic      = "demo"
proto_path = "DemoBatch.samples.sine"
ts_path    = "DemoBatch.samples.t_ns"
type       = "float"
unit       = "V"
color      = "#ff8800"
max_rate   = 2000
history_s  = 10.0

[channels."demo.counter"]
topic      = "demo"
proto_path = "DemoBatch.samples.counter"
ts_path    = "DemoBatch.samples.t_ns"
type       = "int"
max_rate   = 2000
history_s  = 10.0

[channels."demo.enabled"]
topic      = "demo"
proto_path = "DemoBatch.samples.enabled"
ts_path    = "DemoBatch.samples.t_ns"
type       = "bool"
max_rate   = 2000
history_s  = 10.0

[channels."demo.log"]
topic      = "demo"
proto_path = "DemoBatch.samples.message"
ts_path    = "DemoBatch.samples.t_ns"
type       = "text"
max_lines  = 200
```

Create `layout.toml` (repo root):

```toml
[screens.main]

[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"

[[screens.main.panels]]
type = "numeric"
channel = "demo.counter"

[[screens.main.panels]]
type = "numeric"
channel = "demo.enabled"
```

- [ ] **Step 6: Run all tests**

Run: `cargo test`
Expected: all tests from Tasks 2–8 PASS.

- [ ] **Step 7: Manual smoke test (needs a display)**

Run: `cargo run -- --demo`
Expected: window opens; toolbar shows `screen: main` + green `LIVE`; three numeric panels show a sine value oscillating ±10 V, a counter cycling 0–99, and ON/OFF toggling every ~0.5 s. Close the window; process exits cleanly.

If no display is available, run `cargo build` and note in the task report that the GUI smoke test was skipped.

- [ ] **Step 8: Commit**

```bash
git add src/ channels.toml layout.toml
git commit -m "feat: demo data source, eframe app shell, runnable binary"
```

---

## Spec Coverage Note

This foundation plan implements: architecture skeleton, data model (types, timestamps, typed SoA rings, text path, memory sizing), `channel_store` component, config file formats, `viz` trait/registry with the Numeric panel, and the `app` shell in minimal form. Remaining spec sections are explicitly deferred to follow-up plans: **ingest** (ZMQ, prost-reflect, EU scaling, backoff), **viz panels + layout engine** (Waveform/Spectrum/Gauge/XY/StateGraph/Log, egui_tiles, cursors/measurements, screens UI, layout auto-save), **recorder + replay** (lossless queue, zarrs, JSONL sidecar, gap markers, triggers, PlaybackStore, replay controls), and **integration** (100 kHz sustained-load test). Each consumes only the interfaces frozen in this plan.
