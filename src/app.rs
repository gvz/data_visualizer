use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_phosphor::regular as icon;

use crate::channel_tree::ChannelTree;
use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
use crate::record::playback::PlaybackStore;
use crate::record::{start_recording, RecordHandle};
use crate::store::ChannelStore;
use crate::viz::PanelRegistry;
use crate::workspace::Workspace;
use crate::script::config::ScriptInstance;
use crate::script::{ScriptCommand, SharedMetas, SharedStatus};

/// State for the "Add panel" modal.
#[derive(Default)]
struct AddPanelDialog {
    open: bool,
    panel_type: String,
    /// Channel names in click order (order matters for xy_scatter: x then y).
    selected: Vec<String>,
}

/// Channel-count rules per panel type. None = invalid selection for that type.
/// Compact one-line rendering of a channel's latest value for the sidebar tree.
fn fmt_sample(sample: &crate::types::Sample) -> String {
    use crate::types::Sample;
    match sample {
        Sample::Float(v) => format!("{v:.3}"),
        Sample::Int(v) => v.to_string(),
        Sample::Bool(b) => if *b { "ON" } else { "OFF" }.to_string(),
        Sample::Text(s) => s.clone(),
    }
}

pub fn build_panel_entry(panel_type: &str, selected: &[String]) -> Option<PanelEntry> {
    let mut cfg = toml::Table::new();
    match panel_type {
        "waveform" | "log" => {
            if selected.is_empty() {
                return None;
            }
            cfg.insert(
                "channels".to_string(),
                toml::Value::Array(
                    selected.iter().map(|s| toml::Value::String(s.clone())).collect(),
                ),
            );
        }
        "xy_scatter" => {
            if selected.len() != 2 {
                return None;
            }
            cfg.insert("x_channel".to_string(), toml::Value::String(selected[0].clone()));
            cfg.insert("y_channel".to_string(), toml::Value::String(selected[1].clone()));
        }
        _ => {
            if selected.len() != 1 {
                return None;
            }
            cfg.insert("channel".to_string(), toml::Value::String(selected[0].clone()));
        }
    }
    Some(PanelEntry { panel_type: panel_type.to_string(), config: cfg })
}

pub(crate) struct ReplayState {
    store: Arc<PlaybackStore>,
    playing: bool,
    speed: f32,
    last_frame: Instant,
}

pub(crate) enum AppMode {
    Live,
    Replay(ReplayState),
}

/// Ingest-derived fields collapsed from the running sources. `mqtt_topics`,
/// `mqtt_topic_map` and `ingest_schema_bytes` take the first source that
/// provides each (today at most one does). `conn_states` keeps EVERY source's
/// state so the status indicator can aggregate — showing LIVE when any source
/// is live rather than following one arbitrary source that may have no data.
pub(crate) struct DerivedIngest {
    pub conn_states: Vec<Arc<std::sync::atomic::AtomicU8>>,
    pub record_sender_slots:
        Vec<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
    pub ingest_schema_bytes: Vec<u8>,
    pub mqtt_topics: Option<Arc<Mutex<BTreeMap<String, String>>>>,
    pub mqtt_topic_map: Option<Arc<crate::dynamic_channel::MqttTopicMap>>,
}

impl DerivedIngest {
    pub(crate) fn from_handles(handles: Vec<crate::ingest::SourceHandle>) -> Self {
        let mut conn_states = Vec::new();
        let mut record_sender_slots = Vec::new();
        let mut ingest_schema_bytes = Vec::new();
        let mut mqtt_topics = None;
        let mut mqtt_topic_map = None;
        for h in handles {
            record_sender_slots.push(h.record_sender);
            conn_states.push(h.conn_state);
            if let Some(bytes) = h.schema_bytes {
                if ingest_schema_bytes.is_empty() {
                    ingest_schema_bytes = bytes;
                }
            }
            if let Some(d) = h.discovery {
                if mqtt_topics.is_none() {
                    mqtt_topics = Some(d.discovered);
                    mqtt_topic_map = Some(d.topic_map);
                }
            }
        }
        Self { conn_states, record_sender_slots, ingest_schema_bytes, mqtt_topics, mqtt_topic_map }
    }
}

/// Coarse connection status across all sources for the live indicator. `LIVE`
/// wins if any source is live (we are receiving data); otherwise `CONNECTING`
/// while any source is still trying, else `TIMEOUT`. `None` means no sources
/// (e.g. demo) — treated as live. Priority: LIVE > CONNECTING > TIMEOUT.
fn aggregate_conn_state(states: &[Arc<std::sync::atomic::AtomicU8>]) -> Option<u8> {
    use std::sync::atomic::Ordering;
    if states.is_empty() {
        return None;
    }
    let mut best = crate::ingest::TIMEOUT;
    for s in states {
        match s.load(Ordering::Relaxed) {
            crate::ingest::LIVE => return Some(crate::ingest::LIVE),
            crate::ingest::CONNECTING => best = crate::ingest::CONNECTING,
            _ => {}
        }
    }
    Some(best)
}

