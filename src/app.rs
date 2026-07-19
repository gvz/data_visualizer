use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui;

use crate::channel_tree::ChannelTree;
use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
use crate::record::playback::PlaybackStore;
use crate::record::{start_recording, RecordHandle};
use crate::store::ChannelStore;
use crate::viz::PanelRegistry;
use crate::workspace::Workspace;

/// State for the "Add panel" modal.
#[derive(Default)]
struct AddPanelDialog {
    open: bool,
    panel_type: String,
    /// Channel names in click order (order matters for xy_scatter: x then y).
    selected: Vec<String>,
}

/// Channel-count rules per panel type. None = invalid selection for that type.
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

/// Top-level eframe app: menu bar, screen tabs, tiled workspace, dialogs.
pub struct DataVisApp {
    store: Arc<dyn ChannelStore>,
    live_store: Arc<dyn ChannelStore>,
    channels: ChannelRegistry,
    registry: PanelRegistry,
    workspace: Workspace,
    layout_path: PathBuf,
    add_panel: AddPanelDialog,
    new_screen_name: String,
    status: String,
    conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
    mode: AppMode,
    // Recording state
    record_handle: Option<RecordHandle>,
    record_sender_slot: Option<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
    ingest_schema_bytes: Vec<u8>,
    // Channel picker sidebar
    channel_tree: ChannelTree,
    sidebar_visible: bool,
    // Live scrub
    live_view_ns: Arc<AtomicI64>,
    live_view_offset_ns: i64,
    live_history_s: f64,
}

