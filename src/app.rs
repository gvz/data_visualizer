use std::sync::Arc;

use eframe::egui;

use crate::store::ChannelStore;
use crate::viz::VizPanel;

/// Top-level eframe app. Foundation version: single screen, panels stacked
/// vertically. The layout-engine plan replaces the stack with egui_tiles;
/// the replay plan makes `store` swappable (live ↔ playback).
pub struct DataVisApp {
    store: Arc<dyn ChannelStore>,
    screen_name: String,
    panels: Vec<Box<dyn VizPanel>>,
}

impl DataVisApp {
    pub fn new(
        store: Arc<dyn ChannelStore>,
        screen_name: String,
        panels: Vec<Box<dyn VizPanel>>,
    ) -> Self {
        Self { store, screen_name, panels }
    }
}

impl eframe::App for DataVisApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Live data keeps coming whether or not there is input — repaint
        // continuously instead of waiting for events.
        ctx.request_repaint();

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("screen: {}", self.screen_name));
                ui.separator();
                ui.colored_label(egui::Color32::LIGHT_GREEN, "LIVE");
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for panel in &mut self.panels {
                    ui.group(|ui| {
                        ui.heading(panel.title());
                        panel.render(ui, self.store.as_ref());
                    });
                }
            });
        });
    }
}