/// Top-level eframe app: menu bar, screen tabs, tiled workspace, dialogs.
pub struct DataVisApp {
    store: Arc<dyn ChannelStore>,
    live_store: Arc<dyn ChannelStore>,
    channels: Arc<ChannelRegistry>,
    registry: PanelRegistry,
    workspace: Workspace,
    layout_path: PathBuf,
    add_panel: AddPanelDialog,
    new_screen_name: String,
    status: String,
    /// When set, `status` is cleared once this instant passes. Used for
    /// transient confirmations (e.g. "layout saved") that should not linger.
    status_clear_at: Option<Instant>,
    conn_states: Vec<Arc<std::sync::atomic::AtomicU8>>,
    mode: AppMode,
    // Recording state
    record_handle: Option<RecordHandle>,
    record_sender_slots: Vec<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
    ingest_schema_bytes: Vec<u8>,
    // Channel picker sidebar
    channel_tree: ChannelTree,
    /// Live channel tree saved while a replay rebuilds the picker; restored on close.
    saved_channel_tree: Option<ChannelTree>,
    sidebar_visible: bool,
    /// MQTT discovered topics — populated by the MQTT ingest thread via `#`.
    mqtt_topics: Option<Arc<Mutex<BTreeMap<String, String>>>>,
    /// Shared topic→channel routing table; extended when an MQTT topic is
    /// dropped onto a panel so the ingest thread starts routing it.
    mqtt_topic_map: Option<Arc<crate::dynamic_channel::MqttTopicMap>>,
    /// Snapshot of mqtt_topics updated at 1 Hz so the UI doesn't lock every frame.
    mqtt_snapshot: BTreeMap<String, String>,
    mqtt_snapshot_at: Instant,
    dark_mode: bool,
    // Live scrub
    live_view_ns: Arc<AtomicI64>,
    live_view_offset_ns: i64,
    live_history_s: f64,
    /// When true the live view is frozen at `live_pause_ns` (absolute wall-clock
    /// ns). The store keeps ingesting; resuming returns to live and shows the
    /// data buffered during the pause.
    live_paused: bool,
    live_pause_ns: i64,
    /// Last observed store write counter. When it stops advancing the live view
    /// is idle, so the update loop drops to a slow heartbeat instead of forcing
    /// 60 fps repaints. See [`ChannelStore::write_seq`].
    last_write_seq: u64,
    /// App-wide default visible time span (seconds); panels without an explicit
    /// override follow it. Published to egui ctx data each frame.
    default_window_s: f64,
    /// Whether the toolbar "link time zoom" checkbox is on. In-memory only;
    /// published to egui ctx data each frame so time-based panels follow it.
    link_zoom: bool,
    // Scripting panel
    // Kept for future use (e.g. offline display without meta peek); the panel
    // currently builds the script list from `script_metas` which is richer.
    _available_scripts: Vec<String>,
    script_instances: Vec<ScriptInstance>,
    script_metas: SharedMetas,
    script_status: SharedStatus,
    script_commands: crossbeam_channel::Sender<ScriptCommand>,
    script_disabled: Arc<Mutex<Option<String>>>,
    script_panel_state: crate::script::panel::ScriptPanelState,
}

impl DataVisApp {
    pub fn new(
        store: Arc<dyn ChannelStore>,
        channels: Arc<ChannelRegistry>,
        registry: PanelRegistry,
        workspace: Workspace,
        layout_path: PathBuf,
        sources: Vec<crate::ingest::SourceHandle>,
        live_view_ns: Arc<AtomicI64>,
        live_history_s: f64,
        default_window_s: f64,
        available_scripts: Vec<String>,
        script_instances: Vec<ScriptInstance>,
        script_status: SharedStatus,
        script_commands: crossbeam_channel::Sender<ScriptCommand>,
        script_disabled: Arc<Mutex<Option<String>>>,
        script_metas: SharedMetas,
    ) -> Self {
        let DerivedIngest {
            conn_states,
            record_sender_slots,
            ingest_schema_bytes,
            mqtt_topics,
            mqtt_topic_map,
        } = DerivedIngest::from_handles(sources);
        let panel_type = registry
            .pickable_type_names()
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let channel_tree = ChannelTree::build(channels.as_ref());
        Self {
            live_store: store.clone(),
            store,
            channels,
            registry,
            workspace,
            layout_path,
            add_panel: AddPanelDialog { panel_type, ..Default::default() },
            new_screen_name: String::new(),
            status: String::new(),
            status_clear_at: None,
            conn_states,
            mode: AppMode::Live,
            record_handle: None,
            record_sender_slots,
            ingest_schema_bytes,
            channel_tree,
            saved_channel_tree: None,
            sidebar_visible: true,
            mqtt_topics,
            mqtt_topic_map,
            mqtt_snapshot: BTreeMap::new(),
            mqtt_snapshot_at: Instant::now() - Duration::from_secs(2),
            dark_mode: false,
            live_view_ns,
            live_view_offset_ns: 0,
            live_history_s,
            live_paused: false,
            live_pause_ns: 0,
            last_write_seq: 0,
            default_window_s,
            link_zoom: false,
            _available_scripts: available_scripts,
            script_instances,
            script_metas,
            script_status,
            script_commands,
            script_disabled,
            script_panel_state: Default::default(),
        }
    }

    /// Current layout including the app-wide window default.
    fn current_layout(&self) -> LayoutConfig {
        let mut cfg = self.workspace.to_config();
        cfg.default_window_s = self.default_window_s;
        cfg
    }

