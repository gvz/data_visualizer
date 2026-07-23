use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex, RwLock};

use crossbeam_channel::Sender;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::MqttTopicMap;
use crate::record::RecordMsg;
use crate::store::ChannelStore;
use crate::types::{ChannelId, SampleType};

/// A live data input. Constructed with its own config + the channel registry,
/// then spawned against the shared store; returns one uniform handle.
pub trait DataSource: Send {
    /// Human-facing name for UI and logs, e.g. "zmq", "mqtt".
    fn name(&self) -> &str;

    /// Consume config, spawn the worker thread(s), return the handle.
    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle;
}

/// Uniform handle for one running source. Optional capabilities are `None`
/// for sources that lack them.
pub struct SourceHandle {
    pub name: String,
    /// Connection status (`CONNECTING`/`LIVE`/`TIMEOUT` from `ingest`).
    pub conn_state: Arc<AtomicU8>,
    /// Recorder hookup: the app installs a sender here while recording.
    pub record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    /// Live topic discovery + drag-to-bind map (MQTT-shaped sources).
    pub discovery: Option<Discovery>,
    /// Static record schema for the MCAP header (ZMQ only; MQTT embeds
    /// per-frame schemas in `RecordMsg::DynamicProto`).
    pub schema_bytes: Option<Vec<u8>>,
}

/// Capability bundle for sources that discover topics at runtime.
pub struct Discovery {
    /// All received topics with their last payload, for the sidebar picker.
    pub discovered: Arc<Mutex<BTreeMap<String, String>>>,
    /// topic → (id, type); extended when a topic is dropped onto a panel.
    pub topic_map: Arc<MqttTopicMap>,
}

/// Seed a topic map from the registry's `mqtt_topic` channels. Shared by the
/// discovery-shaped sources (MQTT, WebSocket) so a dropped/known topic routes.
pub fn topic_map_from_registry(registry: &ChannelRegistry) -> Arc<MqttTopicMap> {
    let mut initial: HashMap<String, (ChannelId, SampleType)> = HashMap::new();
    for id in registry.iter_ids() {
        if let Some(mqtt_topic) = &registry.config(id).mqtt_topic {
            initial.insert(mqtt_topic.clone(), (id, registry.meta(id).sample_type));
        }
    }
    Arc::new(RwLock::new(initial))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, RwLock};

    #[test]
    fn handle_holds_optional_capabilities() {
        let discovery = Discovery {
            discovered: Arc::new(Mutex::new(BTreeMap::new())),
            topic_map: Arc::new(RwLock::new(std::collections::HashMap::new())),
        };
        let h = SourceHandle {
            name: "mqtt".to_string(),
            conn_state: Arc::new(AtomicU8::new(0)),
            record_sender: Arc::new(Mutex::new(None)),
            discovery: Some(discovery),
            schema_bytes: None,
        };
        assert_eq!(h.name, "mqtt");
        assert!(h.discovery.is_some());
        assert!(h.schema_bytes.is_none());
        assert_eq!(h.conn_state.load(Ordering::Relaxed), 0);
    }
}
