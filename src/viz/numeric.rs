use anyhow::anyhow;
use eframe::egui;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, Sample, SampleType};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "numeric";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int, SampleType::Bool];

/// Large latest-value display with unit label.
pub struct NumericPanel {
    channel_name: String,
    channel: Option<ChannelId>,
    type_ok: bool,
    unit: String,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let channel_name = cfg
        .get("channel")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("numeric panel: missing string key `channel`"))?
        .to_string();
    let channel = reg.id(&channel_name);
    let (type_ok, unit) = match channel {
        Some(id) => {
            let meta = reg.meta(id);
            (ACCEPTED.contains(&meta.sample_type), meta.unit.clone())
        }
        None => (true, String::new()),
    };
    Ok(Box::new(NumericPanel { channel_name, channel, type_ok, unit }))
}

impl VizPanel for NumericPanel {
    fn title(&self) -> &str {
        &self.channel_name
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("channel: {}", self.channel_name));
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        let Some(id) = self.channel else {
            ui.colored_label(
                egui::Color32::RED,
                format!("unknown channel `{}`", self.channel_name),
            );
            return;
        };
        if !self.type_ok {
            ui.colored_label(
                egui::Color32::RED,
                format!(
                    "channel `{}` has a type not supported by the numeric panel",
                    self.channel_name
                ),
            );
            return;
        }
        let text = match store.latest(id) {
            Some((_, Sample::Float(v))) => format!("{v:.3}"),
            Some((_, Sample::Int(v))) => v.to_string(),
            Some((_, Sample::Bool(b))) => if b { "ON" } else { "OFF" }.to_string(),
            Some((_, Sample::Text(_))) | None => "\u{2014}".to_string(),
        };
        ui.label(egui::RichText::new(format!("{text} {}", self.unit)).size(32.0));
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channel".to_string(),
            toml::Value::String(self.channel_name.clone()),
        );
        t
    }
}
