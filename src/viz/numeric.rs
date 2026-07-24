use eframe::egui;

use crate::config::ChannelRegistry;
use crate::dynamic_channel::resolve_or_register_drop;
use crate::store::ChannelStore;
use crate::types::{ChannelId, Sample, SampleType};
use crate::viz::common::{
    label_config_row, opt_label, opt_str, sample_as_f64, serialize_label, ColorThresholds,
    RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "numeric";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int, SampleType::Bool];

/// Large latest-value display with unit label.
pub struct NumericPanel {
    channel_name: String,
    label: Option<String>,
    channel: Option<ChannelId>,
    type_ok: bool,
    /// Unit from the channel's metadata; shown when no override is set.
    unit: String,
    /// Per-panel unit suffix override; when non-empty it replaces `unit`.
    unit_override: String,
    /// Color cutoffs applied to the readout text.
    colors: ColorThresholds,
}

impl NumericPanel {
    /// The suffix shown after the value: the override if set, else channel meta.
    fn effective_unit(&self) -> &str {
        if self.unit_override.is_empty() {
            &self.unit
        } else {
            &self.unit_override
        }
    }
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
    Ok(Box::new(NumericPanel {
        channel_name,
        label: opt_label(cfg),
        channel,
        type_ok,
        unit,
        unit_override: opt_str(cfg, "unit"),
        colors: ColorThresholds::from_config(cfg),
    }))
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
        ui.horizontal(|ui| {
            ui.label("unit:");
            let hint = if self.unit.is_empty() { "e.g. V" } else { &self.unit };
            ui.add(
                egui::TextEdit::singleline(&mut self.unit_override)
                    .desired_width(80.0)
                    .hint_text(hint),
            );
        });
        ui.separator();
        self.colors.config_ui(ui);
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
        // In sync mode the shared zoom window's end selects which sample to
        // show, so the readout matches the zoomed waveform's right edge.
        let at = crate::viz::common::linked_window(ui.ctx())
            .map(|(_, end)| end)
            .unwrap_or_else(|| store.now_ns());
        let sample = store.latest_at(id, at);
        let text = match &sample {
            Some((_, Sample::Float(v))) => format!("{v:.3}"),
            Some((_, Sample::Int(v))) => v.to_string(),
            Some((_, Sample::Bool(b))) => if *b { "ON" } else { "OFF" }.to_string(),
            Some((_, Sample::Text(_))) | None => "\u{2014}".to_string(),
        };
        let mut rich =
            egui::RichText::new(format!("{text} {}", self.effective_unit())).size(32.0);
        if let Some(v) = sample.as_ref().and_then(|(_, s)| sample_as_f64(s)) {
            if let Some(color) = self.colors.color_for(v) {
                rich = rich.color(color);
            }
        }
        ui.label(rich);
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channel".to_string(),
            toml::Value::String(self.channel_name.clone()),
        );
        serialize_label(&mut t, &self.label);
        if !self.unit_override.is_empty() {
            t.insert("unit".to_string(), toml::Value::String(self.unit_override.clone()));
        }
        self.colors.write_config(&mut t);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(unit: &str, unit_override: &str) -> NumericPanel {
        NumericPanel {
            channel_name: "x".into(),
            label: None,
            channel: None,
            type_ok: true,
            unit: unit.into(),
            unit_override: unit_override.into(),
            colors: ColorThresholds::default(),
        }
    }

    #[test]
    fn override_takes_precedence_over_meta_unit() {
        assert_eq!(panel("V", "kV").effective_unit(), "kV");
    }

    #[test]
    fn empty_override_falls_back_to_meta_unit() {
        assert_eq!(panel("V", "").effective_unit(), "V");
    }

    #[test]
    fn unit_override_round_trips_through_config() {
        let reg = ChannelRegistry::from_toml_str(
            "[channels.\"x\"]\nmqtt_topic = \"t\"\ntype = \"float\"\n",
        )
        .unwrap();
        let mut cfg = toml::Table::new();
        cfg.insert("channel".into(), toml::Value::String("x".into()));
        cfg.insert("unit".into(), toml::Value::String("psi".into()));
        let out = ctor(&cfg, &reg).unwrap().serialize();
        assert_eq!(out.get("unit").and_then(|v| v.as_str()), Some("psi"));
    }

    #[test]
    fn thresholds_round_trip_through_config() {
        let reg = ChannelRegistry::from_toml_str(
            "[channels.\"x\"]\nmqtt_topic = \"t\"\ntype = \"float\"\n",
        )
        .unwrap();
        let mut cfg = toml::Table::new();
        cfg.insert("channel".into(), toml::Value::String("x".into()));
        let mut th = toml::Table::new();
        th.insert("value".into(), toml::Value::Float(90.0));
        th.insert("color".into(), toml::Value::String("#ff0000".into()));
        cfg.insert("thresholds".into(), toml::Value::Array(vec![toml::Value::Table(th)]));
        let out = ctor(&cfg, &reg).unwrap().serialize();
        let arr = out.get("thresholds").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr.len(), 1);
        let t0 = arr[0].as_table().unwrap();
        assert_eq!(t0.get("value").and_then(|v| v.as_float()), Some(90.0));
        assert_eq!(t0.get("color").and_then(|v| v.as_str()), Some("#ff0000"));
    }

    #[test]
    fn base_color_round_trips_through_config() {
        let reg = ChannelRegistry::from_toml_str(
            "[channels.\"x\"]\nmqtt_topic = \"t\"\ntype = \"float\"\n",
        )
        .unwrap();
        let mut cfg = toml::Table::new();
        cfg.insert("channel".into(), toml::Value::String("x".into()));
        cfg.insert("base_color".into(), toml::Value::String("#00ff00".into()));
        let out = ctor(&cfg, &reg).unwrap().serialize();
        assert_eq!(out.get("base_color").and_then(|v| v.as_str()), Some("#00ff00"));
    }

    #[test]
    fn no_override_is_not_serialized() {
        let reg = ChannelRegistry::from_toml_str(
            "[channels.\"x\"]\nmqtt_topic = \"t\"\ntype = \"float\"\n",
        )
        .unwrap();
        let mut cfg = toml::Table::new();
        cfg.insert("channel".into(), toml::Value::String("x".into()));
        let out = ctor(&cfg, &reg).unwrap().serialize();
        assert!(out.get("unit").is_none());
    }
}
