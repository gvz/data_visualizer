# Data Visualizer — Viz Panels + Layout Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the six remaining panel types (Waveform, Spectrum, Gauge, XY Scatter, State Graph, Log) with cursors/measurements, and replace the vertical panel stack with an egui_tiles dockable layout engine supporting multiple named screens, add-panel UI, and layout auto-save.

**Architecture:** Panels plug into the frozen foundation interfaces (`VizPanel`, `PanelCtor`, `PanelRegistry`, `ChannelStore`) — no changes to those signatures. A new `workspace` module owns one `egui_tiles::Tree<usize>` per screen where each pane is an index into a `Vec<PanelSlot>`; the tile arrangement persists as a JSON string embedded in `layout.toml` (`tiles_json` per screen), while the panel list + configs stay the authoritative TOML they already are. Shared pure helpers (decimation, stats, binding, config parsing) live in `viz/common.rs`, `viz/measure.rs`, `viz/decimate.rs` so each panel file stays small and every algorithm is unit-testable headless.

**Tech Stack:** Existing: Rust stable, eframe/egui 0.28, serde, toml, anyhow. New deps (this plan only): `egui_plot 0.28`, `egui_tiles 0.9` (serde feature), `rustfft 6`, `serde_json 1`.

## Global Constraints

- Frozen interfaces — do NOT change signatures of: `VizPanel`, `PanelCtor`, `PanelRegistry::{with_builtins,register,build}`, `ChannelStore`, `ChannelSnapshot`, `PanelEntry`, `ChannelRegistry`.
- New dependencies allowed in this plan and no others: `egui_plot = "0.28"`, `egui_tiles = { version = "0.9", features = ["serde"] }`, `rustfft = "6"`, `serde_json = "1"`. After adding, `cargo tree -i egui` must show exactly ONE egui version (0.28.x); if egui_tiles pulls a different egui, adjust its version until unified.
- egui only via `use eframe::egui;` (and `egui_plot`/`egui_tiles` crates directly).
- Timestamps: `i64` ns since Unix epoch. Plot x-coordinates: **seconds relative to the window start** (`(ts - t0) as f64 / 1e9`) — never raw ns as f64 (precision loss above 2^53).
- Panel ctor contract (unchanged from foundation): `Err` only for malformed config (missing/mistyped required key); unknown channel name or wrong channel type constructs fine and renders a red inline error. Never panic in render.
- `VizPanel::serialize()` returns panel-specific keys only, NO `"type"` key, and must round-trip: `build(entry) → serialize() == entry.config` when the entry carries all keys explicitly.
- All tests headless (`egui::Context::default()` + `ctx.run(RawInput::default(), …)`); `cargo test` needs no display.
- Commit messages: plain description only. No Co-Authored-By, no AI attribution, no emoji.
- No unix-only APIs.
- eframe/egui/egui_tiles API drift: if a method named in this plan doesn't exist in the installed version, follow the compiler to the equivalent (e.g. `id_source` vs `id_salt`, `from_id_salt` vs `from_id_source`) — do not change behaviour.

## Module Map (state after this plan)

```
src/
├── viz/
│   ├── mod.rs            + register 6 new panels; + PanelRegistry::type_names()
│   ├── common.rs         NEW: Binding/bind/binding_error, parse_hex_color,
│   │                     snapshot_to_f64, sample_as_f64, format_time_of_day,
│   │                     req_str/req_str_array/opt_f64/opt_i64/opt_bool
│   ├── measure.rs        NEW: Stats, stats()
│   ├── decimate.rs       NEW: decimate_minmax()
│   ├── numeric.rs        (unchanged)
│   ├── waveform.rs       NEW: scrolling time-series, cursors + min/max/mean/RMS
│   ├── spectrum.rs       NEW: rustfft, hann window, non-uniform-ts warning
│   ├── gauge.rs          NEW: bar gauge, configurable min/max
│   ├── xy_scatter.rs     NEW: two channels index-aligned
│   ├── state_graph.rs    NEW: colored bands over time
│   └── log.rs            NEW: filterable scrolling log
├── config/layout.rs      + ScreenConfig.tiles_json: Option<String>
├── workspace.rs          NEW: PanelSlot, ScreenState, Workspace, TreeBehavior,
│                         ErrorPanel (egui_tiles engine + persistence)
├── app.rs                REWRITE: menu bar, screen selector, add-panel dialog,
│                         auto-save on exit
└── main.rs               MODIFY: build Workspace, new DataVisApp::new signature
```

Panel `type` strings (layout.toml): `numeric` (exists), `waveform`, `spectrum`, `gauge`, `xy_scatter`, `state_graph`, `log`.

---

### Task 1: Dependencies + shared viz utilities

**Files:**
- Modify: `Cargo.toml` (via cargo add)
- Create: `src/viz/common.rs`, `src/viz/measure.rs`, `src/viz/decimate.rs`
- Modify: `src/viz/mod.rs` (module declarations only)

**Interfaces:**
- Consumes: `types::{ChannelId, ChannelSnapshot, Sample, SampleType}`, `config::ChannelRegistry`
- Produces (every panel task consumes these — exact signatures):
  - `common::Binding { pub name: String, pub id: Option<ChannelId>, pub type_ok: bool, pub unit: String, pub color: egui::Color32 }`
  - `common::bind(name: &str, reg: &ChannelRegistry, accepted: &[SampleType]) -> Binding`
  - `common::binding_error(ui: &mut egui::Ui, b: &Binding, panel: &str) -> bool` (renders red label + returns true if unusable)
  - `common::parse_hex_color(s: &str) -> egui::Color32`
  - `common::snapshot_to_f64(snap: &ChannelSnapshot) -> Option<(&[i64], Vec<f64>)>`
  - `common::sample_as_f64(s: &Sample) -> Option<f64>`
  - `common::format_time_of_day(ts_ns: i64) -> String` ("HH:MM:SS.mmm" UTC)
  - `common::req_str(cfg: &toml::Table, key: &str, panel: &str) -> anyhow::Result<String>`
  - `common::req_str_array(cfg: &toml::Table, key: &str, panel: &str) -> anyhow::Result<Vec<String>>`
  - `common::opt_f64(cfg: &toml::Table, key: &str, default: f64) -> f64` (accepts Integer or Float)
  - `common::opt_i64(cfg: &toml::Table, key: &str, default: i64) -> i64`
  - `common::opt_bool(cfg: &toml::Table, key: &str, default: bool) -> bool`
  - `measure::Stats { pub min: f64, pub max: f64, pub mean: f64, pub rms: f64, pub count: usize }`
  - `measure::stats(vals: &[f64]) -> Option<Stats>`
  - `decimate::decimate_minmax(ts: &[i64], vals: &[f64], t0: i64, max_buckets: usize) -> Vec<[f64; 2]>`

- [ ] **Step 1: Add dependencies**

```bash
cargo add egui_plot@0.28 rustfft@6 serde_json@1
cargo add egui_tiles@0.9 --features serde
cargo tree -i egui | head -n 3
```

Expected: build graph resolves; `cargo tree -i egui` shows exactly one `egui v0.28.x`. If two egui versions appear, adjust `egui_tiles`/`egui_plot` versions until unified.

- [ ] **Step 2: Write the failing tests**

Create `src/viz/measure.rs`:

```rust
/// Min/max/mean/RMS over a selection — the cursor-measurement numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub rms: f64,
    pub count: usize,
}

pub fn stats(vals: &[f64]) -> Option<Stats> {
    if vals.is_empty() {
        return None;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut sum = 0.0;
    let mut sq = 0.0;
    for &v in vals {
        min = min.min(v);
        max = max.max(v);
        sum += v;
        sq += v * v;
    }
    let n = vals.len() as f64;
    Some(Stats { min, max, mean: sum / n, rms: (sq / n).sqrt(), count: vals.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_of_known_values() {
        let s = stats(&[3.0, 4.0]).unwrap();
        assert_eq!(s.min, 3.0);
        assert_eq!(s.max, 4.0);
        assert_eq!(s.mean, 3.5);
        assert!((s.rms - 12.5f64.sqrt()).abs() < 1e-12);
        assert_eq!(s.count, 2);
    }

    #[test]
    fn stats_empty_is_none() {
        assert_eq!(stats(&[]), None);
    }

    #[test]
    fn stats_single_value() {
        let s = stats(&[-2.0]).unwrap();
        assert_eq!((s.min, s.max, s.mean, s.rms), (-2.0, -2.0, -2.0, 2.0));
    }
}
```

Create `src/viz/decimate.rs`:

```rust
/// Downsample for plotting: at most ~2 points per bucket (the bucket's min and
/// max, in timestamp order), so the drawn envelope matches the raw data.
/// X output is seconds relative to `t0` (raw ns as f64 loses precision).
/// Input shorter than 2×max_buckets passes through unchanged.
pub fn decimate_minmax(ts: &[i64], vals: &[f64], t0: i64, max_buckets: usize) -> Vec<[f64; 2]> {
    debug_assert_eq!(ts.len(), vals.len());
    let x = |t: i64| (t - t0) as f64 / 1e9;
    if max_buckets == 0 || ts.is_empty() {
        return Vec::new();
    }
    if ts.len() <= 2 * max_buckets {
        return ts.iter().zip(vals).map(|(&t, &v)| [x(t), v]).collect();
    }
    let bucket = ts.len().div_ceil(max_buckets);
    let mut out = Vec::with_capacity(2 * max_buckets + 2);
    let mut start = 0;
    while start < ts.len() {
        let end = (start + bucket).min(ts.len());
        let (mut imin, mut imax) = (start, start);
        for i in start..end {
            if vals[i] < vals[imin] {
                imin = i;
            }
            if vals[i] > vals[imax] {
                imax = i;
            }
        }
        let (a, b) = if imin <= imax { (imin, imax) } else { (imax, imin) };
        out.push([x(ts[a]), vals[a]]);
        if b != a {
            out.push([x(ts[b]), vals[b]]);
        }
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_input_passes_through() {
        let ts = [0i64, 1_000_000_000, 2_000_000_000];
        let vals = [1.0, 2.0, 3.0];
        let out = decimate_minmax(&ts, &vals, 0, 100);
        assert_eq!(out, vec![[0.0, 1.0], [1.0, 2.0], [2.0, 3.0]]);
    }

    #[test]
    fn envelope_preserved_on_large_input() {
        // 10k samples of a sine with a spike; decimated output must still
        // contain the global min and max.
        let n = 10_000;
        let mut ts = Vec::with_capacity(n);
        let mut vals = Vec::with_capacity(n);
        for i in 0..n {
            ts.push(i as i64 * 1_000_000);
            vals.push((i as f64 * 0.01).sin());
        }
        vals[7777] = 99.0; // spike
        vals[3333] = -99.0;
        let out = decimate_minmax(&ts, &vals, 0, 500);
        assert!(out.len() <= 2 * 500 + 2);
        let ys: Vec<f64> = out.iter().map(|p| p[1]).collect();
        assert!(ys.contains(&99.0), "max spike lost");
        assert!(ys.contains(&-99.0), "min spike lost");
        // x monotonically non-decreasing
        for w in out.windows(2) {
            assert!(w[1][0] >= w[0][0]);
        }
    }

    #[test]
    fn empty_and_zero_buckets() {
        assert!(decimate_minmax(&[], &[], 0, 100).is_empty());
        assert!(decimate_minmax(&[1], &[1.0], 0, 0).is_empty());
    }
}
```

