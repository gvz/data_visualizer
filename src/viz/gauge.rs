use eframe::egui::{self, Color32, FontId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::SampleType;
use crate::viz::common::{
    bind, binding_error, is_light, label_config_row, opt_f64, opt_label, opt_str, outlined_text,
    refresh_binding, sample_as_f64, serialize_label, Binding, ColorThresholds, RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "gauge";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// Horizontal bar gauge showing the latest value within [min, max].
pub struct GaugePanel {
    bound: Binding,
    label: Option<String>,
    min: f64,
    max: f64,
    /// Color cutoffs applied to the bar fill.
    colors: ColorThresholds,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = opt_str(cfg, "channel");
    let min = opt_f64(cfg, "min", 0.0);
    let max = opt_f64(cfg, "max", 100.0);
    if max <= min {
        anyhow::bail!("{TYPE_NAME} panel: max ({max}) must be greater than min ({min})");
    }
    Ok(Box::new(GaugePanel {
        bound: bind(&name, reg, ACCEPTED),
        label: opt_label(cfg),
        min,
        max,
        colors: ColorThresholds::from_config(cfg),
    }))
}

pub(crate) fn fraction(v: f64, min: f64, max: f64) -> f32 {
    (((v - min) / (max - min)).clamp(0.0, 1.0)) as f32
}

impl VizPanel for GaugePanel {
    fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.bound.name)
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        label_config_row(ui, &mut self.label, &self.bound.name);
        ui.horizontal(|ui| {
            ui.label("min:");
            ui.add(egui::DragValue::new(&mut self.min).speed(0.1));
            ui.label("max:");
            ui.add(egui::DragValue::new(&mut self.max).speed(0.1));
            if self.max <= self.min {
                self.max = self.min + 1.0;
            }
        });
        ui.separator();
        self.colors.config_ui(ui);
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if self.bound.name.is_empty() {
            ui.label(egui::RichText::new("Drop a channel here").weak());
            return;
        }
        if binding_error(ui, &self.bound, TYPE_NAME) {
            return;
        }
        let id = self.bound.id.expect("checked by binding_error");
        // In sync mode read the value at the shared zoom window's end.
        let at = crate::viz::common::linked_window(ui.ctx())
            .map(|(_, end)| end)
            .unwrap_or_else(|| store.now_ns());
        let value = store.latest_at(id, at).and_then(|(_, s)| sample_as_f64(&s));
        let desired = egui::vec2(ui.available_width().max(80.0), 32.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, Color32::from_gray(40));
        // Bar color follows the thresholds; below all of them it falls back to
        // the base color, then the channel's binding color.
        let bar_color = value
            .and_then(|v| self.colors.color_for(v))
            .unwrap_or_else(|| crate::viz::common::binding_color(&self.bound, 0));
        let text = match value {
            Some(v) => {
                let mut fill = rect;
                fill.set_width(rect.width() * fraction(v, self.min, self.max));
                painter.rect_filled(fill, 4.0, bar_color);
                format!("{v:.3} {}", self.bound.unit)
            }
            None => "—".to_string(),
        };
        // Keep the readout legible over any fill: pick black on light bars,
        // white on dark ones, and outline with the opposite so the part of the
        // text over the unfilled track stays readable too.
        let (fg, outline) = if is_light(bar_color) {
            (Color32::BLACK, Color32::WHITE)
        } else {
            (Color32::WHITE, Color32::BLACK)
        };
        outlined_text(
            painter,
            rect.center(),
            &text,
            FontId::proportional(16.0),
            fg,
            outline,
        );
        ui.horizontal(|ui| {
            ui.label(format!("{:.1}", self.min));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{:.1}", self.max));
            });
        });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        t.insert("min".to_string(), toml::Value::Float(self.min));
        t.insert("max".to_string(), toml::Value::Float(self.max));
        serialize_label(&mut t, &self.label);
        self.colors.write_config(&mut t);
        t
    }

    fn drop_channel(&mut self, name: &str, reg: &crate::config::ChannelRegistry) {
        self.bound = bind(name, reg, ACCEPTED);
    }

    fn refresh_bindings(&mut self, ctx: &RebindCtx) {
        refresh_binding(&mut self.bound, ACCEPTED, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;
    use eframe::egui;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
unit = "V"
"#,
        )
        .unwrap()
    }

    #[test]
    fn fraction_clamps() {
        assert_eq!(fraction(5.0, 0.0, 10.0), 0.5);
        assert_eq!(fraction(-1.0, 0.0, 10.0), 0.0);
        assert_eq!(fraction(11.0, 0.0, 10.0), 1.0);
        assert_eq!(fraction(0.0, -10.0, 10.0), 0.5);
    }

    #[test]
    fn light_bars_get_dark_text() {
        assert!(is_light(Color32::YELLOW));
        assert!(is_light(Color32::WHITE));
        assert!(!is_light(Color32::from_rgb(0xd6, 0x27, 0x28))); // palette red
        assert!(!is_light(Color32::BLACK));
    }

    #[test]
    fn thresholds_round_trip_through_config() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r##"type = "gauge"
channel = "demo.sine"
min = 0.0
max = 100.0

[[thresholds]]
value = 90.0
color = "#ff0000""##,
        )
        .unwrap();
        let out = reg.build(&e, &channels).unwrap().serialize();
        let arr = out.get("thresholds").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr[0].as_table().unwrap().get("color").and_then(|v| v.as_str()), Some("#ff0000"));
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "gauge"
channel = "demo.sine"
min = -10.0
max = 10.0"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn invalid_range_is_err() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "gauge"
channel = "demo.sine"
min = 5.0
max = 5.0"#,
        )
        .unwrap();
        assert!(reg.build(&e, &channels).is_err());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        store.write_numeric(id, 1, NumericVal::Float(3.0));
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "gauge"
channel = "demo.sine""#,
        )
        .unwrap();
        let mut p = reg.build(&e, &channels).unwrap();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
                p.config_ui(ui);
            });
        });
    }
}
