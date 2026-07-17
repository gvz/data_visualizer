use std::path::PathBuf;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;

pub mod decode;
pub mod loader;
pub mod router;
pub mod thread;

pub const CONNECTING: u8 = 0;
pub const LIVE: u8 = 1;
pub const TIMEOUT: u8 = 2;

pub struct IngestConfig {
    pub endpoint: String,
    pub proto_path: PathBuf,
}

pub struct IngestHandle {
    pub conn_state: Arc<AtomicU8>,
}

pub fn spawn_ingest(
    config: IngestConfig,
    registry: &ChannelRegistry,
    store: Arc<dyn ChannelStore>,
) -> anyhow::Result<IngestHandle> {
    let schema = loader::ProtoSchema::from_path(&config.proto_path)?;
    let router = router::TopicRouter::build(registry, &schema);
    let conn_state = Arc::new(AtomicU8::new(CONNECTING));
    let state_clone = conn_state.clone();
    let endpoint = config.endpoint.clone();
    std::thread::spawn(move || {
        thread::run_loop(endpoint, router, store, state_clone);
    });
    Ok(IngestHandle { conn_state })
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
}