Create `src/viz/common.rs`:

```rust
use anyhow::anyhow;
use eframe::egui::{self, Color32};

use crate::config::ChannelRegistry;
use crate::types::{ChannelId, ChannelSnapshot, Sample, SampleType};

/// A panel's link to one channel: resolved id + validity + display metadata.
pub struct Binding {
    pub name: String,
    pub id: Option<ChannelId>,
    pub type_ok: bool,
    pub unit: String,
    pub color: Color32,
}

/// Resolve a channel name. Unknown names and wrong types still produce a
/// Binding (panels render the problem inline; ctors must not fail on it).
pub fn bind(name: &str, reg: &ChannelRegistry, accepted: &[SampleType]) -> Binding {
    match reg.id(name) {
        Some(id) => {
            let m = reg.meta(id);
            Binding {
                name: name.to_string(),
                id: Some(id),
                type_ok: accepted.contains(&m.sample_type),
                unit: m.unit.clone(),
                color: parse_hex_color(&m.color),
            }
        }
        None => Binding {
            name: name.to_string(),
            id: None,
            type_ok: true,
            unit: String::new(),
            color: Color32::GRAY,
        },
    }
}

/// Render the standard inline error for a broken binding.
/// Returns true if the binding is unusable (caller should skip the channel).
pub fn binding_error(ui: &mut egui::Ui, b: &Binding, panel: &str) -> bool {
    if b.id.is_none() {
        ui.colored_label(Color32::RED, format!("unknown channel `{}`", b.name));
        return true;
    }
    if !b.type_ok {
        ui.colored_label(
            Color32::RED,
            format!("channel `{}` type not supported by {panel} panel", b.name),
        );
        return true;
    }
    false
}

/// "#rrggbb" → Color32; anything unparsable → gray.
pub fn parse_hex_color(s: &str) -> Color32 {
    let hex = s.trim_start_matches('#');
    if hex.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        ) {
            return Color32::from_rgb(r, g, b);
        }
    }
    Color32::GRAY
}

/// Numeric snapshot as (borrowed ts, owned f64 values). None for Text.
pub fn snapshot_to_f64(snap: &ChannelSnapshot) -> Option<(&[i64], Vec<f64>)> {
    match snap {
        ChannelSnapshot::Float { ts, vals } => Some((ts, vals.clone())),
        ChannelSnapshot::Int { ts, vals } => {
            Some((ts, vals.iter().map(|&v| v as f64).collect()))
        }
        ChannelSnapshot::Bool { ts, vals } => {
            Some((ts, vals.iter().map(|&v| v as f64).collect()))
        }
        ChannelSnapshot::Text { .. } => None,
    }
}

pub fn sample_as_f64(s: &Sample) -> Option<f64> {
    match s {
        Sample::Float(v) => Some(*v),
        Sample::Int(v) => Some(*v as f64),
        Sample::Bool(b) => Some(u8::from(*b) as f64),
        Sample::Text(_) => None,
    }
}

/// "HH:MM:SS.mmm" (UTC time of day) from ns since Unix epoch.
pub fn format_time_of_day(ts_ns: i64) -> String {
    let secs = ts_ns.div_euclid(1_000_000_000);
    let millis = ts_ns.rem_euclid(1_000_000_000) / 1_000_000;
    let s = secs.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600, (s % 3600) / 60, s % 60, millis)
}

// ---- panel-config accessors (ctor helpers) ----

pub fn req_str(cfg: &toml::Table, key: &str, panel: &str) -> anyhow::Result<String> {
    cfg.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{panel} panel: missing string key `{key}`"))
}

pub fn req_str_array(cfg: &toml::Table, key: &str, panel: &str) -> anyhow::Result<Vec<String>> {
    let arr = cfg
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("{panel} panel: missing array key `{key}`"))?;
    let names: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if names.is_empty() {
        return Err(anyhow!("{panel} panel: `{key}` is empty"));
    }
    Ok(names)
}

pub fn opt_f64(cfg: &toml::Table, key: &str, default: f64) -> f64 {
    match cfg.get(key) {
        Some(toml::Value::Float(f)) => *f,
        Some(toml::Value::Integer(i)) => *i as f64,
        _ => default,
    }
}

pub fn opt_i64(cfg: &toml::Table, key: &str, default: i64) -> i64 {
    match cfg.get(key) {
        Some(toml::Value::Integer(i)) => *i,
        _ => default,
    }
}

pub fn opt_bool(cfg: &toml::Table, key: &str, default: bool) -> bool {
    match cfg.get(key) {
        Some(toml::Value::Boolean(b)) => *b,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SampleType;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."a.float"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
unit = "V"
color = "#ff0000"

[channels."d.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap()
    }

    #[test]
    fn parse_hex_colors() {
        assert_eq!(parse_hex_color("#ff0000"), Color32::from_rgb(255, 0, 0));
        assert_eq!(parse_hex_color("00ff00"), Color32::from_rgb(0, 255, 0));
        assert_eq!(parse_hex_color("garbage"), Color32::GRAY);
        assert_eq!(parse_hex_color(""), Color32::GRAY);
    }

    #[test]
    fn bind_resolves_and_checks_type() {
        let reg = registry();
        let b = bind("a.float", &reg, &[SampleType::Float]);
        assert!(b.id.is_some() && b.type_ok);
        assert_eq!(b.unit, "V");
        assert_eq!(b.color, Color32::from_rgb(255, 0, 0));

        let wrong = bind("d.log", &reg, &[SampleType::Float]);
        assert!(wrong.id.is_some() && !wrong.type_ok);

        let unknown = bind("nope", &reg, &[SampleType::Float]);
        assert!(unknown.id.is_none());
    }

    #[test]
    fn snapshot_conversions() {
        let (ts, vals) = snapshot_to_f64(&ChannelSnapshot::Int {
            ts: vec![1, 2],
            vals: vec![10, 20],
        })
        .unwrap();
        assert_eq!(ts, &[1, 2]);
        assert_eq!(vals, vec![10.0, 20.0]);
        assert!(snapshot_to_f64(&ChannelSnapshot::Text { lines: vec![] }).is_none());
        assert_eq!(sample_as_f64(&Sample::Bool(true)), Some(1.0));
        assert_eq!(sample_as_f64(&Sample::Text("x".into())), None);
    }

    #[test]
    fn time_of_day_formatting() {
        // 1970-01-01 01:02:03.456 UTC
        let ns = (3_723 * 1_000_000_000i64) + 456_000_000;
        assert_eq!(format_time_of_day(ns), "01:02:03.456");
    }

    #[test]
    fn config_accessors() {
        let cfg: toml::Table =
            toml::from_str(r#"s = "x"
arr = ["a"]
f = 2
b = true"#).unwrap();
        assert_eq!(req_str(&cfg, "s", "test").unwrap(), "x");
        assert!(req_str(&cfg, "missing", "test").is_err());
        assert_eq!(req_str_array(&cfg, "arr", "test").unwrap(), vec!["a"]);
        assert!(req_str_array(&cfg, "missing", "test").is_err());
        assert_eq!(opt_f64(&cfg, "f", 0.0), 2.0); // Integer accepted as f64
        assert_eq!(opt_f64(&cfg, "missing", 7.5), 7.5);
        assert_eq!(opt_i64(&cfg, "f", 0), 2);
        assert!(opt_bool(&cfg, "b", false));
    }
}
```

Add to `src/viz/mod.rs` (next to `pub mod numeric;`):

```rust
pub mod common;
pub mod decimate;
pub mod measure;
```

- [ ] **Step 3: Run tests to verify they compile and pass**

Run: `cargo test --lib viz`
Expected: existing 4 viz tests + 11 new tests PASS (these are pure functions written with their tests; the "failing first" cycle here is the compile failure before Step 2 files existed).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/viz/
git commit -m "feat: shared viz utilities (binding, stats, min/max decimation) and plot deps"
```

---

### Task 2: Waveform panel with cursors + measurements

**Files:**
- Create: `src/viz/waveform.rs`
- Modify: `src/viz/mod.rs` (register)

**Interfaces:**
- Consumes: `common::{bind, binding_error, Binding, opt_bool, opt_f64, req_str_array, snapshot_to_f64}`, `decimate::decimate_minmax`, `measure::stats`, `egui_plot`
- Produces: `waveform::TYPE_NAME = "waveform"`, `waveform::ctor: PanelCtor`. Config keys: `channels` (required string array), `time_window_s` (default 5.0), `cursors` (default false).

- [ ] **Step 1: Write the failing tests**

Create `src/viz/waveform.rs` with the test module (implementation comes in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;
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
max_rate = 1000
history_s = 10.0

[channels."demo.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap()
    }

    fn entry(src: &str) -> PanelEntry {
        toml::from_str(src).unwrap()
    }

    #[test]
    fn builds_and_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e = entry(
            r#"type = "waveform"
channels = ["demo.sine"]
time_window_s = 5.0
cursors = true"#,
        );
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.title(), "demo.sine");
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn missing_channels_key_is_err() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e = entry(r#"type = "waveform""#);
        assert!(reg.build(&e, &channels).is_err());
    }

    #[test]
    fn defaults_applied() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e = entry(
            r#"type = "waveform"
channels = ["demo.sine"]"#,
        );
        let p = reg.build(&e, &channels).unwrap();
        let cfg = p.serialize();
        assert_eq!(cfg["time_window_s"], toml::Value::Float(5.0));
        assert_eq!(cfg["cursors"], toml::Value::Boolean(false));
    }

    #[test]
    fn selection_stats_over_range() {
        let ts = [0i64, 10, 20, 30, 40];
        let vals = [1.0, 2.0, 3.0, 4.0, 5.0];
        let s = selection_stats(&ts, &vals, 10, 30).unwrap();
        assert_eq!(s.min, 2.0);
        assert_eq!(s.max, 4.0);
        assert_eq!(s.count, 3); // ts 10, 20, 30 inclusive
        assert!(selection_stats(&ts, &vals, 100, 200).is_none());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let sine = channels.id("demo.sine").unwrap();
        for i in 0..100i64 {
            store.write_numeric(sine, i * 1_000_000, NumericVal::Float((i as f64 * 0.1).sin()));
        }
        let reg = PanelRegistry::with_builtins();
        let sources = [
            r#"type = "waveform"
channels = ["demo.sine"]
cursors = true"#,
            r#"type = "waveform"
channels = ["does.not.exist"]"#,
            r#"type = "waveform"
channels = ["demo.log"]"#,
            r#"type = "waveform"
channels = ["demo.sine", "demo.log", "does.not.exist"]"#,
        ];
        for src in sources {
            let mut p = reg.build(&entry(src), &channels).unwrap();
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

Add to `src/viz/mod.rs`:

```rust
pub mod waveform;
```

and inside `with_builtins()`:

```rust
reg.register(waveform::TYPE_NAME, waveform::ctor);
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib viz::waveform`
Expected: compile FAILS (`ctor`, `selection_stats` not defined).

- [ ] **Step 3: Write the implementation**

Prepend to `src/viz/waveform.rs`:

```rust
use eframe::egui::{self, Color32};
use egui_plot::{Legend, Line, Plot, PlotPoints, VLine};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{SampleType, TimeWindow};
use crate::viz::common::{
    bind, binding_error, opt_bool, opt_f64, req_str_array, snapshot_to_f64, Binding,
};
use crate::viz::decimate::decimate_minmax;
use crate::viz::measure::{stats, Stats};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "waveform";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int, SampleType::Bool];

