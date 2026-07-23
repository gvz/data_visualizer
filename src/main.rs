use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::anyhow;

use datavis::app::DataVisApp;
use datavis::config::{ChannelRegistry, LayoutConfig};
use datavis::ingest::{IngestConfig, MqttConfig};
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

    let mqtt_handles = mqtt_endpoint.map(|broker| {
        datavis::ingest::spawn_mqtt_ingest(
            MqttConfig { broker_url: broker, client_id: "datavis".to_string() },
            &channels,
            store.clone(),
        )
    });
    let (mqtt_topics, mqtt_topic_map, mqtt_record_sender) = match mqtt_handles {
        Some(h) => (Some(h.discovered), Some(h.topic_map), Some(h.record_sender)),
        None => (None, None, None),
    };

    let (conn_state, zmq_record_sender, ingest_schema_bytes) = if demo {
        datavis::demo::spawn_demo(store.clone(), &channels);
        (None, None, vec![])
    } else {
        let config = IngestConfig {
            endpoint,
            proto_path: PathBuf::from(&schema_path),
        };
        match datavis::ingest::spawn_ingest(config, &channels, store.clone()) {
            Ok(handle) => {
                let schema_bytes = handle.schema_bytes.clone();
                (Some(handle.conn_state), Some(handle.record_sender), schema_bytes)
            }
            Err(e) => {
                eprintln!("ingest: failed to start ({e}); running without live data");
                (None, None, vec![])
            }
        }
    };

    // Recording targets every active ingest source (ZMQ and/or MQTT).
    let record_sender_slots: Vec<_> =
        [zmq_record_sender, mqtt_record_sender].into_iter().flatten().collect();

    let registry = PanelRegistry::with_builtins();
    let workspace = Workspace::from_config(&layout, &registry, &channels);
    let dyn_store: Arc<dyn ChannelStore> = store;
    let app = DataVisApp::new(
        dyn_store,
        channels,
        registry,
        workspace,
        layout_path,
        conn_state,
        record_sender_slots,
        ingest_schema_bytes,
        live_view_ns,
        live_history_s,
        mqtt_topics,
        mqtt_topic_map,
        layout.default_window_s,
    );

    eframe::run_native(
        "datavis",
        eframe::NativeOptions::default(),
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
