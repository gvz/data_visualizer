# Global `[defaults]` Section in channels.toml Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional top-level `[defaults]` table to `channels.toml` that supplies `max_rate` and `history_s` for any channel (static or runtime-discovered) that omits them.

**Architecture:** Parse `[defaults]` into a `ChannelDefaults { max_rate: Option<u32>, history_s: Option<f64> }`; make `ChannelConfig.max_rate`/`history_s` optional so "omitted" is distinguishable; resolve each field at parse time with precedence `per-channel → [defaults] → hardcoded` into `ChannelMeta` (which drives ring sizing). The registry retains the parsed `ChannelDefaults` so the runtime `dynamic_channel` path applies them too. All changes are confined to `src/config/channels.rs`.

**Tech Stack:** Rust, serde, toml.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-23-channel-defaults-section-design.md`.
- Only `max_rate` and `history_s` are settable in `[defaults]`. No other field.
- Precedence per field: (1) channel's own value, (2) `[defaults]` value, (3) hardcoded fallback.
- Hardcoded fallbacks differ by origin: **static** `max_rate=1000`, `history_s=10.0`; **dynamic** `max_rate=100`, `history_s=30.0`. These preserve today's behavior when `[defaults]` is absent.
- `[defaults]` is optional; omitting it (or either key) must not change current behavior.
- Keep `deny_unknown_fields` on `ChannelsFile`, `ChannelConfig`, and `ChannelDefaults` (typo detection).
- No new validation of `max_rate=0` / negative `history_s`; `SoaRing::new` already clamps to 16 slots.
- Commit messages: plain Conventional Commits. NO `Co-Authored-By`/`Claude-Session`/self-attribution trailer.

---

### Task 1: `[defaults]` parsing, resolution, and dynamic-channel wiring

**Files:**
- Modify: `src/config/channels.rs`
- Test: inline `#[cfg(test)] mod tests` in `src/config/channels.rs`

**Interfaces:**
- Consumes: existing `ChannelRegistry::from_toml_str`, `add_dynamic`, `ChannelMeta` (from `crate::types`), `SoaRing` sizing in `src/store/live.rs` (unchanged — reads `meta.max_rate`/`meta.history_s`).
- Produces: a `[defaults]` table understood by `channels.toml`; `ChannelConfig.max_rate: Option<u32>` and `history_s: Option<f64>`; resolved values in `ChannelMeta`. No new public function is consumed by other files.

- [ ] **Step 1: Write the failing tests**

Append these tests inside the existing `#[cfg(test)] mod tests { … }` block in `src/config/channels.rs` (add them after the last existing test, before the closing `}`):

