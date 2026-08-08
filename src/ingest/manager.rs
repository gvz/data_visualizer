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
    spec: SourceSpec,
    handle: SourceHandle,
}

impl LiveSource {
    /// The specification that was used to create this source.
    pub fn spec(&self) -> &SourceSpec {
        &self.spec
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
}

impl SourceManager {
    /// Create an empty manager backed by `store` and `channels`.
    pub fn new(store: Arc<dyn ChannelStore>, channels: Arc<ChannelRegistry>) -> Self {
        Self { store, channels, sources: Vec::new(), next_id: 0 }
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
                let src = crate::ingest::MqttSource::new(
                    crate::ingest::MqttConfig {
                        broker_url: broker.clone(),
                        client_id: client_id.clone(),
                    },
                    self.channels.as_ref(),
                );
                Box::new(src).spawn(self.store.clone())
            }
            SourceSpec::WebSocket { listen } => {
                let src = crate::ingest::WsSource::new(
                    crate::ingest::WsConfig { listen: listen.clone() },
                    self.channels.as_ref(),
                );
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
            spec: spec.clone(),
            handle,
        });
        Ok(id)
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

    /// Topic-discovery maps from the first source that supports runtime
    /// discovery (MQTT / WebSocket).
    ///
    /// Returns `None` if no source provides discovery.
    pub fn discovery(&self) -> Option<(Arc<Mutex<BTreeMap<String, String>>>, Arc<MqttTopicMap>)> {
        for s in &self.sources {
            if let Some(d) = &s.handle.discovery {
                return Some((d.discovered.clone(), d.topic_map.clone()));
            }
        }
        None
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
}
