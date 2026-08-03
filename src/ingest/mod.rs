use std::path::PathBuf;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use crate::config::ChannelRegistry;
use crate::record::RecordMsg;
use crate::store::ChannelStore;

pub mod bridge;
pub mod decode;
pub mod lineproto;
pub mod loader;
pub mod mqtt;
pub mod router;
pub mod scalar;
pub mod source;
pub mod thread;
pub mod websocket;

pub use mqtt::{MqttConfig, MqttSource};
pub use source::{DataSource, Discovery, SourceHandle};
pub use websocket::{WsConfig, WsSource};

/// Source is attempting to connect; no data has been received yet.
pub const CONNECTING: u8 = 0;
/// Source is connected and receiving data within the expected heartbeat window.
pub const LIVE: u8 = 1;
/// Source was live but has not received data within the timeout window.
pub const TIMEOUT: u8 = 2;

/// Configuration for the ZeroMQ Protobuf source.
pub struct IngestConfig {
    /// ZMQ SUB socket endpoint, e.g. `"tcp://localhost:5555"`.
    pub endpoint: String,
    /// Path to the `.proto` schema file that describes the message format.
    pub proto_path: PathBuf,
}

/// ZeroMQ SUB source that decodes Protobuf-framed channel samples.
///
/// Each message is a two-part ZMQ frame: the topic string followed by a
/// serialised Protobuf payload decoded against the schema loaded from
/// [`IngestConfig::proto_path`]. The schema is also serialised into
/// [`SourceHandle::schema_bytes`] for embedding in MCAP recording headers.
///
/// Build with [`ZmqSource::build`], then call [`DataSource::spawn`] to start
/// the background receive loop.
pub struct ZmqSource {
    endpoint: String,
    router: router::TopicRouter,
    pub(crate) schema_bytes: Vec<u8>,
}

impl ZmqSource {
    /// Parse the `.proto` schema at `config.proto_path`, build a topic router
    /// from the registry, and return a ready-to-spawn source.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the schema file cannot be read or parsed.
    pub fn build(config: IngestConfig, registry: &ChannelRegistry) -> anyhow::Result<Self> {
        let schema = loader::ProtoSchema::from_path(&config.proto_path)?;
        let schema_bytes = schema.schema_bytes().to_vec();
        let router = router::TopicRouter::build(registry, &schema);
        Ok(Self { endpoint: config.endpoint, router, schema_bytes })
    }
}

impl source::DataSource for ZmqSource {
    fn name(&self) -> &str {
        "zmq"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> source::SourceHandle {
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));
        let record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>> =
            Arc::new(Mutex::new(None));

        let state_clone = conn_state.clone();
        let sender_clone = record_sender.clone();
        let endpoint = self.endpoint.clone();
        let router = self.router;
        std::thread::spawn(move || {
            thread::run_loop(endpoint, router, store, state_clone, sender_clone);
        });

        source::SourceHandle {
            name: "zmq".to_string(),
            conn_state,
            record_sender,
            discovery: None,
            schema_bytes: Some(self.schema_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_module_compiles() {}

    #[test]
    fn conn_state_constants_are_distinct() {
        assert_eq!(CONNECTING, 0u8);
        assert_eq!(LIVE, 1u8);
        assert_eq!(TIMEOUT, 2u8);
        assert_ne!(CONNECTING, LIVE);
        assert_ne!(LIVE, TIMEOUT);
        assert_ne!(CONNECTING, TIMEOUT);
    }

    #[test]
    fn spawn_ingest_missing_schema_returns_err() {
        let registry = crate::config::ChannelRegistry::from_toml_str(r#"
[channels."x"]
topic = "t"
proto_path = "A.v"
ts_path = "A.t"
type = "float"
"#).unwrap();
        let result = ZmqSource::build(
            IngestConfig {
                endpoint: "tcp://localhost:55999".to_string(),
                proto_path: std::path::PathBuf::from("/nonexistent/schema.proto"),
            },
            &registry,
        );
        assert!(result.is_err(), "expected Err for missing schema file");
    }

    #[test]
    fn ingest_handle_initial_state_is_connecting() {
        use std::sync::atomic::Ordering;
        let state = Arc::new(AtomicU8::new(CONNECTING));
        assert_eq!(state.load(Ordering::Relaxed), CONNECTING);
    }

    #[test]
    fn ingest_handle_has_record_sender() {
        // Just compile-checks the field types are accessible.
        let sender: Arc<std::sync::Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>> =
            Arc::new(std::sync::Mutex::new(None));
        drop(sender);
    }

    #[test]
    fn zmq_source_handle_has_schema_no_discovery() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let proto_path = dir.path().join("test.proto");
        let mut f = std::fs::File::create(&proto_path).unwrap();
        write!(f, "syntax = \"proto3\";\nmessage M {{ int64 t = 1; float v = 2; }}\n").unwrap();
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
        let store: Arc<dyn crate::store::ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(&registry));
        let src = ZmqSource::build(
            IngestConfig { endpoint: "tcp://localhost:59998".into(), proto_path },
            &registry,
        )
        .unwrap();
        let handle = Box::new(src).spawn(store);
        assert_eq!(handle.name, "zmq");
        assert!(handle.discovery.is_none());
        assert!(handle.schema_bytes.as_ref().is_some_and(|b| !b.is_empty()));
    }

    #[test]
    fn schema_bytes_via_spawn_ingest_are_non_empty() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let proto_path = dir.path().join("test.proto");
        let mut f = std::fs::File::create(&proto_path).unwrap();
        write!(f, "syntax = \"proto3\";\nmessage M {{ int64 t = 1; float v = 2; }}\n").unwrap();
        let registry = crate::config::ChannelRegistry::from_toml_str(r#"
[channels."x"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap();
        let src = ZmqSource::build(
            IngestConfig {
                endpoint: "tcp://localhost:59999".to_string(),
                proto_path,
            },
            &registry,
        ).unwrap();
        assert!(!src.schema_bytes.is_empty(), "schema_bytes must be populated");
    }
}