    fn save_layout(&mut self) {
        // Saving the layout also persists the script instances into the same
        // file — they share config.toml and there is no separate save action.
        let result =
            self.current_layout().save(&self.layout_path).and_then(|()| self.save_scripts());
        self.status = match result {
            Ok(()) => format!("layout saved to {}", self.layout_path.display()),
            Err(e) => format!("layout save failed: {e}"),
        };
        // Auto-clear this confirmation after 2s so it does not linger.
        self.status_clear_at = Some(Instant::now() + Duration::from_secs(2));
    }

    fn load_layout(&mut self) {
        match LayoutConfig::load(&self.layout_path) {
            Ok(cfg) => {
                self.default_window_s = cfg.default_window_s;
                self.workspace = Workspace::from_config(&cfg, &self.registry, &self.channels);
                self.status = format!("layout loaded from {}", self.layout_path.display());
            }
            Err(e) => self.status = format!("layout load failed: {e}"),
        }
    }

    /// Prompt for a target file and save the layout there, then make it the
    /// active config path so later quick-saves follow it. The channel section
    /// of an existing target is preserved (same merge as `save_layout`); a new
    /// target gets a layout-only file.
    fn save_layout_as(&mut self) {
        let start = self.layout_path.file_name().and_then(|n| n.to_str()).unwrap_or("config.toml");
        let Some(path) = rfd::FileDialog::new()
            .add_filter("TOML config", &["toml"])
            .set_file_name(start)
            .save_file()
        else {
            return;
        };
        self.layout_path = path;
        self.save_layout();
    }

    /// Prompt for a config file and load its layout, then make it the active
    /// config path. Channels are fixed at startup and unaffected.
    fn load_layout_from(&mut self) {
        let Some(path) =
            rfd::FileDialog::new().add_filter("TOML config", &["toml"]).pick_file()
        else {
            return;
        };
        self.layout_path = path;
        self.load_layout();
    }

    fn start_recording(&mut self) {
        if self.record_sender_slots.is_empty() {
            self.status = "Recording unavailable (no ingest source)".to_string();
            return;
        }
        let (tx, rx) = crate::record::record_channel();
        // Install the sender into every active ingest source (mpmc queue).
        for slot in &self.record_sender_slots {
            *slot.lock().unwrap() = Some(tx.clone());
        }
        match start_recording(
            Path::new("."),
            &self.channels,
            self.ingest_schema_bytes.clone(),
            rx,
        ) {
            Ok(handle) => {
                self.record_handle = Some(handle);
                self.status = "Recording started".to_string();
            }
            Err(e) => {
                // Remove senders since the recorder won't consume them.
                for slot in &self.record_sender_slots {
                    *slot.lock().unwrap() = None;
                }
                self.status = format!("Record failed: {e}");
            }
        }
    }

    fn stop_recording(&mut self) {
        // Remove senders first so ingest stops queuing, then drop handle to signal recorder.
        for slot in &self.record_sender_slots {
            *slot.lock().unwrap() = None;
        }
        self.record_handle = None;
        self.status = "Recording stopped".to_string();
    }