/// Points fed to egui_plot per channel; ~2 per horizontal pixel is plenty.
const MAX_PLOT_BUCKETS: usize = 1000;

/// Scrolling time-series plot with optional measurement cursors.
pub struct WaveformPanel {
    title: String,
    bound: Vec<Binding>,
    time_window_s: f64,
    cursors: bool,
    /// Cursor positions in absolute ns so they stay put while the plot scrolls.
    cursor_a_ns: Option<i64>,
    cursor_b_ns: Option<i64>,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let names = req_str_array(cfg, "channels", TYPE_NAME)?;
    let bound: Vec<Binding> = names.iter().map(|n| bind(n, reg, ACCEPTED)).collect();
    Ok(Box::new(WaveformPanel {
        title: names.join(", "),
        bound,
        time_window_s: opt_f64(cfg, "time_window_s", 5.0),
        cursors: opt_bool(cfg, "cursors", false),
        cursor_a_ns: None,
        cursor_b_ns: None,
    }))
}

/// Stats over samples with lo <= ts <= hi (both cursors inclusive).
pub(crate) fn selection_stats(ts: &[i64], vals: &[f64], lo: i64, hi: i64) -> Option<Stats> {
    let sel: Vec<f64> = ts
        .iter()
        .zip(vals)
        .filter(|(&t, _)| t >= lo && t <= hi)
        .map(|(_, &v)| v)
        .collect();
    stats(&sel)
}

impl VizPanel for WaveformPanel {
    fn title(&self) -> &str {
        &self.title
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("window [s]:");
            ui.add(egui::Slider::new(&mut self.time_window_s, 0.1..=60.0).logarithmic(true));
            ui.checkbox(&mut self.cursors, "cursors");
            if ui.button("clear cursors").clicked() {
                self.cursor_a_ns = None;
                self.cursor_b_ns = None;
            }
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        for b in &self.bound {
            binding_error(ui, b, TYPE_NAME);
        }
        let end_ns = self
            .bound
            .iter()
            .filter(|b| b.type_ok)
            .filter_map(|b| b.id)
            .filter_map(|id| store.latest(id))
            .map(|(t, _)| t)
            .max();
        let Some(end_ns) = end_ns else {
            ui.label("no data");
            return;
        };
        let span_ns = (self.time_window_s * 1e9) as i64;
        let t0 = end_ns - span_ns;
        let window = TimeWindow { start_ns: t0, end_ns: end_ns + 1 };

        // Snapshots kept for the stats table below the plot.
        let mut snaps: Vec<(usize, Vec<i64>, Vec<f64>)> = Vec::new();
        for (i, b) in self.bound.iter().enumerate() {
            let (Some(id), true) = (b.id, b.type_ok) else { continue };
            let snap = store.snapshot(id, window);
            if let Some((ts, vals)) = snapshot_to_f64(&snap) {
                snaps.push((i, ts.to_vec(), vals));
            }
        }

        let plot = Plot::new(("waveform", &self.title))
            .legend(Legend::default())
            .include_x(0.0)
            .include_x(self.time_window_s);
        let inner = plot.show(ui, |plot_ui| {
            for (i, ts, vals) in &snaps {
                let points = decimate_minmax(ts, vals, t0, MAX_PLOT_BUCKETS);
                let b = &self.bound[*i];
                plot_ui.line(
                    Line::new(PlotPoints::from(points))
                        .color(b.color)
                        .name(&b.name),
                );
            }
            if self.cursors {
                if let Some(a) = self.cursor_a_ns {
                    plot_ui.vline(VLine::new((a - t0) as f64 / 1e9).color(Color32::YELLOW));
                }
                if let Some(b) = self.cursor_b_ns {
                    plot_ui.vline(VLine::new((b - t0) as f64 / 1e9).color(Color32::LIGHT_BLUE));
                }
            }
            plot_ui.pointer_coordinate()
        });
        if self.cursors && inner.response.clicked() {
            if let Some(p) = inner.inner {
                let ts = t0 + (p.x * 1e9) as i64;
                let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
                if ctrl {
                    self.cursor_b_ns = Some(ts);
                } else {
                    self.cursor_a_ns = Some(ts);
                }
            }
        }

        if self.cursors {
            ui.label("click: cursor A — ctrl+click: cursor B");
            if let (Some(a), Some(b)) = (self.cursor_a_ns, self.cursor_b_ns) {
                let (lo, hi) = (a.min(b), a.max(b));
                ui.label(format!("selection: {:.4} s", (hi - lo) as f64 / 1e9));
                egui::Grid::new(("wf-stats", &self.title))
                    .striped(true)
                    .show(ui, |ui| {
                        for h in ["channel", "min", "max", "mean", "rms", "n"] {
                            ui.strong(h);
                        }
                        ui.end_row();
                        for (i, ts, vals) in &snaps {
                            if let Some(s) = selection_stats(ts, vals, lo, hi) {
                                ui.label(&self.bound[*i].name);
                                ui.label(format!("{:.4}", s.min));
                                ui.label(format!("{:.4}", s.max));
                                ui.label(format!("{:.4}", s.mean));
                                ui.label(format!("{:.4}", s.rms));
                                ui.label(s.count.to_string());
                                ui.end_row();
                            }
                        }
                    });
            }
        }
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channels".to_string(),
            toml::Value::Array(
                self.bound
                    .iter()
                    .map(|b| toml::Value::String(b.name.clone()))
                    .collect(),
            ),
        );
        t.insert("time_window_s".to_string(), toml::Value::Float(self.time_window_s));
        t.insert("cursors".to_string(), toml::Value::Boolean(self.cursors));
        t
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib viz::waveform`
Expected: 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/viz/
git commit -m "feat: waveform panel with decimated plotting and measurement cursors"
```

---

### Task 3: Spectrum panel

**Files:**
- Create: `src/viz/spectrum.rs`
- Modify: `src/viz/mod.rs` (register)

**Interfaces:**
- Consumes: `common::{bind, binding_error, opt_i64, req_str, snapshot_to_f64}`, `rustfft`, `egui_plot`
- Produces: `spectrum::TYPE_NAME = "spectrum"`, `spectrum::ctor: PanelCtor`. Config keys: `channel` (required), `fft_size` (default 1024, clamped to power of two in 64..=65536), `window` (`"hann"` default | `"none"`).

- [ ] **Step 1: Write the failing tests**

Create `src/viz/spectrum.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;
    use eframe::egui;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
max_rate = 10000
history_s = 2.0
"#,
        )
        .unwrap()
    }

    #[test]
    fn hann_window_shape() {
        let w = hann(8);
        assert_eq!(w.len(), 8);
        assert!(w[0].abs() < 1e-12); // starts at 0
        assert!((w[4] - 1.0).abs() < 1e-12); // peak at n/2
    }

    #[test]
    fn spectrum_peak_at_sine_frequency() {
        // 1000 Hz sine sampled at 8192 Hz, n=1024 → peak at bin 125.
        let rate = 8192.0;
        let n = 1024;
        let samples: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate).sin())
            .collect();
        let bins = spectrum_db(&samples, &hann(n), rate);
        assert_eq!(bins.len(), n / 2);
        let peak = bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1[1].total_cmp(&b.1[1]))
            .unwrap()
            .0;
        assert_eq!(peak, 125);
        assert!((bins[125][0] - 1000.0).abs() < 1.0); // freq axis in Hz
    }

    #[test]
    fn rate_estimation_and_uniformity() {
        let uniform: Vec<i64> = (0..100).map(|i| i * 1_000_000).collect(); // 1 kHz
        let (rate, ok) = estimate_rate(&uniform).unwrap();
        assert!((rate - 1000.0).abs() < 1e-6);
        assert!(ok);

        let mut jittered = uniform.clone();
        jittered[50] += 500_000; // 50% off
        let (_, ok) = estimate_rate(&jittered).unwrap();
        assert!(!ok);

        assert!(estimate_rate(&[42]).is_none());
    }

    #[test]
    fn builds_serializes_and_clamps() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "spectrum"
channel = "demo.sine"
fft_size = 1000
window = "hann""#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        let cfg = p.serialize();
        assert_eq!(cfg["channel"], toml::Value::String("demo.sine".into()));
        assert_eq!(cfg["fft_size"], toml::Value::Integer(1024)); // clamped up to pow2
        assert_eq!(cfg["window"], toml::Value::String("hann".into()));
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        for i in 0..2048i64 {
            store.write_numeric(id, i * 100_000, NumericVal::Float((i as f64 * 0.3).sin()));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "spectrum"
channel = "demo.sine""#,
        )
        .unwrap();
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

Add to `src/viz/mod.rs`: `pub mod spectrum;` and in `with_builtins()`: `reg.register(spectrum::TYPE_NAME, spectrum::ctor);`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib viz::spectrum`
Expected: compile FAILS.

- [ ] **Step 3: Write the implementation**

Prepend to `src/viz/spectrum.rs`:

```rust
use eframe::egui::{self, Color32};
use egui_plot::{Line, Plot, PlotPoints};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{SampleType, TimeWindow};
use crate::viz::common::{bind, binding_error, opt_i64, req_str, snapshot_to_f64, Binding};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "spectrum";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// FFT of the newest `fft_size` samples, drawn as magnitude in dB over Hz.
pub struct SpectrumPanel {
    bound: Binding,
    fft_size: usize,
    hann_window: bool,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = req_str(cfg, "channel", TYPE_NAME)?;
    let fft_size = (opt_i64(cfg, "fft_size", 1024).max(1) as usize)
        .next_power_of_two()
        .clamp(64, 65_536);
    let hann_window = match cfg.get("window").and_then(|v| v.as_str()) {
        None | Some("hann") => true,
        Some("none") => false,
        Some(other) => anyhow::bail!("{TYPE_NAME} panel: unknown window `{other}`"),
    };
    Ok(Box::new(SpectrumPanel {
        bound: bind(&name, reg, ACCEPTED),
        fft_size,
        hann_window,
    }))
}

/// Periodic Hann window, peak 1.0 at n/2.
pub(crate) fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 * (1.0 - (std::f64::consts::TAU * i as f64 / n as f64).cos()))
        .collect()
}

/// (freq_hz, magnitude_db) for bins 0..n/2.
pub(crate) fn spectrum_db(samples: &[f64], window: &[f64], sample_rate: f64) -> Vec<[f64; 2]> {
    let n = samples.len();
    debug_assert_eq!(n, window.len());
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(n);
    let mut buf: Vec<Complex<f64>> = samples
        .iter()
        .zip(window)
        .map(|(&s, &w)| Complex::new(s * w, 0.0))
        .collect();
    fft.process(&mut buf);
    let scale = 2.0 / n as f64;
    (0..n / 2)
        .map(|i| {
            let mag = buf[i].norm() * scale;
            [
                i as f64 * sample_rate / n as f64,
                20.0 * mag.max(1e-12).log10(),
            ]
        })
        .collect()
}

/// Sample rate from the median inter-sample gap. Second value is false when
/// any gap deviates from the median by more than 10% (non-uniform sampling).
pub(crate) fn estimate_rate(ts: &[i64]) -> Option<(f64, bool)> {
    if ts.len() < 2 {
        return None;
    }
    let mut dts: Vec<i64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    dts.sort_unstable();
    let median = dts[dts.len() / 2];
    if median <= 0 {
        return None;
    }
    let tol = median / 10;
    let uniform = (median - dts[0]) <= tol && (dts[dts.len() - 1] - median) <= tol;
    Some((1e9 / median as f64, uniform))
}

impl VizPanel for SpectrumPanel {
    fn title(&self) -> &str {
        &self.bound.name
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("fft size:");
            for size in [256usize, 1024, 4096, 16_384] {
                ui.selectable_value(&mut self.fft_size, size, size.to_string());
            }
            ui.checkbox(&mut self.hann_window, "hann window");
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if binding_error(ui, &self.bound, TYPE_NAME) {
            return;
        }
        let id = self.bound.id.expect("checked by binding_error");
        let Some((end_ns, _)) = store.latest(id) else {
            ui.label("no data");
            return;
        };
        let snap = store.snapshot(id, TimeWindow { start_ns: i64::MIN, end_ns: end_ns + 1 });
        let Some((ts, vals)) = snapshot_to_f64(&snap) else {
            return;
        };
        if vals.len() < self.fft_size {
            ui.label(format!("collecting… {}/{}", vals.len(), self.fft_size));
            return;
        }
        let tail_ts = &ts[ts.len() - self.fft_size..];
        let tail = &vals[vals.len() - self.fft_size..];
        let Some((rate, uniform)) = estimate_rate(tail_ts) else {
            ui.label("cannot estimate sample rate");
            return;
        };
        if !uniform {
            ui.colored_label(
                Color32::YELLOW,
                "warning: non-uniform sample timestamps — spectrum may be distorted",
            );
        }
        let window = if self.hann_window {
            hann(self.fft_size)
        } else {
            vec![1.0; self.fft_size]
        };
        let bins = spectrum_db(tail, &window, rate);
        Plot::new(("spectrum", &self.bound.name))
            .x_axis_label("Hz")
            .y_axis_label("dB")
            .show(ui, |plot_ui| {
                plot_ui.line(Line::new(PlotPoints::from(bins)).color(self.bound.color));
            });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        t.insert("fft_size".to_string(), toml::Value::Integer(self.fft_size as i64));
        t.insert(
            "window".to_string(),
            toml::Value::String(if self.hann_window { "hann" } else { "none" }.to_string()),
        );
        t
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib viz::spectrum`
Expected: 5 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/viz/
git commit -m "feat: spectrum panel with rustfft, hann window, non-uniform warning"
```

---

### Task 4: Gauge panel

**Files:**
- Create: `src/viz/gauge.rs`
- Modify: `src/viz/mod.rs` (register)

**Interfaces:**
- Consumes: `common::{bind, binding_error, opt_f64, req_str, sample_as_f64}`
- Produces: `gauge::TYPE_NAME = "gauge"`, `gauge::ctor: PanelCtor`. Config keys: `channel` (required), `min` (default 0.0), `max` (default 100.0; must be > min or ctor errors).

- [ ] **Step 1: Write the failing tests**

Create `src/viz/gauge.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;
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
"#,
        )
        .unwrap()
    }

    #[test]
    fn fraction_clamps() {
        assert_eq!(fraction(5.0, 0.0, 10.0), 0.5);
        assert_eq!(fraction(-1.0, 0.0, 10.0), 0.0);
        assert_eq!(fraction(11.0, 0.0, 10.0), 1.0);
        assert_eq!(fraction(0.0, -10.0, 10.0), 0.5);
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "gauge"
channel = "demo.sine"
min = -10.0
max = 10.0"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn invalid_range_is_err() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "gauge"
channel = "demo.sine"
min = 5.0
max = 5.0"#,
        )
        .unwrap();
        assert!(reg.build(&e, &channels).is_err());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        store.write_numeric(id, 1, NumericVal::Float(3.0));
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "gauge"
channel = "demo.sine""#,
        )
        .unwrap();
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