```rust
    #[test]
    fn defaults_apply_when_channel_omits_fields() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[defaults]
max_rate = 100000
history_s = 5.0

[channels."x"]
mqtt_topic = "t"
type = "float"
"#,
        )
        .unwrap();
        let id = reg.id("x").unwrap();
        assert_eq!(reg.meta(id).max_rate, 100_000);
        assert_eq!(reg.meta(id).history_s, 5.0);
    }

    #[test]
    fn per_channel_value_overrides_defaults() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[defaults]
max_rate = 100000
history_s = 5.0

[channels."y"]
mqtt_topic = "t"
type = "float"
max_rate = 1000
"#,
        )
        .unwrap();
        let id = reg.id("y").unwrap();
        assert_eq!(reg.meta(id).max_rate, 1000);
        // history_s not set on the channel → inherits [defaults]
        assert_eq!(reg.meta(id).history_s, 5.0);
    }

    #[test]
    fn no_defaults_table_keeps_static_hardcoded() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[channels."z"]
mqtt_topic = "t"
type = "float"
"#,
        )
        .unwrap();
        let id = reg.id("z").unwrap();
        assert_eq!(reg.meta(id).max_rate, 1000);
        assert_eq!(reg.meta(id).history_s, 10.0);
    }

    #[test]
    fn partial_defaults_falls_back_per_field() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[defaults]
max_rate = 50000

[channels."x"]
mqtt_topic = "t"
type = "float"
"#,
        )
        .unwrap();
        let id = reg.id("x").unwrap();
        assert_eq!(reg.meta(id).max_rate, 50_000);
        // history_s absent everywhere → static hardcoded 10.0
        assert_eq!(reg.meta(id).history_s, 10.0);
    }

    #[test]
    fn defaults_govern_dynamic_channel() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[defaults]
max_rate = 100000
history_s = 5.0

[channels."x"]
mqtt_topic = "t"
type = "float"
"#,
        )
        .unwrap();
        let id = reg.add_dynamic("dyn/topic", "dyn/topic", SampleType::Float);
        assert_eq!(reg.meta(id).max_rate, 100_000);
        assert_eq!(reg.meta(id).history_s, 5.0);
    }

    #[test]
    fn dynamic_channel_hardcoded_when_no_defaults() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[channels."x"]
mqtt_topic = "t"
type = "float"
"#,
        )
        .unwrap();
        let id = reg.add_dynamic("dyn/topic", "dyn/topic", SampleType::Float);
        assert_eq!(reg.meta(id).max_rate, 100);
        assert_eq!(reg.meta(id).history_s, 30.0);
    }

    #[test]
    fn unknown_field_in_defaults_is_rejected() {
        let err = ChannelRegistry::from_toml_str(
            r#"
[defaults]
max_rate = 100000
bogus = 1

[channels."x"]
mqtt_topic = "t"
type = "float"
"#,
        );
        assert!(err.is_err(), "unknown [defaults] key must be rejected");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::channels`
Expected: FAIL — compile errors (`[defaults]` unknown field is rejected by `deny_unknown_fields`, and the resolution behavior does not yet exist). This confirms the tests bind to the intended behavior.

- [ ] **Step 3: Add `ChannelDefaults` and make the two `ChannelConfig` fields optional**

In `src/config/channels.rs`, change the `max_rate`/`history_s` fields of `ChannelConfig` (currently lines 33-36) from:

```rust
    #[serde(default = "default_max_rate")]
    pub max_rate: u32,
    #[serde(default = "default_history_s")]
    pub history_s: f64,
```

to:

```rust
    #[serde(default)]
    pub max_rate: Option<u32>,
    #[serde(default)]
    pub history_s: Option<f64>,
```

Remove the now-unused `default_max_rate` and `default_history_s` functions (currently lines 48-53). Keep `default_color`, `default_eu_scale`, and `default_max_lines`.

Add the defaults struct immediately above `struct ChannelsFile` (currently line 61):

```rust
/// Global fallbacks for channels that omit `max_rate`/`history_s`. Optional in
/// channels.toml; precedence is per-channel value → these → hardcoded.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ChannelDefaults {
    max_rate: Option<u32>,
    history_s: Option<f64>,
}
```

Extend `ChannelsFile` (currently lines 61-66) to carry the table:

```rust
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelsFile {
    #[serde(default)]
    defaults: ChannelDefaults,
    // BTreeMap: sorted names → deterministic ChannelId assignment.
    channels: BTreeMap<String, ChannelConfig>,
}
```

- [ ] **Step 4: Add resolution helpers**

Add these free functions in `src/config/channels.rs` (place them next to the other `fn default_*` helpers):

```rust
/// Resolve a static channel's max_rate: channel value, else [defaults], else 1000.
fn resolve_static_rate(cfg: Option<u32>, def: Option<u32>) -> u32 {
    cfg.or(def).unwrap_or(1000)
}
/// Resolve a static channel's history_s: channel value, else [defaults], else 10.0.
fn resolve_static_history(cfg: Option<f64>, def: Option<f64>) -> f64 {
    cfg.or(def).unwrap_or(10.0)
}
```

- [ ] **Step 5: Store the defaults on the registry and resolve static metas**

Add a field to `ChannelRegistry` (in the struct definition, currently lines 73-82) after `metas`:

