use std::collections::BTreeMap;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{Client, Event, MqttOptions, Packet, QoS};

use crate::config::ChannelRegistry;
use crate::dynamic_channel::MqttTopicMap;
use crate::ingest::source::{DataSource, Discovery, SourceHandle};
use crate::ingest::{CONNECTING, LIVE};
use crate::record::RecordMsg;
use crate::store::ChannelStore;

pub struct MqttConfig {
    /// Broker address as "host:port" or "host" (defaults to port 1883).
    pub broker_url: String,
    pub client_id: String,
}

pub struct MqttSource {
    config: MqttConfig,
    pub(crate) topic_map: Arc<MqttTopicMap>,
}

impl MqttSource {
    pub fn new(config: MqttConfig, registry: &ChannelRegistry) -> Self {
        Self { config, topic_map: crate::ingest::source::topic_map_from_registry(registry) }
    }
}

impl DataSource for MqttSource {
    fn name(&self) -> &str {
        "mqtt"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let discovered: Arc<Mutex<BTreeMap<String, String>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let (host, port) = parse_broker_url(&self.config.broker_url);
        let mut opts = MqttOptions::new(self.config.client_id.clone(), host, port);
        opts.set_keep_alive(Duration::from_secs(30));

        let record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>> =
            Arc::new(Mutex::new(None));
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));

        let disc = discovered.clone();
        let map = self.topic_map.clone();
        let rec = record_sender.clone();
        let state = conn_state.clone();
        std::thread::spawn(move || {
            run_loop(opts, map, disc, store, rec, state);
        });

        SourceHandle {
            name: "mqtt".to_string(),
            conn_state,
            record_sender,
            discovery: Some(Discovery { discovered, topic_map: self.topic_map }),
            schema_bytes: None,
        }
    }
}

fn run_loop(
    opts: MqttOptions,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
    conn_state: Arc<AtomicU8>,
) {
    use std::sync::atomic::Ordering;
    let (client, mut connection) = Client::new(opts, 64);
    let mut ingest =
        crate::ingest::scalar::ScalarIngest::new(discovered, topic_map, store, record_sender);

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                conn_state.store(LIVE, Ordering::Relaxed);
                if let Err(e) = client.subscribe("#", QoS::AtMostOnce) {
                    eprintln!("mqtt: subscribe # failed: {e}");
                }
            }
            Ok(Event::Incoming(Packet::Publish(p))) => {
                let payload_str = std::str::from_utf8(&p.payload)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| format!("({} bytes)", p.payload.len()));
                let ts = crate::types::now_ns();
                ingest.on_message(&p.topic, payload_str.as_str(), ts);
            }
            Err(e) => {
                conn_state.store(CONNECTING, Ordering::Relaxed);
                eprintln!("mqtt: {e}");
                std::thread::sleep(Duration::from_secs(2));
            }
            _ => {}
        }
    }
}

/// Parse "host:port" or "host" (default port 1883).
/// Handles IPv6 bracket notation: "[::1]:1883".
fn parse_broker_url(url: &str) -> (String, u16) {
    if url.starts_with('[') {
        if let Some(bracket_end) = url.find(']') {
            let host = url[1..bracket_end].to_string();
            let rest = &url[bracket_end + 1..];
            if let Some(port_str) = rest.strip_prefix(':') {
                if let Ok(port) = port_str.parse::<u16>() {
                    return (host, port);
                }
            }
            return (host, 1883);
        }
    }
    if let Some(colon) = url.rfind(':') {
        if let Ok(port) = url[colon + 1..].parse::<u16>() {
            return (url[..colon].to_string(), port);
        }
    }
    (url.to_string(), 1883)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SampleType;

    #[test]
    fn parse_host_port() {
        assert_eq!(parse_broker_url("localhost:1883"), ("localhost".into(), 1883));
        assert_eq!(
            parse_broker_url("broker.example.com:8883"),
            ("broker.example.com".into(), 8883)
        );
    }

    #[test]
    fn parse_host_only_defaults_1883() {
        assert_eq!(parse_broker_url("localhost"), ("localhost".into(), 1883));
        assert_eq!(parse_broker_url("192.168.1.1"), ("192.168.1.1".into(), 1883));
    }

    #[test]
    fn parse_ipv6() {
        assert_eq!(parse_broker_url("[::1]:1883"), ("::1".into(), 1883));
        assert_eq!(parse_broker_url("[::1]"), ("::1".into(), 1883));
    }

    #[test]
    fn mqtt_source_handle_has_discovery_no_schema() {
        use crate::ingest::{DataSource, CONNECTING};
        use std::sync::atomic::Ordering;

        let registry = crate::config::ChannelRegistry::from_toml_str(
            r#"
[channels."x"]
mqtt_topic = "home/x"
type = "float"
"#,
        )
        .unwrap();
        let store: Arc<dyn ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(&registry));
        let src = MqttSource::new(
            MqttConfig { broker_url: "localhost:19997".into(), client_id: "test".into() },
            &registry,
        );
        let handle = Box::new(src).spawn(store);
        assert_eq!(handle.name, "mqtt");
        assert!(handle.discovery.is_some());
        assert!(handle.schema_bytes.is_none());
        // No broker at that port → stays CONNECTING.
        assert_eq!(handle.conn_state.load(Ordering::Relaxed), CONNECTING);
    }

    #[test]
    fn topic_map_built_from_mqtt_channels() {
        let registry = crate::config::ChannelRegistry::from_toml_str(
            r#"
[channels."sensor/temp"]
mqtt_topic = "home/sensors/temperature"
type = "float"

[channels."sensor/door"]
mqtt_topic = "home/sensors/door"
type = "bool"

[channels."zmq/only"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "int"
"#,
        )
        .unwrap();

        let src = MqttSource::new(
            MqttConfig { broker_url: "localhost:19999".into(), client_id: "test".into() },
            &registry,
        );
        let map = src.topic_map.read().unwrap();
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("home/sensors/temperature"));
        assert!(map.contains_key("home/sensors/door"));
        assert!(!map.contains_key("t"));
        assert_eq!(map["home/sensors/temperature"].1, SampleType::Float);
        assert_eq!(map["home/sensors/door"].1, SampleType::Bool);
    }
}
