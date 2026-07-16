use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context};

use datavis::app::DataVisApp;
use datavis::config::{ChannelRegistry, LayoutConfig};
use datavis::store::{ChannelStore, LiveStore};
use datavis::viz::PanelRegistry;

fn main() -> anyhow::Result<()> {
    let demo = std::env::args().any(|a| a == "--demo");

    let channels = ChannelRegistry::load(Path::new("channels.toml"))?;
    let layout = LayoutConfig::load(Path::new("layout.toml"))?;

    let store = Arc::new(LiveStore::from_registry(&channels));
    if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
    }

    let panel_registry = PanelRegistry::with_builtins();
    let (screen_name, screen) = layout
        .screens
        .iter()
        .next()
        .context("layout.toml defines no screens")?;
    let panels = screen
        .panels
        .iter()
        .map(|entry| panel_registry.build(entry, &channels))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let dyn_store: Arc<dyn ChannelStore> = store;
    let app = DataVisApp::new(dyn_store, screen_name.clone(), panels);

    eframe::run_native(
        "datavis",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}