    fn open_recording(&mut self) {
        if self.record_handle.is_some() {
            self.status = "Stop recording before opening a file".to_string();
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MCAP recording", &["mcap"])
            .pick_file()
        else {
            return;
        };

        match PlaybackStore::load(&path, &self.channels) {
            Ok(playback) => {
                self.store = playback.clone();
                self.mode = AppMode::Replay(ReplayState {
                    store: playback,
                    playing: false,
                    speed: 1.0,
                    last_frame: Instant::now(),
                });
                self.saved_channel_tree = Some(self.channel_tree.clone());
                self.channel_tree = ChannelTree::build(&self.channels);
                self.status = format!("Loaded {}", path.display());
            }
            Err(e) => {
                self.status = format!("Failed to load recording: {e}");
            }
        }
    }

    fn close_replay(&mut self) {
        if let Some(tree) = self.saved_channel_tree.take() {
            self.channel_tree = tree;
        }
        self.store = self.live_store.clone();
        self.mode = AppMode::Live;
        self.live_view_offset_ns = 0;
        self.live_paused = false;
        self.status = "Replay closed".to_string();
    }

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button(format!("{} Save layout", icon::FLOPPY_DISK)).clicked() {
                        self.save_layout();
                        ui.close_menu();
                    }
                    if ui.button(format!("{} Save layout as…", icon::FLOPPY_DISK)).clicked() {
                        self.save_layout_as();
                        ui.close_menu();
                    }
                    if ui.button(format!("{} Load layout", icon::FOLDER_OPEN)).clicked() {
                        self.load_layout();
                        ui.close_menu();
                    }
                    if ui.button(format!("{} Load layout from…", icon::FOLDER_OPEN)).clicked() {
                        self.load_layout_from();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button(format!("{} Quit", icon::SIGN_OUT)).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Screen selector
                ui.label("screen:");
                let mut selected = self.workspace.active.clone();
                egui::ComboBox::from_id_source("screen-select")
                    .selected_text(&selected)
                    .show_ui(ui, |ui| {
                        for name in self.workspace.screens.keys() {
                            ui.selectable_value(&mut selected, name.clone(), name);
                        }
                    });
                if selected != self.workspace.active {
                    self.workspace.active = selected;
                }
                ui.menu_button(format!("{} screen", icon::MONITOR), |ui| {
                    ui.text_edit_singleline(&mut self.new_screen_name);
                    if ui
                        .button(format!("{} Create", icon::CHECK))
                        .clicked()
                        && !self.new_screen_name.is_empty()
                    {
                        let name = std::mem::take(&mut self.new_screen_name);
                        self.workspace.add_screen(&name);
                        ui.close_menu();
                    }
                });
                if ui
                    .button(icon::PLUS_SQUARE)
                    .on_hover_text("Add panel")
                    .clicked()
                {
                    self.add_panel.open = true;
                }
                if ui
                    .selectable_label(self.sidebar_visible, icon::SIDEBAR_SIMPLE)
                    .on_hover_text("Toggle channel sidebar")
                    .clicked()
                {
                    self.sidebar_visible = !self.sidebar_visible;
                }
                ui.separator();
                ui.label("window [s]:");
                ui.add(
                    egui::DragValue::new(&mut self.default_window_s)
                        .speed(0.1)
                        .range(0.1..=3600.0),
                )
                .on_hover_text("Default visible time span for time-based panels");
                if ui
                    .button(icon::MAGNIFYING_GLASS_MINUS)
                    .on_hover_text("Reset zoom on all panels")
                    .clicked()
                {
                    crate::viz::common::set_linked_zoom_range(ctx, None);
                    self.workspace.reset_zoom();
                }
                if ui
                    .checkbox(&mut self.link_zoom, icon::LINK)
                    .on_hover_text("Link time zoom across all panels")
                    .changed()
                {
                    if self.link_zoom {
                        // Just armed: start inert (no shared window yet).
                        crate::viz::common::set_linked_zoom_range(ctx, None);
                    } else {
                        // Just released: freeze the shared window into each
                        // panel (if any zoom was active), then clear it.
                        if let Some(r) = crate::viz::common::linked_zoom_range(ctx) {
                            self.workspace.freeze_time_zoom(r);
                        }
                        crate::viz::common::set_linked_zoom_range(ctx, None);
                    }
                }
                crate::viz::common::set_linked_zoom_enabled(ctx, self.link_zoom);
                ui.separator();
                let (theme_icon, theme_hint) =
                    if self.dark_mode { (icon::SUN, "Light mode") } else { (icon::MOON, "Dark mode") };
                if ui.button(theme_icon).on_hover_text(theme_hint).clicked() {
                    self.dark_mode = !self.dark_mode;
                    let visuals = if self.dark_mode {
                        egui::Visuals::dark()
                    } else {
                        egui::Visuals::light()
                    };
                    ctx.set_visuals(visuals);
                }
                ui.separator();

                match &self.mode {
                    AppMode::Live => {
                        // Connection state indicator. Light mode needs darker,
                        // saturated colors — LIGHT_GREEN/YELLOW wash out on white.
                        let dark = ui.visuals().dark_mode;
                        let green = if dark {
                            egui::Color32::LIGHT_GREEN
                        } else {
                            egui::Color32::from_rgb(0x1a, 0x7f, 0x37)
                        };
                        let amber = if dark {
                            egui::Color32::YELLOW
                        } else {
                            egui::Color32::from_rgb(0xb0, 0x6a, 0x00)
                        };
                        let red = if dark {
                            egui::Color32::RED
                        } else {
                            egui::Color32::from_rgb(0xc0, 0x28, 0x28)
                        };
                        let (label, color) = if self.live_paused {
                            ("PAUSED", amber)
                        } else {
                            match aggregate_conn_state(&self.conn_states) {
                                None | Some(crate::ingest::LIVE) => ("LIVE", green),
                                Some(crate::ingest::CONNECTING) => ("CONNECTING", amber),
                                Some(crate::ingest::TIMEOUT) => ("TIMEOUT", red),
                                Some(_) => ("?", egui::Color32::GRAY),
                            }
                        };
                        ui.colored_label(color, label);

                        // Pause/resume the live view.
                        let (pause_icon, pause_hint) = if self.live_paused {
                            (icon::PLAY, "Resume")
                        } else {
                            (icon::PAUSE, "Pause")
                        };
                        if ui.button(pause_icon).on_hover_text(pause_hint).clicked() {
                            if self.live_paused {
                                self.live_paused = false;
                            } else {
                                // Freeze whatever is on screen: the scrubbed instant
                                // if scrolled back, otherwise the current wall clock.
                                self.live_pause_ns = if self.live_view_offset_ns != 0 {
                                    crate::types::now_ns() + self.live_view_offset_ns
                                } else {
                                    crate::types::now_ns()
                                };
                                self.live_paused = true;
                            }
                        }
                        ui.separator();

                        // Record controls
                        if self.record_handle.is_none() {
                            if ui
                                .button(egui::RichText::new(icon::RECORD).color(egui::Color32::RED))
                                .on_hover_text("Record")
                                .clicked()
                            {
                                self.start_recording();
                            }
                            if ui
                                .button(icon::FOLDER_OPEN)
                                .on_hover_text("Open recording")
                                .clicked()
                            {
                                self.open_recording();
                            }
                        } else {
                            if ui.button(icon::STOP).on_hover_text("Stop recording").clicked() {
                                self.stop_recording();
                            }
                            if let Some(handle) = &self.record_handle {
                                let gaps = handle.gap_count.load(Ordering::Relaxed);
                                if gaps > 0 {
                                    ui.colored_label(egui::Color32::RED, format!("{gaps} gaps"));
                                }
                                if handle.record_failed.load(Ordering::Relaxed) {
                                    ui.colored_label(egui::Color32::RED, "WRITE ERROR");
                                }
                            }
                        }
                        ui.separator();

                        // Live scrub slider — drag back through buffered history.
                        // Reserve exactly the readout's measured width so the
                        // label always stays flush against the slider's right
                        // edge, whatever the window size or offset magnitude.
                        let readout_str = {
                            let pos = if self.live_view_offset_ns == 0 {
                                "live".to_string()
                            } else {
                                format!("{:.1}s", self.live_view_offset_ns as f64 / 1e9)
                            };
                            format!("{pos} / {:.0}s", self.live_history_s)
                        };
                        let readout_w = ui.fonts(|f| {
                            f.layout_no_wrap(
                                readout_str,
                                egui::TextStyle::Small.resolve(ui.style()),
                                egui::Color32::PLACEHOLDER,
                            )
                            .size()
                            .x
                        });
                        let slider_w = (ui.available_width()
                            - readout_w
                            - ui.spacing().item_spacing.x * 2.0)
                            .max(120.0);
                        let mut offset_secs = self.live_view_offset_ns as f64 / 1e9;
                        // Scrubbing is disabled while paused (the view is frozen).
                        // Drive the rail length via `slider_width` so the bar
                        // actually fills the reserved space — `add_sized` would
                        // only center a fixed-width rail, leaving a gap that grows
                        // with the window. Restore it so no other slider inherits.
                        let saved_slider_w = ui.spacing().slider_width;
                        ui.spacing_mut().slider_width = slider_w;
                        let mut changed = false;
                        ui.add_enabled_ui(!self.live_paused, |ui| {
                            changed = ui
                                .add(
                                    egui::Slider::new(&mut offset_secs, -self.live_history_s..=0.0)
                                        .show_value(false),
                                )
                                .changed();
                        });
                        ui.spacing_mut().slider_width = saved_slider_w;
                        if changed {
                            // Snap to live within 100 ms of the right edge.
                            self.live_view_offset_ns = if offset_secs > -0.1 {
                                0
                            } else {
                                (offset_secs * 1e9) as i64
                            };
                        }
                        // Position within the window / total span the slider covers.
                        let pos_txt = if self.live_view_offset_ns == 0 {
                            "live".to_string()
                        } else {
                            format!("{:.1}s", self.live_view_offset_ns as f64 / 1e9)
                        };
                        ui.label(
                            egui::RichText::new(format!(
                                "{pos_txt} / {:.0}s",
                                self.live_history_s
                            ))
                            .small()
                            .weak(),
                        )
                        .on_hover_text("scrub position / history covered by slider");
                    }
                    AppMode::Replay(_) => {
                        ui.colored_label(egui::Color32::LIGHT_BLUE, "REPLAY");
                        ui.separator();
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.status);
                });
            });
        });
    }

    fn channel_picker_side(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("channel_picker")
            .min_width(160.0)
            .default_width(200.0)
            .show(ctx, |ui| {
                ui.heading("Channels");
                ui.separator();

                let avail_h = ui.available_height();
                // One drag-only tree for every channel. Registry (ZMQ + dropped
                // MQTT) channels get their live value from the store; discovered
                // MQTT topics not yet in the registry come from the snapshot
                // (refreshed at the top of `update`).
                let store = self.store.clone();
                let now = store.now_ns();
                let channels = &self.channels;
                egui::ScrollArea::vertical()
                    .id_source("ch_tree_scroll")
                    .max_height(avail_h)
                    .show(ui, |ui| {
                        // Pick up channels registered since build (script
                        // outputs, dropped MQTT topics) before rendering.
                        self.channel_tree.sync(channels);
                        self.channel_tree.ui(ui, &self.mqtt_snapshot, |name| {
                            let id = channels.id(name)?;
                            let (_, sample) = store.latest_at(id, now)?;
                            Some(fmt_sample(&sample))
                        });
                    });

                ui.separator();
                {
                    let disabled = self.script_disabled.lock().unwrap().clone();
                    // Compact sidebar list (names + status); editing is in a window.
                    crate::script::panel::draw_script_panel(
                        ui,
                        &mut self.script_panel_state,
                        &self.script_instances,
                        &self.script_status,
                        &disabled,
                    );

                    // Full editor lives in a floating settings window. Mirror the
                    // channel tree's set: registry channels plus discovered-but-
                    // unregistered MQTT topics, so the picker searches all of it.
                    let mut channel_names: Vec<String> =
                        self.channels.iter_ids().map(|id| self.channels.meta(id).name.clone()).collect();
                    for topic in self.mqtt_snapshot.keys() {
                        if !channel_names.iter().any(|n| n == topic) {
                            channel_names.push(topic.clone());
                        }
                    }
                    let metas = self.script_metas.lock().unwrap().clone();
                    let ctx = ui.ctx().clone();
                    let cmds = crate::script::panel::draw_script_settings(
                        &ctx,
                        &mut self.script_panel_state,
                        &self.script_instances,
                        &metas,
                        &channel_names,
                        &self.script_status,
                        &disabled,
                    );
                    for cmd in cmds {
                        self.apply_panel_command(cmd);
                    }
                }
            });
    }

    fn apply_panel_command(&mut self, cmd: crate::script::panel::PanelCommand) {
        use crate::script::panel::PanelCommand;
        use crate::script::ScriptCommand;
        match cmd {
            PanelCommand::Upsert(inst) => {
                match self.script_instances.iter_mut().find(|i| i.id == inst.id) {
                    Some(slot) => *slot = inst.clone(),
                    None => self.script_instances.push(inst.clone()),
                }
                let _ = self.script_commands.send(ScriptCommand::Upsert(inst));
            }
            PanelCommand::Remove(id) => {
                self.script_instances.retain(|i| i.id != id);
                let _ = self.script_commands.send(ScriptCommand::Remove(id));
            }
        }
    }

    /// Write the current script instances into config.toml, preserving every
    /// other section. Called as part of [`Self::save_layout`] — there is no
    /// separate scripts-save action.
    fn save_scripts(&self) -> anyhow::Result<()> {
        let existing = std::fs::read_to_string(&self.layout_path).unwrap_or_default();
        let mut cfg =
            crate::script::config::ScriptsConfig::from_toml_str(&existing).unwrap_or_default();
        cfg.instances = self.script_instances.clone();
        cfg.save(&self.layout_path)
    }

    fn add_panel_window(&mut self, ctx: &egui::Context) {
        if !self.add_panel.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Add panel")
            .open(&mut open)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::ComboBox::from_label("type")
                    .selected_text(&self.add_panel.panel_type)
                    .show_ui(ui, |ui| {
                        for t in self.registry.pickable_type_names() {
                            ui.selectable_value(&mut self.add_panel.panel_type, t.to_string(), t);
                        }
                    });
                ui.label(match self.add_panel.panel_type.as_str() {
                    "xy_scatter" => "select exactly 2 channels (x first, then y)",
                    "waveform" | "log" => "select one or more channels",
                    _ => "select exactly 1 channel",
                });
                ui.separator();
                for id in self.channels.iter_ids() {
                    let name = self.channels.meta(id).name.clone();
                    let mut checked = self.add_panel.selected.contains(&name);
                    if ui.checkbox(&mut checked, &name).changed() {
                        if checked {
                            self.add_panel.selected.push(name);
                        } else {
                            self.add_panel.selected.retain(|n| n != &name);
                        }
                    }
                }
                ui.separator();
                let entry = build_panel_entry(&self.add_panel.panel_type, &self.add_panel.selected);
                if ui
                    .add_enabled(entry.is_some(), egui::Button::new("Add"))
                    .clicked()
                {
                    if let Some(e) = entry {
                        if let Err(err) =
                            self.workspace.add_panel(&e, &self.registry, &self.channels)
                        {
                            self.status = format!("add panel failed: {err}");
                        }
                        self.add_panel.selected.clear();
                        self.add_panel.open = false;
                    }
                }
            });
        if !open {
            self.add_panel.open = false;
        }
    }
}

