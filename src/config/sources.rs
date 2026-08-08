use serde::Deserialize;
use anyhow::Context;

/// Collection of all data source configurations from the `[sources]` section.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourcesConfig {
    pub zmq: Vec<ZmqSourceConfig>,
    pub mqtt: Vec<MqttSourceConfig>,
    pub websocket: Vec<WsSourceConfig>,
    pub bridge: Vec<crate::ingest::bridge::BridgeConfig>,
}

/// A ZMQ data source configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZmqSourceConfig {
    pub endpoint: String,
    pub schema: String,
}

/// An MQTT data source configuration. `client_id` defaults to "datavis" if not specified.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttSourceConfig {
    pub broker: String,
    #[serde(default = "default_client_id")]
    pub client_id: String,
}

fn default_client_id() -> String {
    "datavis".to_string()
}

/// A WebSocket data source configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WsSourceConfig {
    pub listen: String,
}

// `[sources]` may grow other keys later, so this wrapper does not deny unknown
// fields; and the top-level doc ignores every section except `sources`.
#[derive(Deserialize)]
struct DocWrapper {
    sources: Option<RawSources>,
}

#[derive(Deserialize, Default)]
struct RawSources {
    #[serde(default)]
    zmq: Vec<ZmqSourceConfig>,
    #[serde(default)]
    mqtt: Vec<MqttSourceConfig>,
    #[serde(default)]
    websocket: Vec<WsSourceConfig>,
    #[serde(default)]
    bridge: Vec<crate::ingest::bridge::BridgeConfig>,
}

impl SourcesConfig {
    /// Parse the `[sources]` section from a TOML string. An absent section yields
    /// an empty `SourcesConfig`.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let doc: DocWrapper = toml::from_str(s).context("parsing [sources]")?;
        let r = doc.sources.unwrap_or_default();
        Ok(Self {
            zmq: r.zmq,
            mqtt: r.mqtt,
            websocket: r.websocket,
            bridge: r.bridge,
        })
    }

    /// Load and parse the `[sources]` section from a TOML file.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let s =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml_str(&s)
    }
}

impl SourcesConfig {
    /// Convert every entry in this config into a `SourceSpec` suitable for
    /// passing to `SourceManager::add`.
    pub fn specs(&self) -> Vec<crate::ingest::SourceSpec> {
        use crate::ingest::SourceSpec as S;
        let mut v = Vec::new();
        for z in &self.zmq {
            v.push(S::Zmq { endpoint: z.endpoint.clone(), schema: z.schema.clone() });
        }
        for m in &self.mqtt {
            v.push(S::Mqtt { broker: m.broker.clone(), client_id: m.client_id.clone() });
        }
        for w in &self.websocket {
            v.push(S::WebSocket { listen: w.listen.clone() });
        }
        for b in &self.bridge {
            v.push(S::Bridge(b.clone()));
        }
        v
    }

    /// Append a `SourceSpec` to the appropriate sub-list.
    ///
    /// Used by `SourceManager` when persisting a GUI-added source back to
    /// `SourcesConfig` before writing `config.toml`.
    pub fn push(&mut self, spec: &crate::ingest::SourceSpec) {
        use crate::ingest::SourceSpec as S;
        match spec {
            S::Zmq { endpoint, schema } => self.zmq.push(ZmqSourceConfig {
                endpoint: endpoint.clone(),
                schema: schema.clone(),
            }),
            S::Mqtt { broker, client_id } => self.mqtt.push(MqttSourceConfig {
                broker: broker.clone(),
                client_id: client_id.clone(),
            }),
            S::WebSocket { listen } => self.websocket.push(WsSourceConfig { listen: listen.clone() }),
            S::Bridge(cfg) => self.bridge.push(cfg.clone()),
        }
    }
}

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