Add to `src/viz/mod.rs`: `pub mod gauge;` and in `with_builtins()`: `reg.register(gauge::TYPE_NAME, gauge::ctor);`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib viz::gauge`
Expected: compile FAILS.

- [ ] **Step 3: Write the implementation**

Prepend to `src/viz/gauge.rs`:

```rust
use eframe::egui::{self, Align2, Color32, FontId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::SampleType;
use crate::viz::common::{bind, binding_error, opt_f64, req_str, sample_as_f64, Binding};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "gauge";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// Horizontal bar gauge showing the latest value within [min, max].
pub struct GaugePanel {
    bound: Binding,
    min: f64,
    max: f64,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = req_str(cfg, "channel", TYPE_NAME)?;
    let min = opt_f64(cfg, "min", 0.0);
    let max = opt_f64(cfg, "max", 100.0);
    if max <= min {
        anyhow::bail!("{TYPE_NAME} panel: max ({max}) must be greater than min ({min})");
    }
    Ok(Box::new(GaugePanel { bound: bind(&name, reg, ACCEPTED), min, max }))
}

pub(crate) fn fraction(v: f64, min: f64, max: f64) -> f32 {
    (((v - min) / (max - min)).clamp(0.0, 1.0)) as f32
}

impl VizPanel for GaugePanel {
    fn title(&self) -> &str {
        &self.bound.name
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("min:");
            ui.add(egui::DragValue::new(&mut self.min).speed(0.1));
            ui.label("max:");
            ui.add(egui::DragValue::new(&mut self.max).speed(0.1));
            if self.max <= self.min {
                self.max = self.min + 1.0;
            }
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if binding_error(ui, &self.bound, TYPE_NAME) {
            return;
        }
        let id = self.bound.id.expect("checked by binding_error");
        let value = store.latest(id).and_then(|(_, s)| sample_as_f64(&s));
        let desired = egui::vec2(ui.available_width().max(80.0), 32.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, Color32::from_gray(40));
        let text = match value {
            Some(v) => {
                let mut fill = rect;
                fill.set_width(rect.width() * fraction(v, self.min, self.max));
                painter.rect_filled(fill, 4.0, self.bound.color);
                format!("{v:.3} {}", self.bound.unit)
            }
            None => "—".to_string(),
        };
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(16.0),
            Color32::WHITE,
        );
        ui.horizontal(|ui| {
            ui.label(format!("{:.1}", self.min));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{:.1}", self.max));
            });
        });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        t.insert("min".to_string(), toml::Value::Float(self.min));
        t.insert("max".to_string(), toml::Value::Float(self.max));
        t
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib viz::gauge`
Expected: 4 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/viz/
git commit -m "feat: gauge panel with configurable range"
```

---

### Task 5: XY Scatter panel

**Files:**
- Create: `src/viz/xy_scatter.rs`
- Modify: `src/viz/mod.rs` (register)

**Interfaces:**
- Consumes: `common::{bind, binding_error, opt_f64, req_str, snapshot_to_f64}`, `egui_plot`
- Produces: `xy_scatter::TYPE_NAME = "xy_scatter"`, `xy_scatter::ctor: PanelCtor`. Config keys: `x_channel` (required), `y_channel` (required), `time_window_s` (default 1.0).

- [ ] **Step 1: Write the failing tests**

Create `src/viz/xy_scatter.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;
    use eframe::egui;

    fn registry() -> ChannelRegistry {
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

    #[test]
    fn index_align_takes_tails() {
        // Different lengths: align from the newest sample backwards.
        let x = [1.0, 2.0, 3.0, 4.0];
        let y = [10.0, 20.0];
        assert_eq!(index_align(&x, &y), vec![[3.0, 10.0], [4.0, 20.0]]);
        assert!(index_align(&[], &y).is_empty());
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "xy_scatter"
x_channel = "demo.sine"
y_channel = "demo.counter"
time_window_s = 2.0"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
        assert_eq!(p.title(), "demo.sine / demo.counter");
    }

    #[test]
    fn missing_channel_key_is_err() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "xy_scatter"
x_channel = "demo.sine""#,
        )
        .unwrap();
        assert!(reg.build(&e, &channels).is_err());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let x = channels.id("demo.sine").unwrap();
        let y = channels.id("demo.counter").unwrap();
        for i in 0..50i64 {
            store.write_numeric(x, i * 1_000_000, NumericVal::Float((i as f64).sin()));
            store.write_numeric(y, i * 1_000_000, NumericVal::Int(i));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "xy_scatter"
x_channel = "demo.sine"
y_channel = "demo.counter""#,
        )
        .unwrap();
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

Add to `src/viz/mod.rs`: `pub mod xy_scatter;` and in `with_builtins()`: `reg.register(xy_scatter::TYPE_NAME, xy_scatter::ctor);`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib viz::xy_scatter`
Expected: compile FAILS.

- [ ] **Step 3: Write the implementation**

Prepend to `src/viz/xy_scatter.rs`:

