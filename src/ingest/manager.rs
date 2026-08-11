use std::collections::BTreeMap;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::MqttTopicMap;
use crate::ingest::source::{DataSource, SourceHandle};
use crate::record::RecordMsg;
use crate::store::ChannelStore;

/// Discriminant for a source; used by GUI form defaults and display logic.
///
/// `Default` yields `Zmq` so that the GUI's "add source" form opens on the
/// most-common type without an explicit selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SourceKind {
    #[default]
    Zmq,
    Mqtt,
    WebSocket,
    Bridge,
    /// A pre-spawned internal source adopted via [`SourceManager::adopt`]
    /// (e.g. the scripting engine). Not user-configurable and never persisted.
    Internal,
}

impl std::fmt::Display for SourceKind {
    /// Short human-readable label; used by the GUI's kind picker.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SourceKind::Zmq => "ZMQ",
            SourceKind::Mqtt => "MQTT",
            SourceKind::WebSocket => "WebSocket",
            SourceKind::Bridge => "Bridge",
            SourceKind::Internal => "Internal",
        };
        f.write_str(s)
    }
}

/// Full specification for one data source, used both to spawn it and (for
/// persistent sources) to reconstruct a `SourcesConfig` on save.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceSpec {
    /// ZeroMQ Protobuf source.
    Zmq { endpoint: String, schema: String },
    /// MQTT line-protocol source.
    Mqtt { broker: String, client_id: String },
    /// WebSocket line-protocol listener.
    WebSocket { listen: String },
    /// Proprietary bridge subprocess.
    Bridge(crate::ingest::bridge::BridgeConfig),
}

impl SourceSpec {
    /// Short human-readable label derived from the connection parameters.
    pub fn label(&self) -> String {
        match self {
            SourceSpec::Zmq { endpoint, .. } => format!("zmq {endpoint}"),
            SourceSpec::Mqtt { broker, .. } => format!("mqtt {broker}"),
            SourceSpec::WebSocket { listen } => format!("ws {listen}"),
            SourceSpec::Bridge(cfg) => format!("bridge {}", cfg.name),
        }
    }

    /// Which `SourceKind` discriminant this spec represents.
    pub fn kind(&self) -> SourceKind {
        match self {
            SourceSpec::Zmq { .. } => SourceKind::Zmq,
            SourceSpec::Mqtt { .. } => SourceKind::Mqtt,
            SourceSpec::WebSocket { .. } => SourceKind::WebSocket,
            SourceSpec::Bridge(_) => SourceKind::Bridge,
        }
    }
}

/// A source that has been spawned and is currently running.
pub struct LiveSource {
    /// Stable identifier assigned by `SourceManager::add`.
    pub id: u64,
    /// Kind discriminant (mirrors `spec.kind()`; cached for quick reads).
    pub kind: SourceKind,
    /// Human-readable label derived from the spec at spawn time.
    pub label: String,
    /// `true` for sources added via the config file or GUI (will be written
    /// back to `config.toml`); `false` for ephemeral CLI-only sources.
    pub persist: bool,
    /// The spec used to create this source, or `None` for adopted internal
    /// sources (the scripting engine) that have no config representation.
    spec: Option<SourceSpec>,
    handle: SourceHandle,
}

impl LiveSource {
    /// The specification that was used to create this source, if any. Internal
    /// adopted sources return `None`.
    pub fn spec(&self) -> Option<&SourceSpec> {
        self.spec.as_ref()
    }

