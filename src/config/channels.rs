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
    /// Runtime-added name → id. Written only from the UI thread on drop.
    dyn_ids: RwLock<HashMap<String, ChannelId>>,
    dyn_configs: boxcar::Vec<ChannelConfig>,
    dyn_metas: boxcar::Vec<ChannelMeta>,
}

/// Defaults for a runtime-registered MQTT channel. MQTT is low-rate, so the
/// ring is sized modestly.
fn dynamic_channel(name: String, mqtt_topic: String, sample_type: SampleType) -> (ChannelConfig, ChannelMeta) {
    let cfg = ChannelConfig {
        topic: None,
        proto_path: None,
        ts_path: None,
        mqtt_topic: Some(mqtt_topic),
        sample_type,
        unit: String::new(),
        color: default_color(),
        max_rate: 100,
        history_s: 30.0,
        eu_scale: 1.0,
        eu_offset: 0.0,
        max_lines: default_max_lines(),
    };
    let meta = ChannelMeta {
        name,
        sample_type,
        unit: String::new(),
        color: default_color(),
        max_rate: cfg.max_rate,
        history_s: cfg.history_s,
        max_lines: cfg.max_lines,
    };
    (cfg, meta)
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
        Ok(Self {
            ids,
            configs,
            metas,
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
        let (cfg, meta) = dynamic_channel(name.to_string(), mqtt_topic.to_string(), sample_type);
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
}
