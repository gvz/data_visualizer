use eframe::egui;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::resolve_or_register_drop;
use crate::store::ChannelStore;
use crate::types::{ChannelId, Sample, SampleType};
use crate::viz::common::{label_config_row, opt_label, opt_str, serialize_label, RebindCtx};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "numeric";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int, SampleType::Bool];

/// Large latest-value display with unit label.
pub struct NumericPanel {
    channel_name: String,
    label: Option<String>,
    channel: Option<ChannelId>,
    type_ok: bool,
    unit: String,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let channel_name = opt_str(cfg, "channel");
    let channel = reg.id(&channel_name);
    let (type_ok, unit) = match channel {
        Some(id) => {
            let meta = reg.meta(id);
            (ACCEPTED.contains(&meta.sample_type), meta.unit.clone())
        }
        None => (true, String::new()),
    };
    Ok(Box::new(NumericPanel { channel_name, label: opt_label(cfg), channel, type_ok, unit }))
}

impl VizPanel for NumericPanel {
    fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.channel_name)
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("channel: {}", self.channel_name));
        label_config_row(ui, &mut self.label, &self.channel_name);
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if self.channel_name.is_empty() {
            ui.label(egui::RichText::new("Drop a channel here").weak());
            return;
        }
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
        let text = match store.latest_at(id, store.now_ns()) {
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
        serialize_label(&mut t, &self.label);
        t
    }

    fn drop_channel(&mut self, name: &str, reg: &ChannelRegistry) {
        self.channel_name = name.to_string();
        self.channel = reg.id(name);
        let (type_ok, unit) = match self.channel {
            Some(id) => {
                let meta = reg.meta(id);
                (ACCEPTED.contains(&meta.sample_type), meta.unit.clone())
            }
            None => (true, String::new()),
        };
        self.type_ok = type_ok;
        self.unit = unit;
    }

    fn refresh_bindings(&mut self, ctx: &RebindCtx) {
        if self.channel.is_some() || self.channel_name.is_empty() {
            return;
        }
        if let Some(name) =
            resolve_or_register_drop(&self.channel_name, ctx.channels, ctx.store, ctx.mqtt)
        {
            self.drop_channel(&name, ctx.channels);
        }
    }
}
