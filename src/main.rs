use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::anyhow;

use datavis::app::DataVisApp;
use datavis::config::{ChannelRegistry, LayoutConfig};
use datavis::ingest::{DataSource, IngestConfig, MqttConfig, MqttSource, ZmqSource};
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
    let mqtt_endpoint = arg_value(&args, "--mqtt-endpoint");
    let layout_path = PathBuf::from("layout.toml");

    let channels = ChannelRegistry::load(Path::new("channels.toml"))?;
    let layout = LayoutConfig::load(&layout_path)?;

    let store = Arc::new(LiveStore::from_registry(&channels));
    let live_view_ns = store.view_override.clone();
    let live_history_s = channels
        .iter_ids()
        .map(|id| channels.meta(id).history_s)
        .fold(5.0_f64, f64::max);

    let mut sources: Vec<datavis::ingest::SourceHandle> = Vec::new();

    if let Some(broker) = mqtt_endpoint {
        let src = MqttSource::new(
            MqttConfig { broker_url: broker, client_id: "datavis".to_string() },
            &channels,
        );
        sources.push(Box::new(src).spawn(store.clone()));
    }

    if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
    } else {
        let config = IngestConfig {
            endpoint,
            proto_path: PathBuf::from(&schema_path),
        };
        match ZmqSource::build(config, &channels) {
            Ok(src) => sources.push(Box::new(src).spawn(store.clone())),
            Err(e) => eprintln!("ingest: failed to start ({e}); running without live data"),
        }
    }

    let registry = PanelRegistry::with_builtins();
    let workspace = Workspace::from_config(&layout, &registry, &channels);
    let dyn_store: Arc<dyn ChannelStore> = store;
    let app = DataVisApp::new(
        dyn_store,
        channels,
        registry,
        workspace,
        layout_path,
        sources,
        live_view_ns,
        live_history_s,
        layout.default_window_s,
    );

    eframe::run_native(
        "datavis",
        eframe::NativeOptions { vsync: false, ..Default::default() },
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(eframe::egui::Visuals::light());
            let mut fonts = eframe::egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("eframe: {e}"))
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}