impl DataVisApp {
    pub fn new(
        store: Arc<dyn ChannelStore>,
        channels: ChannelRegistry,
        registry: PanelRegistry,
        workspace: Workspace,
        layout_path: PathBuf,
        conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
        record_sender_slot: Option<Arc<Mutex<Option<crossbeam_channel::Sender<crate::record::RecordMsg>>>>>,
        ingest_schema_bytes: Vec<u8>,
        live_view_ns: Arc<AtomicI64>,
        live_history_s: f64,
    ) -> Self {
        let panel_type = registry
            .type_names()
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let channel_tree = ChannelTree::build(&channels);
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
            conn_state,
            mode: AppMode::Live,
            record_handle: None,
            record_sender_slot,
            ingest_schema_bytes,
            channel_tree,
            sidebar_visible: true,
            live_view_ns,
            live_view_offset_ns: 0,
            live_history_s,
        }
    }

    fn save_layout(&mut self) {
        self.status = match self.workspace.to_config().save(&self.layout_path) {
            Ok(()) => format!("layout saved to {}", self.layout_path.display()),
            Err(e) => format!("layout save failed: {e}"),
        };
    }

    fn load_layout(&mut self) {
        match LayoutConfig::load(&self.layout_path) {
            Ok(cfg) => {
                self.workspace = Workspace::from_config(&cfg, &self.registry, &self.channels);
                self.status = format!("layout loaded from {}", self.layout_path.display());
            }
            Err(e) => self.status = format!("layout load failed: {e}"),
        }
    }

    fn start_recording(&mut self) {
        if self.record_sender_slot.is_none() {
            self.status = "Recording not available in demo mode".to_string();
            return;
        }
        let (tx, rx) = crate::record::record_channel();
        // Install sender so the ingest thread starts queuing messages.
        if let Some(slot) = &self.record_sender_slot {
            *slot.lock().unwrap() = Some(tx);
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
                // Remove sender since the recorder won't consume it.
                if let Some(slot) = &self.record_sender_slot {
                    *slot.lock().unwrap() = None;
                }
                self.status = format!("Record failed: {e}");
            }
        }
    }

    fn stop_recording(&mut self) {
        // Remove sender first so ingest stops queuing, then drop handle to signal recorder.
        if let Some(slot) = &self.record_sender_slot {
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

        if self.ingest_schema_bytes.is_empty() {
            self.status = "Replay not available in demo mode (no proto schema)".to_string();
            return;
        }
        let schema = match crate::ingest::loader::ProtoSchema::from_bytes(&self.ingest_schema_bytes) {
            Ok(s) => s,
            Err(e) => {
                self.status = format!("Failed to reconstruct schema: {e}");
                return;
            }
        };
        match PlaybackStore::load(&path, &self.channels, &schema) {
            Ok(playback) => {
                self.store = playback.clone();
                self.mode = AppMode::Replay(ReplayState {
                    store: playback,
                    playing: false,
                    speed: 1.0,
                    last_frame: Instant::now(),
                });
                self.status = format!("Loaded {}", path.display());
            }
            Err(e) => {
                self.status = format!("Failed to load recording: {e}");
            }
        }
    }

    fn close_replay(&mut self) {
        self.store = self.live_store.clone();
        self.mode = AppMode::Live;
        self.live_view_offset_ns = 0;
        self.status = "Replay closed".to_string();
    }

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Save layout").clicked() {
                        self.save_layout();
                        ui.close_menu();
                    }
                    if ui.button("Load layout").clicked() {
                        self.load_layout();
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
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
                ui.menu_button("+ screen", |ui| {
                    ui.text_edit_singleline(&mut self.new_screen_name);
                    if ui.button("Create").clicked() && !self.new_screen_name.is_empty() {
                        let name = std::mem::take(&mut self.new_screen_name);
                        self.workspace.add_screen(&name);
                        ui.close_menu();
                    }
                });
                if ui.button("+ panel").clicked() {
                    self.add_panel.open = true;
                }
                if ui.selectable_label(self.sidebar_visible, "Channels").clicked() {
                    self.sidebar_visible = !self.sidebar_visible;
                }
                ui.separator();

                match &self.mode {
                    AppMode::Live => {
                        // Connection state indicator
                        let (label, color) = match self
                            .conn_state
                            .as_ref()
                            .map(|s| s.load(std::sync::atomic::Ordering::Relaxed))
                        {
                            None | Some(crate::ingest::LIVE) => ("LIVE", egui::Color32::LIGHT_GREEN),
                            Some(crate::ingest::CONNECTING) => ("CONNECTING", egui::Color32::YELLOW),
                            Some(crate::ingest::TIMEOUT) => ("TIMEOUT", egui::Color32::RED),
                            Some(_) => ("?", egui::Color32::GRAY),
                        };
                        ui.colored_label(color, label);
                        ui.separator();

                        // Record controls
                        if self.record_handle.is_none() {
                            if ui.button("Rec").clicked() {
                                self.start_recording();
                            }
                            if ui.button("Open recording").clicked() {
                                self.open_recording();
                            }
                        } else {
                            if ui.button("Stop Rec").clicked() {
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
                egui::ScrollArea::vertical()
                    .id_source("ch_tree_scroll")
                    .max_height((avail_h - 90.0).max(60.0))
                    .show(ui, |ui| {
                        self.channel_tree.ui(ui, &mut self.add_panel.selected);
                    });

                ui.separator();

                ui.label(match self.add_panel.panel_type.as_str() {
                    "xy_scatter" => "select exactly 2 channels",
                    "waveform" | "log" => "select 1 or more channels",
                    _ => "select 1 channel",
                });
                egui::ComboBox::from_id_source("sidebar_panel_type")
                    .selected_text(&self.add_panel.panel_type)
                    .show_ui(ui, |ui| {
                        for t in self.registry.type_names() {
                            ui.selectable_value(
                                &mut self.add_panel.panel_type,
                                t.to_string(),
                                t,
                            );
                        }
                    });
                ui.horizontal(|ui| {
                    let entry = build_panel_entry(
                        &self.add_panel.panel_type,
                        &self.add_panel.selected,
                    );
                    if ui.add_enabled(entry.is_some(), egui::Button::new("Add")).clicked() {
                        if let Some(e) = entry {
                            if let Err(err) =
                                self.workspace.add_panel(&e, &self.registry, &self.channels)
                            {
                                self.status = format!("add panel failed: {err}");
                            }
                            self.add_panel.selected.clear();
                        }
                    }
                    if !self.add_panel.selected.is_empty() && ui.button("Clear").clicked() {
                        self.add_panel.selected.clear();
                    }
                });
            });
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
                        for t in self.registry.type_names() {
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
        ctx.request_repaint();

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
            let v = if self.live_view_offset_ns != 0 {
                crate::types::now_ns() + self.live_view_offset_ns
            } else {
                0
            };
            self.live_view_ns.store(v, Ordering::Relaxed);
        }

        self.menu_bar(ctx);
        self.toolbar(ctx);

        // Live timeline — always visible in live mode so the user can scrub history.
        if let AppMode::Live = self.mode {
            egui::TopBottomPanel::bottom("live_timeline").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let right_reserved = 70.0;
                    let slider_w = (ui.available_width() - right_reserved).max(60.0);
                    let mut offset_secs = self.live_view_offset_ns as f64 / 1e9;
                    if ui
                        .add_sized(
                            [slider_w, ui.spacing().interact_size.y],
                            egui::Slider::new(&mut offset_secs, -self.live_history_s..=0.0)
                                .show_value(false),
                        )
                        .changed()
                    {
                        // Snap to live within 100 ms of the right edge.
                        self.live_view_offset_ns = if offset_secs > -0.1 {
                            0
                        } else {
                            (offset_secs * 1e9) as i64
                        };
                    }
                    if self.live_view_offset_ns == 0 {
                        ui.colored_label(egui::Color32::LIGHT_GREEN, "LIVE");
                    } else {
                        ui.label(format!("{:.1}s", self.live_view_offset_ns as f64 / 1e9));
                    }
                });
            });
        }

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

                    if ui.button("Close").clicked() {
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
            self.workspace.ui(ui, self.store.as_ref());
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.workspace.to_config().save(&self.layout_path);
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
}
