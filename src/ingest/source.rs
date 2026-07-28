use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex, RwLock};

use crossbeam_channel::Sender;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::MqttTopicMap;
use crate::record::RecordMsg;
use crate::store::ChannelStore;
use crate::types::{ChannelId, SampleType};

/// A live data input that can be spawned against the shared channel store.
///
/// Implementations construct themselves from source-specific config and a
/// [`ChannelRegistry`], then call [`spawn`](DataSource::spawn) to hand off to
/// a background thread and receive a uniform [`SourceHandle`] in return.
///
/// # Implementing
///
/// Write source-specific config into a companion struct (e.g. [`MqttConfig`],
/// [`WsConfig`], [`IngestConfig`]). In `spawn`:
/// 1. Allocate a shared `conn_state` (`Arc<AtomicU8>`, starting at
///    [`CONNECTING`](crate::ingest::CONNECTING)).
/// 2. Allocate a shared `record_sender` (`Arc<Mutex<Option<Sender<RecordMsg>>>>`).
/// 3. Spawn your background thread, cloning both `Arc`s into it.
/// 4. Return a [`SourceHandle`] that wraps all shared state, plus optional
///    [`Discovery`] if the source discovers topics at runtime.
///
/// [`MqttConfig`]: crate::ingest::MqttConfig
/// [`WsConfig`]: crate::ingest::WsConfig
/// [`IngestConfig`]: crate::ingest::IngestConfig
pub trait DataSource: Send {
    /// Human-facing name shown in the UI status bar and log output, e.g.
    /// `"zmq"`, `"mqtt"`, `"websocket"`.
    fn name(&self) -> &str;

    /// Consume the source, spawn its background worker thread(s), and return a
    /// [`SourceHandle`] the app uses to observe status and hook up recording.
    ///
    /// Takes `Box<Self>` so the trait is object-safe and construction can be
    /// split from spawning (build with config, bind to a store later).
    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle;
}

/// Uniform handle for a running data source.
///
/// All fields are `Arc`-backed so the app can share them with the background
/// thread without lifetimes. Optional capabilities are `None` for sources that
/// do not provide them.
pub struct SourceHandle {
    /// Human-facing source name, matches [`DataSource::name`].
    pub name: String,
    /// Connection state, written atomically by the background thread.
    ///
    /// Values are the module-level constants:
    /// [`CONNECTING`](crate::ingest::CONNECTING) (0),
    /// [`LIVE`](crate::ingest::LIVE) (1),
    /// [`TIMEOUT`](crate::ingest::TIMEOUT) (2).
    /// The app reads this each frame to drive the status indicator.
    pub conn_state: Arc<AtomicU8>,
    /// Recording hook: the app installs a sender while recording is active and
    /// removes it when recording stops. The background thread checks each frame
    /// and forwards raw messages when set.
    pub record_sender: Arc<Mutex<Option<Sender<RecordMsg>>>>,
    /// Live topic discovery and drag-to-bind channel map.
    ///
    /// `Some` for sources that discover topics at runtime (MQTT, WebSocket).
    /// `None` for sources whose channels are fully known at startup (ZMQ).
    pub discovery: Option<Discovery>,
    /// Serialised Protobuf schema for the MCAP recording header.
    ///
    /// `Some` for ZMQ, which has a static `.proto` schema loaded at startup.
    /// `None` for MQTT/WebSocket, which embed per-message schemas in
    /// [`RecordMsg::DynamicProto`](crate::record::RecordMsg).
    pub schema_bytes: Option<Vec<u8>>,
}

/// Runtime topic discovery capability, present on MQTT-shaped sources.
///
/// Both [`MqttSource`](crate::ingest::MqttSource) and
/// [`WsSource`](crate::ingest::WsSource) discover channel topics at runtime
/// rather than requiring them all to be declared in `config.toml` upfront.
/// This struct exposes the two shared maps the UI needs to support that.
pub struct Discovery {
    /// Every topic seen since startup, mapped to its most recent payload
    /// (formatted as a string). Written by the background thread; read by the
    /// sidebar channel picker to populate the discovery list.
    pub discovered: Arc<Mutex<BTreeMap<String, String>>>,
    /// Maps `topic → (ChannelId, SampleType)`. Pre-seeded from `config.toml`
    /// `mqtt_topic` entries; extended at runtime when a topic is dragged from
    /// the sidebar onto a panel, dynamically registering a new channel.
    pub topic_map: Arc<MqttTopicMap>,
}

/// Build a topic map pre-seeded from the registry's `mqtt_topic` channel
/// declarations.
///
/// Both MQTT and WebSocket sources call this at construction time so that
/// channels already declared in `config.toml` with an `mqtt_topic` field are
/// immediately routable without requiring a runtime drop.
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