impl eframe::App for DataVisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Repaint only as fast as something actually changes. A blanket 60 fps
        // repaint burns a full CPU core re-tessellating static panels even with
        // no data. Animate at 60 fps while replay is playing or fresh live
        // samples are arriving; otherwise fall back to a slow heartbeat that
        // still polls for new data and connection state. Any input event
        // repaints immediately regardless of this hint.
        let animating = match self.mode {
            AppMode::Replay(ref rs) => rs.playing,
            AppMode::Live => {
                let seq = self.store.write_seq();
                let changed = seq != self.last_write_seq;
                self.last_write_seq = seq;
                changed && !self.live_paused
            }
        };
        ctx.request_repaint_after(Duration::from_millis(if animating { 16 } else { 200 }));

        // Expire transient status messages (e.g. "layout saved"). The heartbeat
        // repaint above guarantees this runs within ~200ms of the deadline.
        if let Some(deadline) = self.status_clear_at {
            if Instant::now() >= deadline {
                self.status.clear();
                self.status_clear_at = None;
            }
        }

        // Publish the app-wide default window so panels can read it this frame.
        crate::viz::common::set_global_window_s(ctx, self.default_window_s);

        // Advance playback clock before any rendering.
        if let AppMode::Replay(ref mut rs) = self.mode {
            if rs.playing {
                let delta_ns = rs.last_frame.elapsed().as_nanos() as i64;
                let advance = (delta_ns as f64 * rs.speed as f64) as i64;
                let pos = rs.store.position_ns.load(Ordering::Relaxed);
                let end = rs.store.start_ns + rs.store.duration_ns;
                let new_pos = (pos + advance).min(end);
                rs.store.position_ns.store(new_pos, Ordering::Relaxed);
                if new_pos >= end {
                    rs.playing = false;
                }
            }
            rs.last_frame = Instant::now();
        }

        // Keep the live store's view_override in sync with the scrub offset.
        // 0 means "use wall clock"; non-zero freezes the view at that ns value.
        if matches!(self.mode, AppMode::Live) {
            let v = if self.live_paused {
                self.live_pause_ns
            } else if self.live_view_offset_ns != 0 {
                crate::types::now_ns() + self.live_view_offset_ns
            } else {
                0
            };
            self.live_view_ns.store(v, Ordering::Relaxed);
        }

        // Publish the active store's clock once per frame so every panel shares
        // one time base: equal live windows start at the same time and grid
        // lines coincide. `self.store` is the active store in both modes (the
        // playback store during replay), read after the live view/playback
        // clock is settled above.
        crate::viz::common::set_frame_clock(ctx, self.store.now_ns());

        // Refresh the discovered-MQTT-topic snapshot (throttled) and let panels
        // re-bind channels that were unknown at layout-load time — e.g. a
        // drop-created (dynamic) MQTT channel whose topic has just reappeared
        // after a restart. Runs regardless of sidebar visibility.
        if self.mqtt_topics.is_some() && self.mqtt_snapshot_at.elapsed() >= Duration::from_secs(1) {
            if let Some(arc) = &self.mqtt_topics {
                self.mqtt_snapshot = arc.lock().unwrap().clone();
            }
            self.mqtt_snapshot_at = Instant::now();
            if matches!(self.mode, AppMode::Live) {
                let mqtt_ctx = self.mqtt_topic_map.as_deref().map(|tm| (tm, &self.mqtt_snapshot));
                self.workspace.refresh_bindings(&self.channels, self.store.as_ref(), mqtt_ctx);
            }
        }

        self.menu_bar(ctx);
        self.toolbar(ctx);

        // Timeline bottom panel — close_replay flag defers the borrow-conflicting call.
        let mut close_replay = false;
        if let AppMode::Replay(ref mut rs) = self.mode {
            egui::TopBottomPanel::bottom("timeline").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let play_label = if rs.playing { "Pause" } else { "Play" };
                    if ui.button(play_label).clicked() {
                        rs.playing = !rs.playing;
                    }

                    egui::ComboBox::from_id_source("timeline_speed")
                        .selected_text(format!("{}x", rs.speed))
                        .show_ui(ui, |ui| {
                            for &s in &[0.25f32, 0.5, 1.0, 2.0, 4.0] {
                                ui.selectable_value(&mut rs.speed, s, format!("{s}x"));
                            }
                        });

                    ui.separator();

                    let pos = rs.store.position_ns.load(Ordering::Relaxed);
                    let start = rs.store.start_ns;
                    let dur = rs.store.duration_ns.max(1);
                    let mut offset = (pos - start) as f64;

                    // Reserve space for the right-side label and close button, give the
                    // rest to the slider so it stretches across the full window width.
                    let right_reserved = 140.0;
                    let slider_w = (ui.available_width() - right_reserved).max(60.0);
                    if ui
                        .add_sized(
                            [slider_w, ui.spacing().interact_size.y],
                            egui::Slider::new(&mut offset, 0.0..=(dur as f64))
                                .show_value(false),
                        )
                        .changed()
                    {
                        rs.store.position_ns.store(start + offset as i64, Ordering::Relaxed);
                    }

                    let t_secs = (pos - start) as f64 / 1e9;
                    let dur_secs = dur as f64 / 1e9;
                    ui.label(format!("{:.1}s / {:.1}s", t_secs, dur_secs));

                    if ui.button(icon::X).on_hover_text("Close recording").clicked() {
                        close_replay = true;
                    }
                });
            });
        }
        if close_replay {
            self.close_replay();
        }

        self.add_panel_window(ctx);

        if self.sidebar_visible {
            self.channel_picker_side(ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Dynamic MQTT-topic binding is only offered in Live mode, where
            // `self.store` is the growable LiveStore.
            let mqtt_ctx = if matches!(self.mode, AppMode::Live) {
                self.mqtt_topic_map
                    .as_deref()
                    .map(|tm| (tm, &self.mqtt_snapshot))
            } else {
                None
            };
            self.workspace
                .ui(ui, self.store.as_ref(), &self.channels, &self.registry, mqtt_ctx);
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.current_layout().save(&self.layout_path).and_then(|()| self.save_scripts());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_mode_transitions_compile() {
        // Checks that AppMode, ReplayState types exist and are constructible.
        // Full UI tests require eframe harness; this just verifies the types.
        let _live = AppMode::Live;
    }

    #[test]
    fn build_entry_single_channel_types() {
        for t in ["numeric", "gauge", "spectrum", "state_graph"] {
            let e = build_panel_entry(t, &["a".into()]).unwrap();
            assert_eq!(e.panel_type, t);
            assert_eq!(e.config["channel"], toml::Value::String("a".into()));
            assert!(build_panel_entry(t, &[]).is_none());
            assert!(build_panel_entry(t, &["a".into(), "b".into()]).is_none());
        }
    }

    #[test]
    fn build_entry_multi_channel_types() {
        for t in ["waveform", "log"] {
            let e = build_panel_entry(t, &["a".into(), "b".into()]).unwrap();
            assert_eq!(
                e.config["channels"],
                toml::Value::Array(vec![
                    toml::Value::String("a".into()),
                    toml::Value::String("b".into())
                ])
            );
            assert!(build_panel_entry(t, &[]).is_none());
        }
    }

    #[test]
    fn build_entry_xy_needs_exactly_two() {
        let e = build_panel_entry("xy_scatter", &["x".into(), "y".into()]).unwrap();
        assert_eq!(e.config["x_channel"], toml::Value::String("x".into()));
        assert_eq!(e.config["y_channel"], toml::Value::String("y".into()));
        assert!(build_panel_entry("xy_scatter", &["x".into()]).is_none());
        assert!(build_panel_entry("xy_scatter", &["a".into(), "b".into(), "c".into()]).is_none());
    }

    #[test]
    fn record_sender_slots_install_and_clear() {
        use crate::ingest::SourceHandle;
        use std::sync::atomic::AtomicU8;
        use std::sync::{Arc, Mutex};

        let (tx, _rx) = crate::record::record_channel();
        // Build two handles, each with their own record_sender slot.
        let make_handle = |name: &str| SourceHandle {
            name: name.into(),
            conn_state: Arc::new(AtomicU8::new(0)),
            record_sender: Arc::new(Mutex::new(None)),
            discovery: None,
            schema_bytes: None,
            child_guard: None,
        };
        let h1 = make_handle("a");
        let h2 = make_handle("b");
        let slots = vec![h1.record_sender.clone(), h2.record_sender.clone()];
        // Install into all.
        for slot in &slots {
            *slot.lock().unwrap() = Some(tx.clone());
        }
        assert!(slots.iter().all(|s| s.lock().unwrap().is_some()));
        // Clear all.
        for slot in &slots {
            *slot.lock().unwrap() = None;
        }
        assert!(slots.iter().all(|s| s.lock().unwrap().is_none()));
    }

    #[test]
    fn derives_ingest_fields_from_handles() {
        use crate::ingest::{Discovery, SourceHandle};
        use std::collections::BTreeMap;
        use std::sync::atomic::AtomicU8;
        use std::sync::{Arc, Mutex, RwLock};

        let mqtt = SourceHandle {
            name: "mqtt".into(),
            conn_state: Arc::new(AtomicU8::new(0)),
            record_sender: Arc::new(Mutex::new(None)),
            discovery: Some(Discovery {
                discovered: Arc::new(Mutex::new(BTreeMap::new())),
                topic_map: Arc::new(RwLock::new(std::collections::HashMap::new())),
            }),
            schema_bytes: None,
            child_guard: None,
        };
        let zmq = SourceHandle {
            name: "zmq".into(),
            conn_state: Arc::new(AtomicU8::new(1)),
            record_sender: Arc::new(Mutex::new(None)),
            discovery: None,
            schema_bytes: Some(vec![1, 2, 3]),
            child_guard: None,
        };
        let d = DerivedIngest::from_handles(vec![mqtt, zmq]);
        assert_eq!(d.record_sender_slots.len(), 2);
        assert_eq!(d.ingest_schema_bytes, vec![1, 2, 3]);
        assert!(d.mqtt_topics.is_some());
        assert!(d.mqtt_topic_map.is_some());
        // Every source's conn_state is kept (mqtt CONNECTING=0, zmq LIVE=1).
        assert_eq!(d.conn_states.len(), 2);
    }

    #[test]
    fn aggregate_conn_state_prefers_live() {
        use crate::ingest::{CONNECTING, LIVE, TIMEOUT};
        use std::sync::atomic::AtomicU8;

        // No sources → None (treated as live, e.g. demo).
        assert_eq!(aggregate_conn_state(&[]), None);
        // Any source LIVE wins, even if another is stuck CONNECTING — this is the
        // MQTT-live-while-ZMQ-has-no-publisher case.
        let connecting = Arc::new(AtomicU8::new(CONNECTING));
        let live = Arc::new(AtomicU8::new(LIVE));
        assert_eq!(aggregate_conn_state(&[connecting.clone(), live]), Some(LIVE));
        // None live: CONNECTING outranks TIMEOUT.
        let timeout = Arc::new(AtomicU8::new(TIMEOUT));
        assert_eq!(aggregate_conn_state(&[timeout.clone(), connecting]), Some(CONNECTING));
        // All timed out.
        assert_eq!(aggregate_conn_state(&[timeout]), Some(TIMEOUT));
    }

    #[test]
    fn channel_tree_clone_roundtrips_and_rebuild_adds_dynamic() {
        use crate::config::ChannelRegistry;
        use crate::channel_tree::ChannelTree;
        use crate::types::SampleType;

        let reg = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#).unwrap();
        let saved = ChannelTree::build(&reg);
        let restored = saved.clone(); // Clone must be available

        // After a dynamic add, the channel exists and a freshly built tree is the
        // one open_recording swaps in.
        let id = reg.add_dynamic("home/temp", "home/temp", SampleType::Float);
        assert_eq!(reg.meta(id).sample_type, SampleType::Float);
        assert!(reg.id("home/temp").is_some());
        let _rebuilt = ChannelTree::build(&reg);

        // `restored` is what close_replay puts back; it must be usable (Clone worked).
        let _restored_again = restored.clone();
        let _ = saved; // built from the pre-add registry, independent of the rebuild
    }
}