```rust
use eframe::egui;
use egui_plot::{Plot, PlotPoints, Points};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{SampleType, TimeWindow};
use crate::viz::common::{bind, binding_error, opt_f64, req_str, snapshot_to_f64, Binding};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "xy_scatter";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// Two channels plotted against each other, aligned by sample index from the
/// newest sample backwards (uniform-rate assumption; no interpolation in v1).
pub struct XyScatterPanel {
    title: String,
    x: Binding,
    y: Binding,
    time_window_s: f64,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let xn = req_str(cfg, "x_channel", TYPE_NAME)?;
    let yn = req_str(cfg, "y_channel", TYPE_NAME)?;
    Ok(Box::new(XyScatterPanel {
        title: format!("{xn} / {yn}"),
        x: bind(&xn, reg, ACCEPTED),
        y: bind(&yn, reg, ACCEPTED),
        time_window_s: opt_f64(cfg, "time_window_s", 1.0),
    }))
}

/// Pair up the newest min(len) samples of both series.
pub(crate) fn index_align(x: &[f64], y: &[f64]) -> Vec<[f64; 2]> {
    let n = x.len().min(y.len());
    x[x.len() - n..]
        .iter()
        .zip(&y[y.len() - n..])
        .map(|(&a, &b)| [a, b])
        .collect()
}

impl VizPanel for XyScatterPanel {
    fn title(&self) -> &str {
        &self.title
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("window [s]:");
            ui.add(egui::Slider::new(&mut self.time_window_s, 0.05..=10.0).logarithmic(true));
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        let xe = binding_error(ui, &self.x, TYPE_NAME);
        let ye = binding_error(ui, &self.y, TYPE_NAME);
        if xe || ye {
            return;
        }
        let (xid, yid) = (self.x.id.unwrap(), self.y.id.unwrap());
        let end_ns = match (store.latest(xid), store.latest(yid)) {
            (Some((a, _)), Some((b, _))) => a.min(b),
            _ => {
                ui.label("no data");
                return;
            }
        };
        let span = (self.time_window_s * 1e9) as i64;
        let window = TimeWindow { start_ns: end_ns - span, end_ns: end_ns + 1 };
        let xs = store.snapshot(xid, window);
        let ys = store.snapshot(yid, window);
        let (Some((_, xv)), Some((_, yv))) = (snapshot_to_f64(&xs), snapshot_to_f64(&ys)) else {
            return;
        };
        let pts = index_align(&xv, &yv);
        Plot::new(("xy", &self.title))
            .data_aspect(1.0)
            .show(ui, |plot_ui| {
                plot_ui.points(
                    Points::new(PlotPoints::from(pts))
                        .radius(1.5)
                        .color(self.y.color),
                );
            });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("x_channel".to_string(), toml::Value::String(self.x.name.clone()));
        t.insert("y_channel".to_string(), toml::Value::String(self.y.name.clone()));
        t.insert("time_window_s".to_string(), toml::Value::Float(self.time_window_s));
        t
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib viz::xy_scatter`
Expected: 4 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/viz/
git commit -m "feat: xy scatter panel with tail index alignment"
```

---

### Task 6: State Graph panel

**Files:**
- Create: `src/viz/state_graph.rs`
- Modify: `src/viz/mod.rs` (register)

**Interfaces:**
- Consumes: `common::{bind, binding_error, opt_f64, req_str}`
- Produces: `state_graph::TYPE_NAME = "state_graph"`, `state_graph::ctor: PanelCtor`. Config keys: `channel` (required), `states` (optional table, keys are integer-valued strings like `"0"`, values are labels; non-integer key → ctor Err), `time_window_s` (default 30.0).

- [ ] **Step 1: Write the failing tests**

Create `src/viz/state_graph.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
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

[channels."demo.enabled"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "bool"
"#,
        )
        .unwrap()
    }

    #[test]
    fn segments_merge_consecutive_values() {
        let ts = [0i64, 1, 2, 3, 4];
        let vals = [0i64, 0, 1, 1, 0];
        assert_eq!(segments(&ts, &vals), vec![(0, 2, 0), (2, 4, 1), (4, 4, 0)]);
        assert!(segments(&[], &[]).is_empty());
        assert_eq!(segments(&[7], &[3]), vec![(7, 7, 3)]);
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "state_graph"
channel = "motor.state"
states = { 0 = "IDLE", 1 = "RUN", 2 = "FAULT" }
time_window_s = 30.0"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn bad_state_key_is_err() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "state_graph"
channel = "motor.state"
states = { abc = "IDLE" }"#,
        )
        .unwrap();
        assert!(reg.build(&e, &channels).is_err());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let motor = channels.id("motor.state").unwrap();
        let enabled = channels.id("demo.enabled").unwrap();
        for i in 0..50i64 {
            store.write_numeric(motor, i * 1_000_000, NumericVal::Int(i / 20));
            store.write_numeric(enabled, i * 1_000_000, NumericVal::Bool(i % 10 < 5));
        }
        let reg = PanelRegistry::with_builtins();
        for src in [
            r#"type = "state_graph"
channel = "motor.state"
states = { 0 = "IDLE", 1 = "RUN" }"#,
            r#"type = "state_graph"
channel = "demo.enabled""#,
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
}
```

Add to `src/viz/mod.rs`: `pub mod state_graph;` and in `with_builtins()`: `reg.register(state_graph::TYPE_NAME, state_graph::ctor);`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib viz::state_graph`
Expected: compile FAILS.

- [ ] **Step 3: Write the implementation**

Prepend to `src/viz/state_graph.rs`:

```rust
use std::collections::BTreeMap;

use eframe::egui::{self, Align2, Color32, FontId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::common::{bind, binding_error, opt_f64, req_str, Binding};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "state_graph";

const ACCEPTED: &[SampleType] = &[SampleType::Bool, SampleType::Int];

const PALETTE: &[Color32] = &[
    Color32::from_rgb(0x4c, 0xaf, 0x50), // green
    Color32::from_rgb(0x21, 0x96, 0xf3), // blue
    Color32::from_rgb(0xff, 0x98, 0x00), // orange
    Color32::from_rgb(0xf4, 0x43, 0x36), // red
    Color32::from_rgb(0x9c, 0x27, 0xb0), // purple
    Color32::from_rgb(0x00, 0xbc, 0xd4), // cyan
];

fn color_for(value: i64) -> Color32 {
    PALETTE[(value.rem_euclid(PALETTE.len() as i64)) as usize]
}

/// Grafana-style colored bands: one horizontal strip, one colored segment per
/// contiguous run of equal values.
pub struct StateGraphPanel {
    bound: Binding,
    states: BTreeMap<i64, String>,
    time_window_s: f64,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = req_str(cfg, "channel", TYPE_NAME)?;
    let mut states = BTreeMap::new();
    if let Some(tbl) = cfg.get("states").and_then(|v| v.as_table()) {
        for (k, v) in tbl {
            let key: i64 = k
                .parse()
                .map_err(|_| anyhow::anyhow!("{TYPE_NAME} panel: state key `{k}` is not an integer"))?;
            states.insert(key, v.as_str().unwrap_or_default().to_string());
        }
    }
    Ok(Box::new(StateGraphPanel {
        bound: bind(&name, reg, ACCEPTED),
        states,
        time_window_s: opt_f64(cfg, "time_window_s", 30.0),
    }))
}

/// Contiguous runs of equal values: (start_ts, end_ts, value). The final
/// segment ends at the last timestamp.
pub(crate) fn segments(ts: &[i64], vals: &[i64]) -> Vec<(i64, i64, i64)> {
    if ts.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let (mut start, mut cur) = (ts[0], vals[0]);
    for i in 1..ts.len() {
        if vals[i] != cur {
            out.push((start, ts[i], cur));
            start = ts[i];
            cur = vals[i];
        }
    }
    out.push((start, *ts.last().unwrap(), cur));
    out
}

impl StateGraphPanel {
    fn label_for(&self, value: i64) -> String {
        self.states
            .get(&value)
            .cloned()
            .unwrap_or_else(|| value.to_string())
    }
}

impl VizPanel for StateGraphPanel {
    fn title(&self) -> &str {
        &self.bound.name
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("window [s]:");
            ui.add(egui::Slider::new(&mut self.time_window_s, 1.0..=600.0).logarithmic(true));
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if binding_error(ui, &self.bound, TYPE_NAME) {
            return;
        }
        let id = self.bound.id.expect("checked by binding_error");
        let Some((end_ns, _)) = store.latest(id) else {
            ui.label("no data");
            return;
        };
        let span = (self.time_window_s * 1e9) as i64;
        let t0 = end_ns - span;
        let snap = store.snapshot(id, TimeWindow { start_ns: t0, end_ns: end_ns + 1 });
        let (ts, vals): (Vec<i64>, Vec<i64>) = match &snap {
            ChannelSnapshot::Int { ts, vals } => (ts.clone(), vals.clone()),
            ChannelSnapshot::Bool { ts, vals } => {
                (ts.clone(), vals.iter().map(|&v| v as i64).collect())
            }
            _ => return,
        };
        let desired = egui::vec2(ui.available_width().max(80.0), 40.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, Color32::from_gray(30));
        let x_of = |t: i64| {
            rect.left() + rect.width() * ((t - t0) as f32 / span.max(1) as f32)
        };
        for (s, e, v) in segments(&ts, &vals) {
            // Extend the last segment to "now" (the right edge).
            let e = if e == *ts.last().unwrap() { end_ns } else { e };
            let seg = egui::Rect::from_min_max(
                egui::pos2(x_of(s), rect.top()),
                egui::pos2(x_of(e), rect.bottom()),
            );
            painter.rect_filled(seg, 0.0, color_for(v));
            if seg.width() > 40.0 {
                painter.text(
                    seg.center(),
                    Align2::CENTER_CENTER,
                    self.label_for(v),
                    FontId::proportional(12.0),
                    Color32::BLACK,
                );
            }
        }
        // Legend of known states.
        ui.horizontal_wrapped(|ui| {
            for (v, label) in &self.states {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().rect_filled(dot, 2.0, color_for(*v));
                ui.label(label);
            }
        });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        if !self.states.is_empty() {
            let mut tbl = toml::Table::new();
            for (k, v) in &self.states {
                tbl.insert(k.to_string(), toml::Value::String(v.clone()));
            }
            t.insert("states".to_string(), toml::Value::Table(tbl));
        }
        t.insert("time_window_s".to_string(), toml::Value::Float(self.time_window_s));
        t
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib viz::state_graph`
Expected: 4 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/viz/
git commit -m "feat: state graph panel with colored bands and state legend"
```

---

### Task 7: Log panel

**Files:**
- Create: `src/viz/log.rs`
- Modify: `src/viz/mod.rs` (register)

**Interfaces:**
- Consumes: `common::{bind, binding_error, format_time_of_day, opt_i64, req_str_array}`
- Produces: `log::TYPE_NAME = "log"`, `log::ctor: PanelCtor`. Config keys: `channels` (required string array), `max_lines` (default 500). The filter text is runtime-only view state (not serialized).

- [ ] **Step 1: Write the failing tests**

Create `src/viz/log.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::viz::PanelRegistry;
    use eframe::egui;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."system.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"

