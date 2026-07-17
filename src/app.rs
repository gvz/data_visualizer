use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
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

/// Top-level eframe app: menu bar, screen tabs, tiled workspace, dialogs.
pub struct DataVisApp {
    store: Arc<dyn ChannelStore>,
    channels: ChannelRegistry,
    registry: PanelRegistry,
    workspace: Workspace,
    layout_path: PathBuf,
    add_panel: AddPanelDialog,
    new_screen_name: String,
    status: String,
    conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
}

impl DataVisApp {
    pub fn new(
        store: Arc<dyn ChannelStore>,
        channels: ChannelRegistry,
        registry: PanelRegistry,
        workspace: Workspace,
        layout_path: PathBuf,
        conn_state: Option<Arc<std::sync::atomic::AtomicU8>>,
    ) -> Self {
        let panel_type = registry
            .type_names()
            .first()
            .map(|s| s.to_string())
            .unwrap_or_default();
        Self {
            store,
            channels,
            registry,
            workspace,
            layout_path,
            add_panel: AddPanelDialog { panel_type, ..Default::default() },
            new_screen_name: String::new(),
            status: String::new(),
            conn_state,
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
                ui.separator();
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.status);
                });
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
        // Live data keeps coming whether or not there is input.
        ctx.request_repaint();
        self.menu_bar(ctx);
        self.toolbar(ctx);
        self.add_panel_window(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            self.workspace.ui(ui, self.store.as_ref());
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Spec: layout auto-saves on exit.
        let _ = self.workspace.to_config().save(&self.layout_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
