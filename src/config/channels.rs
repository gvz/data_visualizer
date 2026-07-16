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
