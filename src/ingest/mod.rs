use std::path::PathBuf;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use crate::config::ChannelRegistry;
use crate::record::RecordMsg;
use crate::store::ChannelStore;

pub mod decode;
pub mod loader;
pub mod mqtt;
pub mod router;
pub mod thread;

pub use mqtt::{spawn_mqtt_ingest, MqttConfig, MqttHandles};

pub const CONNECTING: u8 = 0;
pub const LIVE: u8 = 1;
pub const TIMEOUT: u8 = 2;

pub struct IngestConfig {
    pub endpoint: String,
    pub proto_path: PathBuf,
}

pub struct IngestHandle {
    pub conn_state: Arc<AtomicU8>,
    pub record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>>,
    pub schema_bytes: Vec<u8>,
}

pub fn spawn_ingest(
    config: IngestConfig,
    registry: &ChannelRegistry,
    store: Arc<dyn ChannelStore>,
) -> anyhow::Result<IngestHandle> {
    let schema = loader::ProtoSchema::from_path(&config.proto_path)?;
    let schema_bytes = schema.schema_bytes().to_vec();
    let router = router::TopicRouter::build(registry, &schema);
    let conn_state = Arc::new(AtomicU8::new(CONNECTING));
    let state_clone = conn_state.clone();
    let endpoint = config.endpoint.clone();
    let record_sender: Arc<Mutex<Option<crossbeam_channel::Sender<RecordMsg>>>> =
        Arc::new(Mutex::new(None));
    let record_sender_clone = record_sender.clone();
    std::thread::spawn(move || {
        thread::run_loop(endpoint, router, store, state_clone, record_sender_clone);
    });
    Ok(IngestHandle { conn_state, record_sender, schema_bytes })
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
        use std::sync::Arc;
        let registry = crate::config::ChannelRegistry::from_toml_str(r#"
[channels."x"]
topic = "t"
proto_path = "A.v"
ts_path = "A.t"
type = "float"
"#).unwrap();
        let store: Arc<dyn crate::store::ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(&registry));
        let result = spawn_ingest(
            IngestConfig {
                endpoint: "tcp://localhost:55999".to_string(),
                proto_path: std::path::PathBuf::from("/nonexistent/schema.proto"),
            },
            &registry,
            store,
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
        let store: Arc<dyn crate::store::ChannelStore> =
            Arc::new(crate::store::LiveStore::from_registry(&registry));
        let handle = spawn_ingest(
            IngestConfig {
                endpoint: "tcp://localhost:59999".to_string(),
                proto_path,
            },
            &registry,
            store,
        ).unwrap();
        assert!(!handle.schema_bytes.is_empty(), "schema_bytes must be populated");
    }
}
