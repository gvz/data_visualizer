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
}
