use std::collections::BTreeMap;

use eframe::egui::{self, Align2, Color32, FontId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::common::{
    bind, binding_error, effective_window_s, label_config_row, opt_f64_opt, opt_label, opt_str,
    refresh_binding, serialize_label, window_config_row, Binding, RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "state_graph";

const ACCEPTED: &[SampleType] = &[SampleType::Bool, SampleType::Int];

const PALETTE: &[Color32] = &[
    Color32::from_rgb(0x4c, 0xaf, 0x50), // green
    Color32::from_rgb(0x21, 0x96, 0xf3), // blue
    Color32::from_rgb(0xff, 0x98, 0x00), // orange
    Color32::from_rgb(0xf4, 0x43, 0x36), // red
    Color32::from_rgb(0x9c, 0x27, 0xb0), // purple
    Color32::from_rgb(0x00, 0xbc, 0xd4), // cyan
];

fn color_for(value: i64) -> Color32 {
    PALETTE[(value.rem_euclid(PALETTE.len() as i64)) as usize]
}

/// Grafana-style colored bands: one horizontal strip, one colored segment per
/// contiguous run of equal values.
pub struct StateGraphPanel {
    bound: Binding,
    label: Option<String>,
    states: BTreeMap<i64, String>,
    /// Visible span in seconds; `None` follows the global default.
    time_window_s: Option<f64>,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = opt_str(cfg, "channel");
    let mut states = BTreeMap::new();
    if let Some(tbl) = cfg.get("states").and_then(|v| v.as_table()) {
        for (k, v) in tbl {
            let key: i64 = k
                .parse()
                .map_err(|_| anyhow::anyhow!("{TYPE_NAME} panel: state key `{k}` is not an integer"))?;
            states.insert(key, v.as_str().unwrap_or_default().to_string());
        }
    }
    Ok(Box::new(StateGraphPanel {
        bound: bind(&name, reg, ACCEPTED),
        label: opt_label(cfg),
        states,
        time_window_s: opt_f64_opt(cfg, "time_window_s"),
    }))
}

/// Contiguous runs of equal values: (start_ts, end_ts, value). The final
/// segment ends at the last timestamp.
pub(crate) fn segments(ts: &[i64], vals: &[i64]) -> Vec<(i64, i64, i64)> {
    if ts.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let (mut start, mut cur) = (ts[0], vals[0]);
    for i in 1..ts.len() {
        if vals[i] != cur {
            out.push((start, ts[i], cur));
            start = ts[i];
            cur = vals[i];
        }
    }
    out.push((start, *ts.last().unwrap(), cur));
    out
}

impl StateGraphPanel {
    fn label_for(&self, value: i64) -> String {
        self.states
            .get(&value)
            .cloned()
            .unwrap_or_else(|| value.to_string())
    }
}

impl VizPanel for StateGraphPanel {
    fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.bound.name)
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        label_config_row(ui, &mut self.label, &self.bound.name);
        ui.horizontal(|ui| {
            window_config_row(ui, &mut self.time_window_s, 1.0..=600.0);
        });
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
        if store.latest(id).is_none() {
            ui.label("no data");
            return;
        }
        // Anchor the window on the store's clock so the live scrub slider (and
        // replay position) move the view rather than pinning to the newest sample.
        let end_ns = store.now_ns();
        let span = (effective_window_s(ui.ctx(), self.time_window_s) * 1e9) as i64;
        let t0 = end_ns - span;
        let snap = store.snapshot(id, TimeWindow { start_ns: t0, end_ns: end_ns + 1 });
        let (ts, vals): (Vec<i64>, Vec<i64>) = match &snap {
            ChannelSnapshot::Int { ts, vals } => (ts.clone(), vals.clone()),
            ChannelSnapshot::Bool { ts, vals } => {
                (ts.clone(), vals.iter().map(|&v| v as i64).collect())
            }
            _ => return,
        };
        let desired = egui::vec2(ui.available_width().max(80.0), 40.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, Color32::from_gray(30));
        let x_of = |t: i64| {
            rect.left() + rect.width() * ((t - t0) as f32 / span.max(1) as f32)
        };
        for (s, e, v) in segments(&ts, &vals) {
            // Extend the last segment to "now" (the right edge).
            let e = if e == *ts.last().unwrap() { end_ns } else { e };
            let seg = egui::Rect::from_min_max(
                egui::pos2(x_of(s), rect.top()),
                egui::pos2(x_of(e), rect.bottom()),
            );
            painter.rect_filled(seg, 0.0, color_for(v));
            if seg.width() > 40.0 {
                painter.text(
                    seg.center(),
                    Align2::CENTER_CENTER,
                    self.label_for(v),
                    FontId::proportional(12.0),
                    Color32::BLACK,
                );
            }
        }
        // Legend of known states.
        ui.horizontal_wrapped(|ui| {
            for (v, label) in &self.states {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().rect_filled(dot, 2.0, color_for(*v));
                ui.label(label);
            }
        });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        if !self.states.is_empty() {
            let mut tbl = toml::Table::new();
            for (k, v) in &self.states {
                tbl.insert(k.to_string(), toml::Value::String(v.clone()));
            }
            t.insert("states".to_string(), toml::Value::Table(tbl));
        }
        if let Some(w) = self.time_window_s {
            t.insert("time_window_s".to_string(), toml::Value::Float(w));
        }
        serialize_label(&mut t, &self.label);
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
[channels."motor.state"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"

[channels."demo.enabled"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "bool"
"#,
        )
        .unwrap()
    }

    #[test]
    fn segments_merge_consecutive_values() {
        let ts = [0i64, 1, 2, 3, 4];
        let vals = [0i64, 0, 1, 1, 0];
        assert_eq!(segments(&ts, &vals), vec![(0, 2, 0), (2, 4, 1), (4, 4, 0)]);
        assert!(segments(&[], &[]).is_empty());
        assert_eq!(segments(&[7], &[3]), vec![(7, 7, 3)]);
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "state_graph"
channel = "motor.state"
states = { 0 = "IDLE", 1 = "RUN", 2 = "FAULT" }
time_window_s = 30.0"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn bad_state_key_is_err() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "state_graph"
channel = "motor.state"
states = { abc = "IDLE" }"#,
        )
        .unwrap();
        assert!(reg.build(&e, &channels).is_err());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let motor = channels.id("motor.state").unwrap();
        let enabled = channels.id("demo.enabled").unwrap();
        for i in 0..50i64 {
            store.write_numeric(motor, i * 1_000_000, NumericVal::Int(i / 20));
            store.write_numeric(enabled, i * 1_000_000, NumericVal::Bool(i % 10 < 5));
        }
        let reg = PanelRegistry::with_builtins();
        for src in [
            r#"type = "state_graph"
channel = "motor.state"
states = { 0 = "IDLE", 1 = "RUN" }"#,
            r#"type = "state_graph"
channel = "demo.enabled""#,
        ] {
            let e: PanelEntry = toml::from_str(src).unwrap();
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
}