[channels."app.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap()
    }

    #[test]
    fn merge_sorts_filters_and_caps() {
        let sets = vec![
            vec![(3i64, "warn: c".to_string()), (1, "info: a".to_string())],
            vec![(2i64, "WARN: b".to_string())],
        ];
        // case-insensitive filter, merged and sorted by ts
        let out = merge_filter(sets.clone(), "warn", 10);
        assert_eq!(out, vec![(2, "WARN: b".to_string()), (3, "warn: c".to_string())]);
        // cap keeps the newest
        let out = merge_filter(sets, "", 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 2);
        assert_eq!(out[1].0, 3);
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "log"
channels = ["system.log"]
max_lines = 200"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let a = channels.id("system.log").unwrap();
        let b = channels.id("app.log").unwrap();
        store.write_text(a, 1, "boot ok".into());
        store.write_text(b, 2, "app started".into());
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "log"
channels = ["system.log", "app.log", "does.not.exist"]"#,
        )
        .unwrap();
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

Add to `src/viz/mod.rs`: `pub mod log;` and in `with_builtins()`: `reg.register(log::TYPE_NAME, log::ctor);`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib viz::log`
Expected: compile FAILS.

- [ ] **Step 3: Write the implementation**

Prepend to `src/viz/log.rs`:

```rust
use eframe::egui;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::common::{bind, binding_error, format_time_of_day, opt_i64, req_str_array, Binding};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "log";

const ACCEPTED: &[SampleType] = &[SampleType::Text];

/// Scrolling, filterable, timestamped log merged from one or more text channels.
pub struct LogPanel {
    title: String,
    bound: Vec<Binding>,
    max_lines: usize,
    filter: String,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let names = req_str_array(cfg, "channels", TYPE_NAME)?;
    let bound = names.iter().map(|n| bind(n, reg, ACCEPTED)).collect();
    Ok(Box::new(LogPanel {
        title: names.join(", "),
        bound,
        max_lines: opt_i64(cfg, "max_lines", 500).max(1) as usize,
        filter: String::new(),
    }))
}

/// Merge line sets, sort by timestamp, apply case-insensitive substring
/// filter, keep only the newest `max` lines.
pub(crate) fn merge_filter(
    mut sets: Vec<Vec<(i64, String)>>,
    filter: &str,
    max: usize,
) -> Vec<(i64, String)> {
    let mut all: Vec<(i64, String)> = sets.drain(..).flatten().collect();
    all.sort_by_key(|(t, _)| *t);
    let f = filter.to_lowercase();
    let mut out: Vec<(i64, String)> = all
        .into_iter()
        .filter(|(_, l)| f.is_empty() || l.to_lowercase().contains(&f))
        .collect();
    if out.len() > max {
        out.drain(..out.len() - max);
    }
    out
}

impl VizPanel for LogPanel {
    fn title(&self) -> &str {
        &self.title
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("filter:");
            ui.text_edit_singleline(&mut self.filter);
            let mut max = self.max_lines as i64;
            ui.label("max lines:");
            ui.add(egui::DragValue::new(&mut max).clamp_range(1..=100_000));
            self.max_lines = max.max(1) as usize;
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        let mut sets = Vec::new();
        for b in &self.bound {
            if binding_error(ui, b, TYPE_NAME) {
                continue;
            }
            let id = b.id.expect("checked by binding_error");
            let snap = store.snapshot(id, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
            if let ChannelSnapshot::Text { lines } = snap {
                sets.push(lines);
            }
        }
        let lines = merge_filter(sets, &self.filter, self.max_lines);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (ts, line) in &lines {
                    ui.monospace(format!("{}  {}", format_time_of_day(*ts), line));
                }
            });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channels".to_string(),
            toml::Value::Array(
                self.bound
                    .iter()
                    .map(|b| toml::Value::String(b.name.clone()))
                    .collect(),
            ),
        );
        t.insert("max_lines".to_string(), toml::Value::Integer(self.max_lines as i64));
        t
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib viz::log`
Expected: 3 tests PASS. Also run `cargo test --lib` — full suite green.

- [ ] **Step 5: Commit**

```bash
git add src/viz/
git commit -m "feat: filterable log panel merging text channels"
```

---

### Task 8: layout.toml tiles field

**Files:**
- Modify: `src/config/layout.rs`

**Interfaces:**
- Consumes: existing `ScreenConfig`
- Produces: `ScreenConfig.tiles_json: Option<String>` — serde default `None`, omitted from TOML when `None`. Task 9 stores a `serde_json`-encoded `egui_tiles::Tree<usize>` here; this task only adds the transport field.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/config/layout.rs`:

```rust
    #[test]
    fn tiles_json_round_trips() {
        let src = r#"
[screens.main]
tiles_json = '{"fake":"tree"}'

[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
"#;
        let l = LayoutConfig::from_toml_str(src).unwrap();
        assert_eq!(
            l.screens["main"].tiles_json.as_deref(),
            Some(r#"{"fake":"tree"}"#)
        );
        let l2 = LayoutConfig::from_toml_str(&l.to_toml_string().unwrap()).unwrap();
        assert_eq!(l, l2);
    }

    #[test]
    fn tiles_json_absent_is_none_and_not_serialized() {
        let l = LayoutConfig::from_toml_str("[screens.empty]\n").unwrap();
        assert_eq!(l.screens["empty"].tiles_json, None);
        let out = l.to_toml_string().unwrap();
        assert!(!out.contains("tiles_json"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::layout`
Expected: compile FAILS (`tiles_json` field missing).

- [ ] **Step 3: Add the field**