```rust
    /// Parsed [defaults]; applied to runtime-registered dynamic channels.
    defaults: ChannelDefaults,
```

In `from_toml_str` (currently lines 114-140), destructure `defaults` from the file and use the resolvers when building each `ChannelMeta`. Replace the body so it reads:

```rust
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let file: ChannelsFile = toml::from_str(s).context("parsing channels.toml")?;
        let defaults = file.defaults;
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
                max_rate: resolve_static_rate(cfg.max_rate, defaults.max_rate),
                history_s: resolve_static_history(cfg.history_s, defaults.history_s),
                max_lines: cfg.max_lines,
            });
            configs.push(cfg);
        }
        Ok(Self {
            ids,
            configs,
            metas,
            defaults,
            dyn_ids: RwLock::new(HashMap::new()),
            dyn_configs: boxcar::Vec::new(),
            dyn_metas: boxcar::Vec::new(),
        })
    }
```

- [ ] **Step 6: Route the dynamic path through the defaults**

Change `dynamic_channel` (currently lines 86-111) to take the defaults and apply them, and to build a `ChannelConfig` whose two now-optional fields are `None` (the runtime channel has no per-channel override; the resolved values live in `meta`):

```rust
/// Defaults for a runtime-registered MQTT channel. Rate/history come from the
/// file's [defaults] when present, else the hardcoded dynamic fallbacks
/// (100 Hz / 30 s — MQTT is low-rate).
fn dynamic_channel(
    name: String,
    mqtt_topic: String,
    sample_type: SampleType,
    defaults: &ChannelDefaults,
) -> (ChannelConfig, ChannelMeta) {
    let max_rate = defaults.max_rate.unwrap_or(100);
    let history_s = defaults.history_s.unwrap_or(30.0);
    let cfg = ChannelConfig {
        topic: None,
        proto_path: None,
        ts_path: None,
        mqtt_topic: Some(mqtt_topic),
        sample_type,
        unit: String::new(),
        color: default_color(),
        max_rate: None,
        history_s: None,
        eu_scale: 1.0,
        eu_offset: 0.0,
        max_lines: default_max_lines(),
    };
    let meta = ChannelMeta {
        name,
        sample_type,
        unit: String::new(),
        color: default_color(),
        max_rate,
        history_s,
        max_lines: cfg.max_lines,
    };
    (cfg, meta)
}
```

Update the call site in `add_dynamic` (currently line 166) to pass the stored defaults:

```rust
        let (cfg, meta) =
            dynamic_channel(name.to_string(), mqtt_topic.to_string(), sample_type, &self.defaults);
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib config::channels`
Expected: PASS — the seven new tests plus the pre-existing `config::channels` tests are green.

- [ ] **Step 8: Run the full suite and check for warnings**

Run: `cargo build`
Expected: compiles with no new warnings (the removed `default_max_rate`/`default_history_s` leave no dangling references).

Run: `cargo test`
Expected: PASS — full suite green (the store tests that set explicit `max_rate` in their TOML are unaffected, since an explicit per-channel value still wins).

- [ ] **Step 9: Commit**

```bash
git add src/config/channels.rs
git commit -m "feat: global [defaults] max_rate/history_s in channels.toml"
```

---

## Notes for the Implementer

- Do NOT stage unrelated working-tree changes. Only `git add src/config/channels.rs`. The tree has an unrelated modified `layout.toml` and untracked files — leave them.
- The `[defaults]` table must sit at the top level of `channels.toml`, a sibling of `[channels.…]`, not nested under `[channels]`.
- Do not add validation for `max_rate = 0` or negative `history_s`; `SoaRing::new` clamps to a 16-slot minimum and the spec declares this the accepted behavior (YAGNI).
- `config()` now returns `&ChannelConfig` with `Option` `max_rate`/`history_s`; that is intentional (it reflects the raw file). Resolved values are read via `meta()`, as all existing consumers already do.