impl SourcesConfig {
    pub fn merge_into(&self, existing: &str) -> anyhow::Result<String> {
        let mut doc: DocumentMut = existing.parse().context("parsing existing config")?;
        doc.remove("sources");
        if self.is_empty() {
            return Ok(doc.to_string());
        }
        let mut sources = Table::new();
        sources.set_implicit(true);

        let mut zmq = ArrayOfTables::new();
        for z in &self.zmq {
            let mut t = Table::new();
            t["endpoint"] = value(z.endpoint.as_str());
            t["schema"] = value(z.schema.as_str());
            zmq.push(t);
        }
        if !zmq.is_empty() { sources.insert("zmq", Item::ArrayOfTables(zmq)); }

        let mut mqtt = ArrayOfTables::new();
        for m in &self.mqtt {
            let mut t = Table::new();
            t["broker"] = value(m.broker.as_str());
            t["client_id"] = value(m.client_id.as_str());
            mqtt.push(t);
        }
        if !mqtt.is_empty() { sources.insert("mqtt", Item::ArrayOfTables(mqtt)); }

        let mut ws = ArrayOfTables::new();
        for w in &self.websocket {
            let mut t = Table::new();
            t["listen"] = value(w.listen.as_str());
            ws.push(t);
        }
        if !ws.is_empty() { sources.insert("websocket", Item::ArrayOfTables(ws)); }

        let mut bridge = ArrayOfTables::new();
        for b in &self.bridge {
            let mut t = Table::new();
            t["name"] = value(b.name.as_str());
            t["command"] = value(b.command.as_str());
            let mut a = Array::new();
            for arg in &b.args { a.push(arg.clone()); }
            t["args"] = value(a);
            bridge.push(t);
        }
        if !bridge.is_empty() { sources.insert("bridge", Item::ArrayOfTables(bridge)); }

        doc.insert("sources", Item::Table(sources));
        Ok(doc.to_string())
    }

    pub fn is_empty(&self) -> bool {
        self.zmq.is_empty() && self.mqtt.is_empty()
            && self.websocket.is_empty() && self.bridge.is_empty()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let out = self.merge_into(&existing)?;
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_kinds() {
        let s = r#"
[[sources.zmq]]
endpoint = "tcp://localhost:5555"
schema = "schema.proto"

[[sources.mqtt]]
broker = "localhost:1883"

[[sources.websocket]]
listen = "0.0.0.0:9001"

[[sources.bridge]]
name = "acme"
command = "/opt/acme"
args = ["--fast"]
"#;
        let c = SourcesConfig::from_toml_str(s).unwrap();
        assert_eq!(
            c.zmq,
            vec![ZmqSourceConfig {
                endpoint: "tcp://localhost:5555".into(),
                schema: "schema.proto".into()
            }]
        );
        assert_eq!(c.mqtt[0].client_id, "datavis"); // default
        assert_eq!(c.websocket[0].listen, "0.0.0.0:9001");
        assert_eq!(c.bridge[0].name, "acme");
    }

    #[test]
    fn absent_section_is_empty() {
        let c = SourcesConfig::from_toml_str("default_window_s = 5.0\n").unwrap();
        assert!(c.zmq.is_empty() && c.mqtt.is_empty() && c.websocket.is_empty() && c.bridge.is_empty());
    }

    #[test]
    fn save_preserves_other_sections_and_round_trips() {
        let existing = r#"default_window_s = 30.0

[defaults]
max_rate = 100000  # keep me

# hand-authored channel
[channels."volt.l1"]
type = "float"
"#;
        let mut c = SourcesConfig::default();
        c.zmq.push(ZmqSourceConfig { endpoint: "tcp://x:1".into(), schema: "s.proto".into() });
        c.mqtt.push(MqttSourceConfig { broker: "h:1883".into(), client_id: "datavis".into() });
        let out = c.merge_into(existing).unwrap();
        assert!(out.contains("# keep me"), "comment preserved");
        assert!(out.contains("[channels.\"volt.l1\"]"), "channels preserved");
        let reparsed = SourcesConfig::from_toml_str(&out).unwrap();
        assert_eq!(reparsed, c);
    }

    #[test]
    fn save_with_empty_sources_removes_table() {
        let existing = "[[sources.zmq]]\nendpoint=\"a\"\nschema=\"b\"\n[defaults]\nmax_rate=1\n";
        let out = SourcesConfig::default().merge_into(existing).unwrap();
        let reparsed = SourcesConfig::from_toml_str(&out).unwrap();
        assert!(reparsed.zmq.is_empty());
        assert!(out.contains("[defaults]"));
    }
}
