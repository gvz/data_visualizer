use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rumqttc::{Client, Event, MqttOptions, Packet, QoS};

use crate::config::ChannelRegistry;
use crate::dynamic_channel::MqttTopicMap;
use crate::record::RecordMsg;
use crate::store::ChannelStore;
use crate::types::{ChannelId, SampleType};

/// Discovered-topic snapshot and the shared topic→channel routing table.
pub struct MqttHandles {
    /// All received topics with their last payload, for the sidebar picker.
    pub discovered: Arc<Mutex<BTreeMap<String, String>>>,
    /// topic → (id, type); extended when a topic is dropped onto a panel.
    pub topic_map: Arc<MqttTopicMap>,
    /// Installed by the app while recording so the ingest thread queues frames.
    pub record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
}

pub struct MqttConfig {
    /// Broker address as "host:port" or "host" (defaults to port 1883).
    pub broker_url: String,
    pub client_id: String,
}

/// Spawn an MQTT ingest thread. Subscribes to `#` for discovery.
/// Messages on channels with `mqtt_topic` configured are written to the store.
/// All received topic names are added to the returned discovered set.
pub fn spawn_mqtt_ingest(
    config: MqttConfig,
    registry: &ChannelRegistry,
    store: Arc<dyn ChannelStore>,
) -> MqttHandles {
    let discovered: Arc<Mutex<BTreeMap<String, String>>> = Arc::new(Mutex::new(BTreeMap::new()));

    let mut initial: HashMap<String, (ChannelId, SampleType)> = HashMap::new();
    for id in registry.iter_ids() {
        let cfg = registry.config(id);
        if let Some(mqtt_topic) = &cfg.mqtt_topic {
            initial.insert(mqtt_topic.clone(), (id, registry.meta(id).sample_type));
        }
    }
    let topic_map: Arc<MqttTopicMap> = Arc::new(RwLock::new(initial));

    let (host, port) = parse_broker_url(&config.broker_url);
    let mut opts = MqttOptions::new(config.client_id, host, port);
    opts.set_keep_alive(Duration::from_secs(30));

    let record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>> =
        Arc::new(Mutex::new(None));

    let disc = discovered.clone();
    let map = topic_map.clone();
    let rec = record_sender.clone();
    std::thread::spawn(move || {
        run_loop(opts, map, disc, store, rec);
    });

    MqttHandles { discovered, topic_map, record_sender }
}

fn run_loop(
    opts: MqttOptions,
    topic_map: Arc<MqttTopicMap>,
    discovered: Arc<Mutex<BTreeMap<String, String>>>,
    store: Arc<dyn ChannelStore>,
    record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
) {
    let (client, mut connection) = Client::new(opts, 64);
    let mut ingest =
        crate::ingest::scalar::ScalarIngest::new(discovered, topic_map, store, record_sender);

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
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
    fn spawn_returns_discovered_set() {
        let registry = crate::config::ChannelRegistry::from_toml_str(
            r#"
[channels."x"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#,
        )
        .unwrap();
        let store: Arc<dyn ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(&registry));
        let handles = spawn_mqtt_ingest(
            MqttConfig { broker_url: "localhost:19998".into(), client_id: "test".into() },
            &registry,
            store,
        );
        // Starts empty; will stay empty since test broker won't connect.
        assert!(handles.discovered.lock().unwrap().is_empty());
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

        let mut map: HashMap<String, (ChannelId, SampleType)> = HashMap::new();
        for id in registry.iter_ids() {
            let cfg = registry.config(id);
            if let Some(t) = &cfg.mqtt_topic {
                map.insert(t.clone(), (id, registry.meta(id).sample_type));
            }
        }
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("home/sensors/temperature"));
        assert!(map.contains_key("home/sensors/door"));
        assert!(!map.contains_key("t"));
        assert_eq!(map["home/sensors/temperature"].1, SampleType::Float);
        assert_eq!(map["home/sensors/door"].1, SampleType::Bool);
    }
}
