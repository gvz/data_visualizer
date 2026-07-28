use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context};

use datavis::app::DataVisApp;
use datavis::config::{ChannelRegistry, LayoutConfig, DEFAULT_CONFIG_TOML};
use datavis::ingest::{
    DataSource, IngestConfig, MqttConfig, MqttSource, WsConfig, WsSource, ZmqSource,
};
use datavis::store::{ChannelStore, LiveStore};
use datavis::viz::PanelRegistry;
use datavis::workspace::Workspace;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    let demo = args.iter().any(|a| a == "--demo");
    let demo_freq = arg_value(&args, "--demo-freq")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1.0);
    let endpoint =
        arg_value(&args, "--endpoint").unwrap_or_else(|| "tcp://localhost:5555".to_string());
    let schema_path =
        arg_value(&args, "--schema").unwrap_or_else(|| "schema.proto".to_string());
    let mqtt_endpoint = arg_value(&args, "--mqtt-endpoint");
    let mut layout_path = PathBuf::from("config.toml");

    let (channels, layout) = if layout_path.exists() {
        (ChannelRegistry::load(&layout_path)?, LayoutConfig::load(&layout_path)?)
    } else {
        // No config in the working directory: fall back to the hardcoded
        // default, but first ask whether to save it, load a different file,
        // or just run with the defaults this once.
        resolve_missing_config(&mut layout_path)?
    };

    let store = Arc::new(LiveStore::from_registry(&channels));
    let live_view_ns = store.view_override.clone();
    let live_history_s = channels
        .iter_ids()
        .map(|id| channels.meta(id).history_s)
        .fold(5.0_f64, f64::max);

    // The status indicator aggregates every source's conn_state (LIVE if any
    // source is live), so push order does not affect it. Schema/discovery are
    // each provided by a single source, so order is irrelevant there too.
    let mut sources: Vec<datavis::ingest::SourceHandle> = Vec::new();

    if demo {
        datavis::demo::spawn_demo(store.clone(), &channels, demo_freq);
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

    if let Some(broker) = mqtt_endpoint {
        let src = MqttSource::new(
            MqttConfig { broker_url: broker, client_id: "datavis".to_string() },
            &channels,
        );
        sources.push(Box::new(src).spawn(store.clone()));
    }

    if let Some(listen) = arg_value(&args, "--ws-listen") {
        let src = WsSource::new(WsConfig { listen }, &channels);
        sources.push(Box::new(src).spawn(store.clone()));
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

/// Load both halves of a config file (channels + layout) from one path.
fn load_config(path: &Path) -> anyhow::Result<(ChannelRegistry, LayoutConfig)> {
    Ok((ChannelRegistry::load(path)?, LayoutConfig::load(path)?))
}

/// The hardcoded default config, parsed straight from the embedded template.
fn default_config() -> anyhow::Result<(ChannelRegistry, LayoutConfig)> {
    Ok((
        ChannelRegistry::from_toml_str(DEFAULT_CONFIG_TOML)?,
        LayoutConfig::from_toml_str(DEFAULT_CONFIG_TOML)?,
    ))
}

/// No `config.toml` in the working directory. Ask the user whether to save the
/// built-in default there, load a different file, or run with the defaults
/// without saving. On save or load, `layout_path` is pointed at the file the
/// app should persist layout changes back into.
fn resolve_missing_config(
    layout_path: &mut PathBuf,
) -> anyhow::Result<(ChannelRegistry, LayoutConfig)> {
    use rfd::{FileDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let choice = MessageDialog::new()
        .set_level(MessageLevel::Info)
        .set_title("datavis — no config found")
        .set_description(format!(
            "No config.toml was found in {cwd}.\n\n\
             Yes\u{2003}— save the built-in default here and use it\n\
             No\u{2003}— load a different config file\n\
             Cancel\u{2003}— start with default settings (nothing saved)"
        ))
        .set_buttons(MessageButtons::YesNoCancel)
        .show();

    match choice {
        MessageDialogResult::Yes => {
            std::fs::write(&*layout_path, DEFAULT_CONFIG_TOML)
                .with_context(|| format!("writing {}", layout_path.display()))?;
            load_config(layout_path)
        }
        MessageDialogResult::No => match FileDialog::new()
            .add_filter("TOML config", &["toml"])
            .set_title("Load config")
            .pick_file()
        {
            Some(picked) => {
                let cfg = load_config(&picked)?;
                *layout_path = picked;
                Ok(cfg)
            }
            // Picker dismissed — fall back to in-memory defaults.
            None => default_config(),
        },
        // Cancel or window closed.
        _ => default_config(),
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

fn print_help() {
    println!(
        "datavis - real-time channel data visualizer

USAGE:
    datavis [OPTIONS]

OPTIONS:
    --demo                    Run with the built-in demo source (no live inputs).
    --demo-freq <HZ>          Sine frequency for the demo source. [default: 1.0]
    --endpoint <ADDR>         ZMQ SUB endpoint for live proto data.
                              [default: tcp://localhost:5555]
    --schema <PATH>           Proto schema file for the ZMQ source.
                              [default: schema.proto]
    --mqtt-endpoint <ADDR>    MQTT broker as \"host:port\" (or \"host\", port 1883).
                              Enables the MQTT source; off when omitted.
    --ws-listen <ADDR>        Bind a WebSocket server (\"host:port\") that receives
                              InfluxDB line protocol. Off when omitted.
    -h, --help                Print this help and exit.

Channels and layout share config.toml; the layout section persists there.
The status indicator reads LIVE if any configured source is receiving data."
    );
}
