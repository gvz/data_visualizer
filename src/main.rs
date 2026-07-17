use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::anyhow;

use datavis::app::DataVisApp;
use datavis::config::{ChannelRegistry, LayoutConfig};
use datavis::ingest::IngestConfig;
use datavis::store::{ChannelStore, LiveStore};
use datavis::viz::PanelRegistry;
use datavis::workspace::Workspace;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let demo = args.iter().any(|a| a == "--demo");
    let endpoint =
        arg_value(&args, "--endpoint").unwrap_or_else(|| "tcp://localhost:5555".to_string());
    let schema_path =
        arg_value(&args, "--schema").unwrap_or_else(|| "schema.proto".to_string());
    let layout_path = PathBuf::from("layout.toml");

    let channels = ChannelRegistry::load(Path::new("channels.toml"))?;
    let layout = LayoutConfig::load(&layout_path)?;

    let store = Arc::new(LiveStore::from_registry(&channels));

    let conn_state = if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
        None
    } else {
        let config = IngestConfig {
            endpoint,
            proto_path: PathBuf::from(&schema_path),
        };
        match datavis::ingest::spawn_ingest(config, &channels, store.clone()) {
            Ok(handle) => Some(handle.conn_state),
            Err(e) => {
                eprintln!("ingest: failed to start ({e}); running without live data");
                None
            }
        }
    };

    let registry = PanelRegistry::with_builtins();
    let workspace = Workspace::from_config(&layout, &registry, &channels);
    let dyn_store: Arc<dyn ChannelStore> = store;
    let app = DataVisApp::new(dyn_store, channels, registry, workspace, layout_path, conn_state);

    eframe::run_native(
        "datavis",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}
