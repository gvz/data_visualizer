use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::RwLock;

use anyhow::Context;
use serde::Deserialize;

use crate::types::{ChannelId, ChannelMeta, SampleType};

/// One channel entry from channels.toml. eu_scale/eu_offset are consumed by
/// ingest; everything display-relevant is mirrored into ChannelMeta.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    /// ZMQ/protobuf source topic. None for MQTT-only channels.
    #[serde(default)]
    pub topic: Option<String>,
    /// Dotted proto field path for the value. None for MQTT-only channels.
    #[serde(default)]
    pub proto_path: Option<String>,
    /// Dotted proto field path for the timestamp. None for MQTT-only channels.
    #[serde(default)]
    pub ts_path: Option<String>,
    /// MQTT topic this channel subscribes to. None for ZMQ-only channels.
    #[serde(default)]
    pub mqtt_topic: Option<String>,
    #[serde(rename = "type")]
    pub sample_type: SampleType,
    #[serde(default)]
    pub unit: String,
    #[serde(default = "default_color")]
    pub color: String,
    /// Raw file value; `None` when omitted. The resolved rate (after applying
    /// `[defaults]` and the hardcoded fallback) lives in `ChannelMeta` — read
    /// `meta(id).max_rate` for ring sizing, never this field.
    #[serde(default)]
    pub max_rate: Option<u32>,
    /// Raw file value; `None` when omitted. Resolved value is in `ChannelMeta`
    /// — read `meta(id).history_s`, never this field.
    #[serde(default)]
    pub history_s: Option<f64>,
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
fn default_eu_scale() -> f64 {
    1.0
}
fn default_max_lines() -> usize {
    500
}

/// Resolve a static channel's max_rate: channel value, else [defaults], else 1000.
fn resolve_static_rate(cfg: Option<u32>, def: Option<u32>) -> u32 {
    cfg.or(def).unwrap_or(1000)
}
/// Resolve a static channel's history_s: channel value, else [defaults], else 10.0.
fn resolve_static_history(cfg: Option<f64>, def: Option<f64>) -> f64 {
    cfg.or(def).unwrap_or(10.0)
}

/// Global fallbacks for channels that omit `max_rate`/`history_s`. Optional in
/// channels.toml; precedence is per-channel value → these → hardcoded.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ChannelDefaults {
    max_rate: Option<u32>,
    history_s: Option<f64>,
}

// No `deny_unknown_fields`: config.toml is shared with the layout, so the
// `default_window_s` scalar and `[screens]` tables live here too and must be
// ignored by the channel parser (LayoutConfig reads those).
#[derive(Debug, Deserialize)]
struct ChannelsFile {
    #[serde(default)]
    defaults: ChannelDefaults,
    // BTreeMap: sorted names → deterministic ChannelId assignment.
    // Defaults to empty so a channels.toml with no channels (or only
    // `[defaults]`, or an empty file) is valid.
    #[serde(default)]
    channels: BTreeMap<String, ChannelConfig>,
}

/// Channel table built at startup from channels.toml, plus an append-only
/// dynamic tier for channels registered at runtime (e.g. an MQTT topic
/// dropped onto a panel). Ids `< configs.len()` index the static tier; ids
/// `>= configs.len()` index the append-only dynamic tier. Dynamic growth is
/// interior-mutable (`&self`) and preserves `&`-borrows via `boxcar::Vec`.
#[derive(Debug)]
pub struct ChannelRegistry {
    ids: HashMap<String, ChannelId>,
    configs: Vec<ChannelConfig>,
    metas: Vec<ChannelMeta>,
    /// Parsed [defaults]; applied to runtime-registered dynamic channels.
    defaults: ChannelDefaults,
    /// Runtime-added name → id. Written only from the UI thread on drop.
    dyn_ids: RwLock<HashMap<String, ChannelId>>,
    dyn_configs: boxcar::Vec<ChannelConfig>,
    dyn_metas: boxcar::Vec<ChannelMeta>,
}

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

impl ChannelRegistry {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let file: ChannelsFile = toml::from_str(s).context("parsing config.toml channels")?;
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

    /// Number of statically-configured channels; dynamic ids start here.
    fn n_static(&self) -> usize {
        self.configs.len()
    }