    /// Raw connection-state value for this source (`CONNECTING`, `LIVE`, or `TIMEOUT`).
    ///
    /// Use the constants in [`crate::ingest`] to interpret the result.
    pub fn conn_state(&self) -> u8 {
        self.handle.conn_state.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Runtime registry of active data sources.
///
/// Owns a shared [`ChannelStore`] and [`ChannelRegistry`]; sources spawned via
/// [`add`](SourceManager::add) write into the store and read channel config
/// from the registry.
pub struct SourceManager {
    store: Arc<dyn ChannelStore>,
    channels: Arc<ChannelRegistry>,
    sources: Vec<LiveSource>,
    next_id: u64,
    /// Discovery maps shared by every MQTT/WebSocket source this manager
    /// spawns, so any number of discovery-capable sources present a single
    /// unified topic list and one routing table. Seeded once from the
    /// registry's `mqtt_topic` channels; survives individual source removal.
    mqtt_topic_map: Arc<MqttTopicMap>,
    mqtt_discovered: Arc<Mutex<BTreeMap<String, String>>>,
}

impl SourceManager {
    /// Create an empty manager backed by `store` and `channels`.
    pub fn new(store: Arc<dyn ChannelStore>, channels: Arc<ChannelRegistry>) -> Self {
        let mqtt_topic_map = crate::ingest::source::topic_map_from_registry(channels.as_ref());
        Self {
            store,
            channels,
            sources: Vec::new(),
            next_id: 0,
            mqtt_topic_map,
            mqtt_discovered: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Build and spawn a source from `spec`.
    ///
    /// `persist = true` marks the source as config/GUI-owned (it will be
    /// written back to `config.toml`); `persist = false` marks it as
    /// ephemeral (CLI-only).
    ///
    /// On error the source is not added and the internal list is unchanged.
    pub fn add(&mut self, spec: &SourceSpec, persist: bool) -> anyhow::Result<u64> {
        let handle = match spec {
            SourceSpec::Zmq { endpoint, schema } => {
                let src = crate::ingest::ZmqSource::build(
                    crate::ingest::IngestConfig {
                        endpoint: endpoint.clone(),
                        proto_path: std::path::PathBuf::from(schema),
                    },
                    self.channels.as_ref(),
                )?;
                Box::new(src).spawn(self.store.clone())
            }
            SourceSpec::Mqtt { broker, client_id } => {
                let mut src = crate::ingest::MqttSource::new(
                    crate::ingest::MqttConfig {
                        broker_url: broker.clone(),
                        client_id: client_id.clone(),
                    },
                    self.channels.as_ref(),
                );
                src.share(self.mqtt_topic_map.clone(), self.mqtt_discovered.clone());
                Box::new(src).spawn(self.store.clone())
            }
            SourceSpec::WebSocket { listen } => {
                // Bind eagerly so a failure (e.g. the port is already in use by
                // another source) surfaces here as an error instead of failing
                // silently in the background thread.
                let mut src = crate::ingest::WsSource::bind(
                    crate::ingest::WsConfig { listen: listen.clone() },
                    self.channels.as_ref(),
                )?;
                src.share(self.mqtt_topic_map.clone(), self.mqtt_discovered.clone());
                Box::new(src).spawn(self.store.clone())
            }
            SourceSpec::Bridge(cfg) => {
                let src =
                    crate::ingest::bridge::SubprocessSource::new(cfg.clone(), self.channels.as_ref());
                Box::new(src).spawn(self.store.clone())
            }
        };

        let id = self.next_id;
        self.next_id += 1;
        self.sources.push(LiveSource {
            id,
            kind: spec.kind(),
            label: spec.label(),
            persist,
            spec: Some(spec.clone()),
            handle,
        });
        Ok(id)
    }

    /// Adopt an already-spawned [`SourceHandle`] that has no [`SourceSpec`] —
    /// e.g. the scripting engine, which is a `DataSource` but not a
    /// user-configurable one. It participates in the derived views (its
    /// `record_sender`/`conn_state` join the aggregate) but is marked
    /// `Internal`, never persisted, and not shown as a configurable row.
    pub fn adopt(&mut self, handle: SourceHandle, label: String) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.sources.push(LiveSource {
            id,
            kind: SourceKind::Internal,
            label,
            persist: false,
            spec: None,
            handle,
        });
        id
    }

    /// Stop and remove the source with the given `id`.
    ///
    /// Sets the source's stop flag, then drops the handle. For bridge sources
    /// the `ChildGuard` fires on drop, killing the child process. Unknown ids
    /// are silently ignored.
    pub fn remove(&mut self, id: u64) {
        if let Some(pos) = self.sources.iter().position(|s| s.id == id) {
            let s = self.sources.remove(pos);
            s.handle.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            // handle (and child_guard) drop here.
        }
    }

    /// Connection-state atoms from every running source, in insertion order.
    pub fn conn_states(&self) -> Vec<Arc<AtomicU8>> {
        self.sources.iter().map(|s| s.handle.conn_state.clone()).collect()
    }

    /// Recording-hook sender slots from every running source, in insertion order.
    pub fn record_sender_slots(&self) -> Vec<Arc<Mutex<Option<Sender<RecordMsg>>>>> {
        self.sources.iter().map(|s| s.handle.record_sender.clone()).collect()
    }

    /// Serialised Protobuf schema bytes from the first source that provides one.
    ///
    /// Returns an empty `Vec` if no source has schema bytes.
    pub fn schema_bytes(&self) -> Vec<u8> {
        for s in &self.sources {
            if let Some(bytes) = &s.handle.schema_bytes {
                if !bytes.is_empty() {
                    return bytes.clone();
                }
            }
        }
        Vec::new()
    }

    /// The shared topic-discovery maps, if any discovery-capable source
    /// (MQTT / WebSocket) is currently running.
    ///
    /// Every MQTT/WebSocket source spawned by this manager shares one
    /// `discovered` list and one routing `topic_map`, so the returned pair
    /// reflects ALL such sources at once — a topic seen by any source appears
    /// here, and a drag-to-bind written here routes on whichever source
    /// actually receives that topic. Returns `None` when no discovery-capable
    /// source is active.
    pub fn discovery(&self) -> Option<(Arc<Mutex<BTreeMap<String, String>>>, Arc<MqttTopicMap>)> {
        let has_discovery = self
            .sources
            .iter()
            .any(|s| matches!(s.kind, SourceKind::Mqtt | SourceKind::WebSocket));
        has_discovery.then(|| (self.mqtt_discovered.clone(), self.mqtt_topic_map.clone()))
    }

    /// All currently-running sources, in insertion order.
    pub fn sources(&self) -> &[LiveSource] {
        &self.sources
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mgr() -> SourceManager {
        let registry = std::sync::Arc::new(
            crate::config::ChannelRegistry::from_toml_str("").unwrap());
        let store: Arc<dyn ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(&registry));
        SourceManager::new(store, registry)
    }

    #[test]
    fn add_grows_derived_views_remove_shrinks_and_stops() {
        let mut m = mgr();
        // WebSocket binds ephemeral; no external dependency.
        let id = m.add(&SourceSpec::WebSocket { listen: "127.0.0.1:0".into() }, true).unwrap();
        assert_eq!(m.conn_states().len(), 1);
        assert_eq!(m.record_sender_slots().len(), 1);
        assert!(m.discovery().is_some());
        assert!(m.sources()[0].persist);
        m.remove(id);
        assert_eq!(m.conn_states().len(), 0);
        assert!(m.discovery().is_none());
    }

    #[test]
    fn add_zmq_missing_schema_errors_and_adds_nothing() {
        let mut m = mgr();
        let r = m.add(&SourceSpec::Zmq {
            endpoint: "tcp://localhost:5599".into(),
            schema: "/nonexistent.proto".into() }, true);
        assert!(r.is_err());
        assert_eq!(m.sources().len(), 0);
    }

    #[test]
    fn remove_unknown_id_is_noop() {
        let mut m = mgr();
        m.remove(999);
        assert_eq!(m.sources().len(), 0);
    }

    #[test]
    fn websocket_bind_failure_errors_and_adds_nothing() {
        // Occupy a port for the duration of the test, then try to add a
        // WebSocket source on the same address: the eager bind must fail so
        // `add` returns Err and no zombie source is left behind.
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap().to_string();
        let mut m = mgr();
        let r = m.add(&SourceSpec::WebSocket { listen: addr }, true);
        assert!(r.is_err(), "binding an already-occupied port must error");
        assert_eq!(m.sources().len(), 0, "a source that failed to bind must not be added");
    }

    #[test]
    fn multiple_discovery_sources_share_one_topic_map() {
        use crate::types::{ChannelId, SampleType};
        let mut m = mgr();
        // Two WebSocket sources (ephemeral ports, no external dependency).
        let id1 = m.add(&SourceSpec::WebSocket { listen: "127.0.0.1:0".into() }, true).unwrap();
        let _id2 = m.add(&SourceSpec::WebSocket { listen: "127.0.0.1:0".into() }, true).unwrap();

        // A bind written through the shared discovery map...
        let (_disc, map) = m.discovery().expect("discovery active with WS sources");
        map.write().unwrap().insert("topic/x".into(), (ChannelId(0), SampleType::Float));

        // ...survives removal of the FIRST source, proving the map is shared by
        // the manager rather than owned by a single source.
        m.remove(id1);
        let (_disc2, map2) = m.discovery().expect("second WS source still provides discovery");
        assert!(map2.read().unwrap().contains_key("topic/x"));

        // Once every discovery-capable source is gone, discovery() is None.
        let remaining: Vec<u64> = m.sources().iter().map(|s| s.id).collect();
        for id in remaining {
            m.remove(id);
        }
        assert!(m.discovery().is_none());
    }
}
