use std::path::PathBuf;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use anyhow::anyhow;

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
    _config: IngestConfig,
    _registry: &ChannelRegistry,
    _store: Arc<dyn ChannelStore>,
) -> anyhow::Result<IngestHandle> {
    Err(anyhow!("not yet implemented"))
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
}
