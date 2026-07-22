use eframe::egui;
use egui_plot::{Plot, PlotPoints, Points};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{SampleType, TimeWindow};
use crate::viz::common::{
    bind, binding_error, effective_window_s, label_config_row, opt_f64_opt, opt_label, opt_str,
    refresh_binding, serialize_label, snapshot_to_f64, window_config_row, Binding, RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "xy_scatter";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// Two channels plotted against each other, aligned by sample index from the
/// newest sample backwards (uniform-rate assumption; no interpolation in v1).
pub struct XyScatterPanel {
    title: String,
    label: Option<String>,
    x: Binding,
    y: Binding,
    /// Visible span in seconds; `None` follows the global default.
    time_window_s: Option<f64>,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let xn = opt_str(cfg, "x_channel");
    let yn = opt_str(cfg, "y_channel");
    Ok(Box::new(XyScatterPanel {
        title: if xn.is_empty() && yn.is_empty() { String::new() } else { format!("{xn} / {yn}") },
        label: opt_label(cfg),
        x: bind(&xn, reg, ACCEPTED),
        y: bind(&yn, reg, ACCEPTED),
        time_window_s: opt_f64_opt(cfg, "time_window_s"),
    }))
}

/// Pair up the newest min(len) samples of both series.
pub(crate) fn index_align(x: &[f64], y: &[f64]) -> Vec<[f64; 2]> {
    let n = x.len().min(y.len());
    x[x.len() - n..]
        .iter()
        .zip(&y[y.len() - n..])
        .map(|(&a, &b)| [a, b])
        .collect()
}

impl VizPanel for XyScatterPanel {
    fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.title)
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        label_config_row(ui, &mut self.label, &self.title);
        ui.horizontal(|ui| {
            window_config_row(ui, &mut self.time_window_s, 0.05..=10.0);
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if self.x.name.is_empty() || self.y.name.is_empty() {
            ui.label(egui::RichText::new("Drop two channels here (x then y)").weak());
            return;
        }
        let xe = binding_error(ui, &self.x, TYPE_NAME);
        let ye = binding_error(ui, &self.y, TYPE_NAME);
        if xe || ye {
            return;
        }
        let (xid, yid) = (self.x.id.unwrap(), self.y.id.unwrap());
        if store.latest(xid).is_none() || store.latest(yid).is_none() {
            ui.label("no data");
            return;
        }
        // Anchor on the store clock so the live scrub slider / replay position
        // pick which window is paired, not always the newest samples.
        let end_ns = store.now_ns();
        let span = (effective_window_s(ui.ctx(), self.time_window_s) * 1e9) as i64;
        let window = TimeWindow { start_ns: end_ns - span, end_ns: end_ns + 1 };
        let xs = store.snapshot(xid, window);
        let ys = store.snapshot(yid, window);
        let (Some((_, xv)), Some((_, yv))) = (snapshot_to_f64(&xs), snapshot_to_f64(&ys)) else {
            return;
        };
        let pts = index_align(&xv, &yv);
        Plot::new(("xy", &self.title))
            .data_aspect(1.0)
            .show(ui, |plot_ui| {
                plot_ui.points(
                    Points::new(PlotPoints::from(pts))
                        .radius(1.5_f32)
                        .color(crate::viz::common::binding_color(&self.y, 0)),
                );
            });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("x_channel".to_string(), toml::Value::String(self.x.name.clone()));
        t.insert("y_channel".to_string(), toml::Value::String(self.y.name.clone()));
        if let Some(w) = self.time_window_s {
            t.insert("time_window_s".to_string(), toml::Value::Float(w));
        }
        serialize_label(&mut t, &self.label);
        t
    }

    fn drop_channel(&mut self, name: &str, reg: &crate::config::ChannelRegistry) {
        // Shift: new → x, old x → y; title follows.
        let new_x = bind(name, reg, ACCEPTED);
        self.y = std::mem::replace(&mut self.x, new_x);
        self.title = format!("{} / {}", self.x.name, self.y.name);
    }

    fn refresh_bindings(&mut self, ctx: &RebindCtx) {
        refresh_binding(&mut self.x, ACCEPTED, ctx);
        refresh_binding(&mut self.y, ACCEPTED, ctx);
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

[channels."demo.counter"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"
"#,
        )
        .unwrap()
    }

    #[test]
    fn index_align_takes_tails() {
        // Different lengths: align from the newest sample backwards.
        let x = [1.0, 2.0, 3.0, 4.0];
        let y = [10.0, 20.0];
        assert_eq!(index_align(&x, &y), vec![[3.0, 10.0], [4.0, 20.0]]);
        assert!(index_align(&[], &y).is_empty());
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "xy_scatter"
x_channel = "demo.sine"
y_channel = "demo.counter"
time_window_s = 2.0"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
        assert_eq!(p.title(), "demo.sine / demo.counter");
    }

    #[test]
    fn missing_channel_key_builds_empty_panel() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(r#"type = "xy_scatter""#).unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.title(), "");
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let x = channels.id("demo.sine").unwrap();
        let y = channels.id("demo.counter").unwrap();
        for i in 0..50i64 {
            store.write_numeric(x, i * 1_000_000, NumericVal::Float((i as f64).sin()));
            store.write_numeric(y, i * 1_000_000, NumericVal::Int(i));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "xy_scatter"
x_channel = "demo.sine"
y_channel = "demo.counter""#,
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
