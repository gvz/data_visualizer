use eframe::egui;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::SampleType;
use crate::viz::VizPanel;

/// A freshly-split, not-yet-defined pane. It has no visualization of its own;
/// the workspace's pane renderer intercepts panes of this type and draws the
/// type-picker buttons, then replaces the slot once a type is chosen. This is a
/// real registered type so an undefined pane survives a layout save/reload.
pub const TYPE_NAME: &str = "placeholder";

pub struct PlaceholderPanel;

pub fn ctor(
    _cfg: &toml::Table,
    _reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    Ok(Box::new(PlaceholderPanel))
}

impl VizPanel for PlaceholderPanel {
    fn title(&self) -> &str {
        ""
    }

    fn accepted_types(&self) -> &[SampleType] {
        &[]
    }

    fn config_ui(&mut self, _ui: &mut egui::Ui) {}

    // Never reached in normal use — the pane renderer draws the picker for
    // undefined panes. Kept as a sane fallback if rendered directly.
    fn render(&mut self, ui: &mut egui::Ui, _store: &dyn ChannelStore) {
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new("Choose a panel type").weak());
        });
    }

    fn serialize(&self) -> toml::Table {
        toml::Table::new()
    }
}