    /// Register a runtime channel (e.g. a discovered MQTT topic dropped onto a
    /// panel). Idempotent: if `name` already exists, returns its id without
    /// allocating a new slot. Otherwise appends a new dynamic channel and
    /// returns its id. `&self` — interior-mutable, safe to call while ingest
    /// writes samples to existing channels.
    pub fn add_dynamic(
        &self,
        name: &str,
        mqtt_topic: &str,
        sample_type: SampleType,
    ) -> ChannelId {
        if let Some(id) = self.id(name) {
            return id;
        }
        let mut dyn_ids = self.dyn_ids.write().unwrap();
        // Re-check under the lock in case of a concurrent add.
        if let Some(&id) = dyn_ids.get(name) {
            return id;
        }
        let (cfg, meta) =
            dynamic_channel(name.to_string(), mqtt_topic.to_string(), sample_type, &self.defaults);
        // Push config first, then meta: `len()`/`iter_ids()` count metas, so a
        // channel becomes visible only once its config is already in place.
        self.dyn_configs.push(cfg);
        let idx = self.dyn_metas.push(meta);
        let id = ChannelId((self.n_static() + idx) as u32);
        dyn_ids.insert(name.to_string(), id);
        id
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml_str(&s)
    }

    pub fn id(&self, name: &str) -> Option<ChannelId> {
        self.ids
            .get(name)
            .copied()
            .or_else(|| self.dyn_ids.read().unwrap().get(name).copied())
    }

    pub fn meta(&self, id: ChannelId) -> &ChannelMeta {
        let i = id.0 as usize;
        if i < self.n_static() {
            &self.metas[i]
        } else {
            self.dyn_metas.get(i - self.n_static()).expect("dynamic channel id out of range")
        }
    }

    pub fn config(&self, id: ChannelId) -> &ChannelConfig {
        let i = id.0 as usize;
        if i < self.n_static() {
            &self.configs[i]
        } else {
            self.dyn_configs.get(i - self.n_static()).expect("dynamic channel id out of range")
        }
    }

    pub fn len(&self) -> usize {
        self.n_static() + self.dyn_metas.count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter_ids(&self) -> impl Iterator<Item = ChannelId> + '_ {
        (0..self.len() as u32).map(ChannelId)
    }

    /// Return the channel whose `mqtt_topic` equals `topic`, if any.
    pub fn id_by_mqtt_topic(&self, topic: &str) -> Option<ChannelId> {
        self.iter_ids()
            .find(|&id| self.config(id).mqtt_topic.as_deref() == Some(topic))
    }
}

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
        assert_eq!(cfg.topic.as_deref(), Some("accel"));
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
    fn mqtt_only_channel_parses_without_zmq_fields() {
        let toml = r#"
[channels."sensor/temp"]
mqtt_topic = "home/sensors/temp"
type = "float"
"#;
        let reg = ChannelRegistry::from_toml_str(toml).unwrap();
        let id = reg.id("sensor/temp").unwrap();
        let cfg = reg.config(id);
        assert_eq!(cfg.mqtt_topic.as_deref(), Some("home/sensors/temp"));
        assert!(cfg.topic.is_none());
        assert!(cfg.proto_path.is_none());
        assert!(cfg.ts_path.is_none());
    }

    #[test]
    fn add_dynamic_appends_and_resolves() {
        let reg = ChannelRegistry::from_toml_str(EXAMPLE).unwrap();
        let n = reg.len();
        let id = reg.add_dynamic("home/sensors/temp", "home/sensors/temp", SampleType::Float);
        assert_eq!(id.0 as usize, n, "dynamic id continues after static ids");
        assert_eq!(reg.len(), n + 1);
        // Resolvable by name and by mqtt_topic; meta/config reflect the type.
        assert_eq!(reg.id("home/sensors/temp"), Some(id));
        assert_eq!(reg.id_by_mqtt_topic("home/sensors/temp"), Some(id));
        assert_eq!(reg.meta(id).name, "home/sensors/temp");
        assert_eq!(reg.meta(id).sample_type, SampleType::Float);
        assert_eq!(reg.config(id).mqtt_topic.as_deref(), Some("home/sensors/temp"));
        // Idempotent: same name returns the same id, no new slot.
        let again = reg.add_dynamic("home/sensors/temp", "home/sensors/temp", SampleType::Int);
        assert_eq!(again, id);
        assert_eq!(reg.len(), n + 1);
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

    #[test]
    fn empty_file_is_valid_and_has_no_channels() {
        let reg = ChannelRegistry::from_toml_str("").unwrap();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn defaults_only_file_is_valid_with_no_channels() {
        let reg = ChannelRegistry::from_toml_str(
            r#"
[defaults]
max_rate = 100000
history_s = 5.0
"#,
        )
        .unwrap();
        assert!(reg.is_empty());
        // A later dynamic channel still picks up the [defaults].
        let id = reg.add_dynamic("dyn/topic", "dyn/topic", SampleType::Float);
        assert_eq!(reg.meta(id).max_rate, 100_000);
        assert_eq!(reg.meta(id).history_s, 5.0);
    }
}
