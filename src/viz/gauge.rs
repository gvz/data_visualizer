use eframe::egui::{self, Align2, Color32, FontId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::SampleType;
use crate::viz::common::{bind, binding_error, opt_f64, req_str, sample_as_f64, Binding};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "gauge";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// Horizontal bar gauge showing the latest value within [min, max].
pub struct GaugePanel {
    bound: Binding,
    min: f64,
    max: f64,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = req_str(cfg, "channel", TYPE_NAME)?;
    let min = opt_f64(cfg, "min", 0.0);
    let max = opt_f64(cfg, "max", 100.0);
    if max <= min {
        anyhow::bail!("{TYPE_NAME} panel: max ({max}) must be greater than min ({min})");
    }
    Ok(Box::new(GaugePanel { bound: bind(&name, reg, ACCEPTED), min, max }))
}

pub(crate) fn fraction(v: f64, min: f64, max: f64) -> f32 {
    (((v - min) / (max - min)).clamp(0.0, 1.0)) as f32
}

impl VizPanel for GaugePanel {
    fn title(&self) -> &str {
        &self.bound.name
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("min:");
            ui.add(egui::DragValue::new(&mut self.min).speed(0.1));
            ui.label("max:");
            ui.add(egui::DragValue::new(&mut self.max).speed(0.1));
            if self.max <= self.min {
                self.max = self.min + 1.0;
            }
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if binding_error(ui, &self.bound, TYPE_NAME) {
            return;
        }
        let id = self.bound.id.expect("checked by binding_error");
        let value = store.latest(id).and_then(|(_, s)| sample_as_f64(&s));
        let desired = egui::vec2(ui.available_width().max(80.0), 32.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, Color32::from_gray(40));
        let text = match value {
            Some(v) => {
                let mut fill = rect;
                fill.set_width(rect.width() * fraction(v, self.min, self.max));
                painter.rect_filled(fill, 4.0, self.bound.color);
                format!("{v:.3} {}", self.bound.unit)
            }
            None => "—".to_string(),
        };
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(16.0),
            Color32::WHITE,
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
        t
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