In `src/config/layout.rs`, change `ScreenConfig` to:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScreenConfig {
    /// egui_tiles tree layout (JSON-encoded), written by the workspace module.
    /// Absent on hand-written configs — a default grid is built instead.
    /// MUST be declared before `panels`: TOML requires scalar values to
    /// serialize before arrays-of-tables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiles_json: Option<String>,
    #[serde(default)]
    pub panels: Vec<PanelEntry>,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config::layout`
Expected: 6 tests PASS (4 existing + 2 new).

- [ ] **Step 5: Commit**

```bash
git add src/config/layout.rs
git commit -m "feat: per-screen tiles_json field for tile-tree persistence"
```

---

### Task 9: Workspace — egui_tiles layout engine

**Files:**
- Create: `src/workspace.rs`
- Modify: `src/lib.rs` (add `pub mod workspace;`), `src/viz/mod.rs` (add `type_names`)

**Interfaces:**
- Consumes: `LayoutConfig`/`ScreenConfig`/`PanelEntry` (incl. `tiles_json`), `PanelRegistry`, `ChannelRegistry`, `ChannelStore`, `VizPanel`, `egui_tiles`, `serde_json`
- Produces (Task 10 depends on these exactly):
  - `viz::PanelRegistry::type_names(&self) -> Vec<&'static str>` (sorted)
  - `workspace::PanelSlot { pub type_name: String, pub panel: Box<dyn VizPanel> }`
  - `workspace::ScreenState { pub tree: egui_tiles::Tree<usize>, pub panels: Vec<PanelSlot> }`
  - `workspace::Workspace { pub screens: std::collections::BTreeMap<String, ScreenState>, pub active: String }` with:
    - `fn from_config(cfg: &LayoutConfig, reg: &PanelRegistry, channels: &ChannelRegistry) -> Workspace` (never fails; broken entries become error panels)
    - `fn to_config(&self) -> LayoutConfig`
    - `fn ui(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore)`
    - `fn add_screen(&mut self, name: &str)` (creates if missing, switches active)
    - `fn add_panel(&mut self, entry: &PanelEntry, reg: &PanelRegistry, channels: &ChannelRegistry) -> anyhow::Result<()>` (adds to active screen)

- [ ] **Step 1: Add `type_names` to PanelRegistry**

In `src/viz/mod.rs`, add to `impl PanelRegistry`:

```rust
    /// Registered panel type strings, sorted for stable UI listings.
    pub fn type_names(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self.ctors.keys().copied().collect();
        v.sort_unstable();
        v
    }
```

- [ ] **Step 2: Write the failing tests**

Create `src/workspace.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;
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
channel = "demo.sine"

[[screens.main.panels]]
type = "numeric"
channel = "demo.counter"

[screens.aux]
[[screens.aux.panels]]
type = "numeric"
channel = "demo.sine"
"#;

    fn build() -> (ChannelRegistry, PanelRegistry, Workspace) {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let cfg = LayoutConfig::from_toml_str(LAYOUT).unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        (ch, reg, ws)
    }

    fn pane_count(st: &ScreenState) -> usize {
        st.tree
            .tiles
            .iter()
            .filter(|(_, t)| matches!(t, egui_tiles::Tile::Pane(_)))
            .count()
    }

    #[test]
    fn from_config_builds_default_grid() {
        let (_, _, ws) = build();
        assert_eq!(ws.screens.len(), 2);
        assert_eq!(ws.active, "aux"); // BTreeMap order: first key
        assert_eq!(pane_count(&ws.screens["main"]), 2);
        assert_eq!(ws.screens["main"].panels.len(), 2);
    }

    #[test]
    fn round_trip_preserves_panels_and_restores_tree() {
        let (ch, reg, ws) = build();
        let cfg = ws.to_config();
        assert_eq!(cfg.screens.len(), 2);
        let main = &cfg.screens["main"];
        assert_eq!(main.panels.len(), 2);
        assert_eq!(main.panels[0].panel_type, "numeric");
        assert!(main.tiles_json.is_some());
        // Reload: tree restored from tiles_json, still consistent.
        let ws2 = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(pane_count(&ws2.screens["main"]), 2);
        let cfg2 = ws2.to_config();
        assert_eq!(cfg.screens["main"].panels, cfg2.screens["main"].panels);
    }

    #[test]
    fn invalid_tiles_json_falls_back_to_grid() {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let mut cfg = LayoutConfig::from_toml_str(LAYOUT).unwrap();
        cfg.screens.get_mut("main").unwrap().tiles_json = Some("not json".into());
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(pane_count(&ws.screens["main"]), 2);
    }

    #[test]
    fn unknown_panel_type_becomes_error_panel_and_keeps_config_on_save() {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let cfg = LayoutConfig::from_toml_str(
            r#"
[screens.main]
[[screens.main.panels]]
type = "hologram"
channel = "demo.sine"
setting = 42
"#,
        )
        .unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(ws.screens["main"].panels.len(), 1);
        let saved = ws.to_config();
        let entry = &saved.screens["main"].panels[0];
        assert_eq!(entry.panel_type, "hologram"); // original type preserved
        assert_eq!(entry.config["setting"], toml::Value::Integer(42));
    }

    #[test]
    fn add_panel_and_add_screen() {
        let (ch, reg, mut ws) = build();
        ws.add_screen("fresh");
        assert_eq!(ws.active, "fresh");
        assert_eq!(pane_count(&ws.screens["fresh"]), 0);
        let entry = PanelEntry {
            panel_type: "numeric".into(),
            config: toml::from_str(r#"channel = "demo.sine""#).unwrap(),
        };
        ws.add_panel(&entry, &reg, &ch).unwrap();
        ws.add_panel(&entry, &reg, &ch).unwrap();
        assert_eq!(pane_count(&ws.screens["fresh"]), 2);
        assert_eq!(ws.screens["fresh"].panels.len(), 2);
        // Unknown type propagates Err (interactive path — user sees it).
        let bad = PanelEntry { panel_type: "hologram".into(), config: toml::Table::new() };
        assert!(ws.add_panel(&bad, &reg, &ch).is_err());
    }

    #[test]
    fn ui_renders_headless_without_panic() {
        let (ch, _, mut ws) = build();
        let store = LiveStore::from_registry(&ch);
        store.write_numeric(ch.id("demo.sine").unwrap(), 1, NumericVal::Float(1.0));
        for screen in ["aux", "main"] {
            ws.active = screen.to_string();
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ws.ui(ui, &store);
                });
            });
        }
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod workspace;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib workspace`
Expected: compile FAILS (`Workspace` not defined).

- [ ] **Step 4: Write the implementation**

Prepend to `src/workspace.rs`:

```rust
use std::collections::BTreeMap;

use anyhow::anyhow;
use eframe::egui;
use egui_tiles::{Tile, TileId, Tree};

use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry, ScreenConfig};
use crate::store::ChannelStore;
use crate::types::SampleType;
use crate::viz::{PanelRegistry, VizPanel};

/// A panel plus the layout.toml type string needed to re-serialize it.
pub struct PanelSlot {
    pub type_name: String,
    pub panel: Box<dyn VizPanel>,
}

/// One screen: a tile tree whose panes are indices into `panels`.
pub struct ScreenState {
    pub tree: Tree<usize>,
    pub panels: Vec<PanelSlot>,
}

/// All screens + which one is showing. The dockable-layout engine.
pub struct Workspace {
    pub screens: BTreeMap<String, ScreenState>,
    pub active: String,
}

/// Stand-in for a panel whose constructor failed (e.g. unknown type when the
/// layout file came from a newer build). Renders the error, and re-serializes
/// the ORIGINAL config so saving the layout never destroys user data.
struct ErrorPanel {
    title: String,
    msg: String,
    orig: toml::Table,
}

impl VizPanel for ErrorPanel {
    fn title(&self) -> &str {
        &self.title
    }
    fn accepted_types(&self) -> &[SampleType] {
        &[]
    }
    fn config_ui(&mut self, _ui: &mut egui::Ui) {}
    fn render(&mut self, ui: &mut egui::Ui, _store: &dyn ChannelStore) {
        ui.colored_label(egui::Color32::RED, &self.msg);
    }
    fn serialize(&self) -> toml::Table {
        self.orig.clone()
    }
}

fn default_tree(name: &str, n: usize) -> Tree<usize> {
    Tree::new_grid(egui::Id::new(("screen", name)), (0..n).collect())
}

/// A persisted tree is usable only if its panes are exactly {0..n} once each.
fn tree_panes_valid(t: &Tree<usize>, n: usize) -> bool {
    let mut seen = vec![false; n];
    let mut count = 0;
    for (_, tile) in t.tiles.iter() {
        if let Tile::Pane(i) = tile {
            if *i >= n || seen[*i] {
                return false;
            }
            seen[*i] = true;
            count += 1;
        }
    }
    count == n
}

impl ScreenState {
    fn empty(name: &str) -> Self {
        Self { tree: Tree::empty(egui::Id::new(("screen", name))), panels: Vec::new() }
    }

    fn from_screen_config(
        name: &str,
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
        let tree = sc
            .tiles_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<Tree<usize>>(j).ok())
            .filter(|t| tree_panes_valid(t, panels.len()))
            .unwrap_or_else(|| default_tree(name, panels.len()));
        Self { tree, panels }
    }
}

impl Workspace {
    pub fn from_config(
        cfg: &LayoutConfig,
        reg: &PanelRegistry,
        channels: &ChannelRegistry,
    ) -> Self {
        let mut screens = BTreeMap::new();
        for (name, sc) in &cfg.screens {
            screens.insert(
                name.clone(),
                ScreenState::from_screen_config(name, sc, reg, channels),
            );
        }
        if screens.is_empty() {
            screens.insert("main".to_string(), ScreenState::empty("main"));
        }
        let active = screens.keys().next().unwrap().clone();
        Self { screens, active }
    }

    pub fn to_config(&self) -> LayoutConfig {
        let mut cfg = LayoutConfig::default();
        for (name, st) in &self.screens {
            // Panes in TileId order → deterministic panel order in the file.
            let mut pane_tiles: Vec<(TileId, usize)> = st
                .tree
                .tiles
                .iter()
                .filter_map(|(id, tile)| match tile {
                    Tile::Pane(i) => Some((*id, *i)),
                    _ => None,
                })
                .collect();
            pane_tiles.sort_by_key(|(id, _)| *id);

            let mut remap = vec![usize::MAX; st.panels.len()];
            let mut entries = Vec::new();
            for (new_idx, (_, old_idx)) in pane_tiles.iter().enumerate() {
                remap[*old_idx] = new_idx;
                let slot = &st.panels[*old_idx];
                entries.push(PanelEntry {
                    panel_type: slot.type_name.clone(),
                    config: slot.panel.serialize(),
                });
            }
            // Clone the tree with panes renumbered to the new order.
            let mut tree = st.tree.clone();
            let ids: Vec<TileId> = tree.tiles.iter().map(|(id, _)| *id).collect();
            for id in ids {
                if let Some(Tile::Pane(i)) = tree.tiles.get_mut(id) {
                    *i = remap[*i];
                }
            }
            cfg.screens.insert(
                name.clone(),
                ScreenConfig {
                    panels: entries,
                    tiles_json: serde_json::to_string(&tree).ok(),
                },
            );
        }
        cfg
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        let Some(st) = self.screens.get_mut(&self.active) else {
            return;
        };
        let mut behavior = TreeBehavior { store, panels: &mut st.panels };
        st.tree.ui(&mut behavior, ui);
    }

    pub fn add_screen(&mut self, name: &str) {
        if !self.screens.contains_key(name) {
            self.screens.insert(name.to_string(), ScreenState::empty(name));
        }
        self.active = name.to_string();
    }

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
        let panel = reg.build(entry, channels)?;
        let idx = st.panels.len();
        st.panels.push(PanelSlot { type_name: entry.panel_type.clone(), panel });
        let pane = st.tree.tiles.insert_pane(idx);
        match st.tree.root() {
            None => st.tree.root = Some(pane),
            Some(root) => match st.tree.tiles.get_mut(root) {
                Some(Tile::Container(c)) => c.add_child(pane),
                _ => {
                    let new_root = st.tree.tiles.insert_tab_tile(vec![root, pane]);
                    st.tree.root = Some(new_root);
                }
            },
        }
        Ok(())
    }
}

/// egui_tiles glue: renders a pane by delegating to its panel; tab drag &
/// drop and splitting come free from egui_tiles.
struct TreeBehavior<'a> {
    store: &'a dyn ChannelStore,
    panels: &'a mut Vec<PanelSlot>,
}

impl egui_tiles::Behavior<usize> for TreeBehavior<'_> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        _tile_id: TileId,
        pane: &mut usize,
    ) -> egui_tiles::UiResponse {
        if let Some(slot) = self.panels.get_mut(*pane) {
            egui::CollapsingHeader::new("settings")
                .id_source((*pane, "panel-settings"))
                .show(ui, |ui| slot.panel.config_ui(ui));
            slot.panel.render(ui, self.store);
        }
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &usize) -> egui::WidgetText {
        self.panels
            .get(*pane)
            .map(|s| s.panel.title().to_string())
            .unwrap_or_default()
            .into()
    }
}
```

API drift notes for the implementer (follow the compiler, keep behaviour):
- `Tree::new_grid(id, panes)` / `Tree::empty(id)` — if the id parameter type differs, wrap with `egui::Id::new(...)` as shown.
- `tree.tiles.iter()` yields `(&TileId, &Tile<Pane>)`; `tiles.get_mut(TileId)`, `tiles.insert_pane(pane)`, `tiles.insert_tab_tile(children)` exist in egui_tiles 0.9. `tree.root()` gets, `tree.root = …` sets (field is public).
- If `TileId` lacks `Ord`, sort with `sort_by_key(|(id, _)| format!("{id:?}"))` instead.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib workspace`
Expected: 6 tests PASS. Also run `cargo test --lib` — full suite green.

- [ ] **Step 6: Commit**

```bash
git add src/workspace.rs src/lib.rs src/viz/mod.rs
git commit -m "feat: egui_tiles workspace with per-screen trees and layout persistence"
```

---

### Task 10: App shell v2 — menus, screens, add-panel dialog, auto-save

**Files:**
- Rewrite: `src/app.rs`
- Modify: `src/main.rs`, `layout.toml` (demo layout)

**Interfaces:**
- Consumes: `Workspace` (Task 9), `PanelRegistry::type_names`, `LayoutConfig::{load,save}`, `build_panel_entry` (defined here)
- Produces:
  - `app::DataVisApp::new(store: Arc<dyn ChannelStore>, channels: ChannelRegistry, registry: PanelRegistry, workspace: Workspace, layout_path: PathBuf) -> DataVisApp` — NOTE: signature replaces the foundation one; `main.rs` is updated in this task.
  - `app::build_panel_entry(panel_type: &str, selected: &[String]) -> Option<PanelEntry>` (pure; channel-count rules per type)
  - Auto-save: `eframe::App::on_exit` writes `workspace.to_config()` to `layout_path`.

- [ ] **Step 1: Write the failing tests**

Rewrite `src/app.rs` starting with the test module (implementation in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_entry_single_channel_types() {
        for t in ["numeric", "gauge", "spectrum", "state_graph"] {
            let e = build_panel_entry(t, &["a".into()]).unwrap();
            assert_eq!(e.panel_type, t);
            assert_eq!(e.config["channel"], toml::Value::String("a".into()));
            assert!(build_panel_entry(t, &[]).is_none());
            assert!(build_panel_entry(t, &["a".into(), "b".into()]).is_none());
        }
    }

    #[test]
    fn build_entry_multi_channel_types() {
        for t in ["waveform", "log"] {
            let e = build_panel_entry(t, &["a".into(), "b".into()]).unwrap();
            assert_eq!(
                e.config["channels"],
                toml::Value::Array(vec![
                    toml::Value::String("a".into()),
                    toml::Value::String("b".into())
                ])
            );
            assert!(build_panel_entry(t, &[]).is_none());
        }
    }

    #[test]
    fn build_entry_xy_needs_exactly_two() {
        let e = build_panel_entry("xy_scatter", &["x".into(), "y".into()]).unwrap();
        assert_eq!(e.config["x_channel"], toml::Value::String("x".into()));
        assert_eq!(e.config["y_channel"], toml::Value::String("y".into()));
        assert!(build_panel_entry("xy_scatter", &["x".into()]).is_none());
        assert!(build_panel_entry("xy_scatter", &["a".into(), "b".into(), "c".into()]).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib app`
Expected: compile FAILS (`build_panel_entry` not defined).

- [ ] **Step 3: Write the implementation**

Prepend to `src/app.rs`:

```rust
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
use crate::store::ChannelStore;
use crate::viz::PanelRegistry;
use crate::workspace::Workspace;

/// State for the "Add panel" modal.
#[derive(Default)]
struct AddPanelDialog {
    open: bool,
    panel_type: String,
    /// Channel names in click order (order matters for xy_scatter: x then y).
    selected: Vec<String>,
}

/// Channel-count rules per panel type. None = invalid selection for that type.
pub fn build_panel_entry(panel_type: &str, selected: &[String]) -> Option<PanelEntry> {
    let mut cfg = toml::Table::new();
    match panel_type {
        "waveform" | "log" => {
            if selected.is_empty() {
                return None;
            }
            cfg.insert(
                "channels".to_string(),
                toml::Value::Array(
                    selected.iter().map(|s| toml::Value::String(s.clone())).collect(),
                ),
            );
        }
        "xy_scatter" => {
            if selected.len() != 2 {
                return None;
            }
            cfg.insert("x_channel".to_string(), toml::Value::String(selected[0].clone()));
            cfg.insert("y_channel".to_string(), toml::Value::String(selected[1].clone()));
        }
        _ => {
            if selected.len() != 1 {
                return None;
            }
            cfg.insert("channel".to_string(), toml::Value::String(selected[0].clone()));
        }
    }
    Some(PanelEntry { panel_type: panel_type.to_string(), config: cfg })
}

/// Top-level eframe app: menu bar, screen tabs, tiled workspace, dialogs.
pub struct DataVisApp {
    store: Arc<dyn ChannelStore>,
    channels: ChannelRegistry,
    registry: PanelRegistry,
    workspace: Workspace,
    layout_path: PathBuf,
    add_panel: AddPanelDialog,
    new_screen_name: String,
    status: String,
}

impl DataVisApp {
    pub fn new(
        store: Arc<dyn ChannelStore>,
        channels: ChannelRegistry,
        registry: PanelRegistry,
        workspace: Workspace,
        layout_path: PathBuf,
    ) -> Self {
        let panel_type = registry
            .type_names()
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default();
        Self {
            store,
            channels,
            registry,
            workspace,
            layout_path,
            add_panel: AddPanelDialog { panel_type, ..Default::default() },
            new_screen_name: String::new(),
            status: String::new(),
        }
    }

    fn save_layout(&mut self) {
        self.status = match self.workspace.to_config().save(&self.layout_path) {
            Ok(()) => format!("layout saved to {}", self.layout_path.display()),
            Err(e) => format!("layout save failed: {e}"),
        };
    }

    fn load_layout(&mut self) {
        match LayoutConfig::load(&self.layout_path) {
            Ok(cfg) => {
                self.workspace = Workspace::from_config(&cfg, &self.registry, &self.channels);
                self.status = format!("layout loaded from {}", self.layout_path.display());
            }
            Err(e) => self.status = format!("layout load failed: {e}"),
        }
    }

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Save layout").clicked() {
                        self.save_layout();
                        ui.close_menu();
                    }
                    if ui.button("Load layout").clicked() {
                        self.load_layout();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("screen:");
                let mut selected = self.workspace.active.clone();
                egui::ComboBox::from_id_source("screen-select")
                    .selected_text(&selected)
                    .show_ui(ui, |ui| {
                        for name in self.workspace.screens.keys() {
                            ui.selectable_value(&mut selected, name.clone(), name);
                        }
                    });
                if selected != self.workspace.active {
                    self.workspace.active = selected;
                }
                ui.menu_button("+ screen", |ui| {
                    ui.text_edit_singleline(&mut self.new_screen_name);
                    if ui.button("Create").clicked() && !self.new_screen_name.is_empty() {
                        let name = std::mem::take(&mut self.new_screen_name);
                        self.workspace.add_screen(&name);
                        ui.close_menu();
                    }
                });
                if ui.button("+ panel").clicked() {
                    self.add_panel.open = true;
                }
                ui.separator();
                ui.colored_label(egui::Color32::LIGHT_GREEN, "LIVE");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.status);
                });
            });
        });
    }

    fn add_panel_window(&mut self, ctx: &egui::Context) {
        if !self.add_panel.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Add panel")
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::ComboBox::from_label("type")
                    .selected_text(&self.add_panel.panel_type)
                    .show_ui(ui, |ui| {
                        for t in self.registry.type_names() {
                            ui.selectable_value(&mut self.add_panel.panel_type, t.to_string(), t);
                        }
                    });
                ui.label(match self.add_panel.panel_type.as_str() {
                    "xy_scatter" => "select exactly 2 channels (x first, then y)",
                    "waveform" | "log" => "select one or more channels",
                    _ => "select exactly 1 channel",
                });
                ui.separator();
                for id in self.channels.iter_ids() {
                    let name = self.channels.meta(id).name.clone();
                    let mut checked = self.add_panel.selected.contains(&name);
                    if ui.checkbox(&mut checked, &name).changed() {
                        if checked {
                            self.add_panel.selected.push(name);
                        } else {
                            self.add_panel.selected.retain(|n| n != &name);
                        }
                    }
                }
                ui.separator();
                let entry = build_panel_entry(&self.add_panel.panel_type, &self.add_panel.selected);
                if ui
                    .add_enabled(entry.is_some(), egui::Button::new("Add"))
                    .clicked()
                {
                    if let Some(e) = entry {
                        if let Err(err) =
                            self.workspace.add_panel(&e, &self.registry, &self.channels)
                        {
                            self.status = format!("add panel failed: {err}");
                        }
                        self.add_panel.selected.clear();
                        self.add_panel.open = false;
                    }
                }
            });
        if !open {
            self.add_panel.open = false;
        }
    }
}

impl eframe::App for DataVisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Live data keeps coming whether or not there is input.
        ctx.request_repaint();
        self.menu_bar(ctx);
        self.toolbar(ctx);
        self.add_panel_window(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            self.workspace.ui(ui, self.store.as_ref());
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Spec: layout auto-saves on exit.
        let _ = self.workspace.to_config().save(&self.layout_path);
    }
}
```

API drift note: if `eframe::App::on_exit` has a different signature in the installed version (no `glow` parameter, or gated behind a feature), match the trait's actual signature — the body stays one line.

- [ ] **Step 4: Update main.rs**

Replace `src/main.rs`:

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::anyhow;

use datavis::app::DataVisApp;
use datavis::config::{ChannelRegistry, LayoutConfig};
use datavis::store::{ChannelStore, LiveStore};
use datavis::viz::PanelRegistry;
use datavis::workspace::Workspace;

fn main() -> anyhow::Result<()> {
    let demo = std::env::args().any(|a| a == "--demo");
    let layout_path = PathBuf::from("layout.toml");

    let channels = ChannelRegistry::load(Path::new("channels.toml"))?;
    let layout = LayoutConfig::load(&layout_path)?;

    let store = Arc::new(LiveStore::from_registry(&channels));
    if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
    }

    let registry = PanelRegistry::with_builtins();
    let workspace = Workspace::from_config(&layout, &registry, &channels);

    let dyn_store: Arc<dyn ChannelStore> = store;
    let app = DataVisApp::new(dyn_store, channels, registry, workspace, layout_path);

    eframe::run_native(
        "datavis",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}
```

- [ ] **Step 5: Update the demo layout**

Replace `layout.toml` (repo root):

```toml
[screens.main]

[[screens.main.panels]]
type = "waveform"
channels = ["demo.sine"]
time_window_s = 5.0
cursors = true

[[screens.main.panels]]
type = "gauge"
channel = "demo.sine"
min = -10.0
max = 10.0

[[screens.main.panels]]
type = "numeric"
channel = "demo.counter"

[[screens.main.panels]]
type = "state_graph"
channel = "demo.enabled"
states = { 0 = "OFF", 1 = "ON" }
time_window_s = 30.0

[[screens.main.panels]]
type = "log"
channels = ["demo.log"]
max_lines = 200

[screens.analysis]

[[screens.analysis.panels]]
type = "spectrum"
channel = "demo.sine"
fft_size = 1024
window = "hann"

[[screens.analysis.panels]]
type = "xy_scatter"
x_channel = "demo.sine"
y_channel = "demo.counter"
time_window_s = 2.0
```

- [ ] **Step 6: Run all tests + build**

Run: `cargo test --lib && cargo build`
Expected: full suite PASS (foundation 30 + this plan's new tests), binary builds.

- [ ] **Step 7: Manual smoke test (needs a display)**

Run: `cargo run -- --demo`
Expected: window with menu bar + toolbar; screen combo switches between `analysis` and `main`; `main` shows waveform (scrolling sine, click/ctrl-click places cursors and a stats table appears), gauge, numeric, state graph bands toggling OFF/ON, and the log filling; tabs/panes can be dragged around; "+ panel" dialog adds a panel; quitting writes `layout.toml` with `tiles_json` entries.

If no display: `cargo build` success + full test suite is the gate; note the skipped GUI check in the task report.

- [ ] **Step 8: Commit**

```bash
git add src/app.rs src/main.rs layout.toml
git commit -m "feat: app shell with menus, screen switching, add-panel dialog, layout auto-save"
```

---

## Spec Coverage Note

This plan completes the spec's `viz` component (all 7 panel types from the panel table, cursors/measurements on Waveform, Spectrum non-uniform-ts warning banner, wrong-type inline errors) and the `layout` component (egui_tiles splits/tabs/drag-drop, multiple named screens switchable from the toolbar, add-panel flow, save/load via menu, auto-save on exit, tree persistence in layout.toml). Deviations from spec wording, chosen deliberately: "right-click panel → Add panel" is implemented as a toolbar "+ panel" button (right-click inside panes conflicts with plot interactions); cursors are click / ctrl-click rather than draggable lines. Still deferred to later plans: ZMQ ingest, recorder + replay (incl. replay controls in the toolbar), capture triggers, status bar with connection state, 100 kHz integration test.
