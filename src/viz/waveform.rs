use eframe::egui::{self, Color32};
use egui_plot::{Legend, Line, Plot, PlotPoints, VLine};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{SampleType, TimeWindow};
use crate::viz::common::{
    bind, binding_error, opt_bool, opt_f64, req_str_array, snapshot_to_f64, Binding,
};
use crate::viz::decimate::decimate_minmax;
use crate::viz::measure::{stats, Stats};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "waveform";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int, SampleType::Bool];

/// Points fed to egui_plot per channel; ~2 per horizontal pixel is plenty.
const MAX_PLOT_BUCKETS: usize = 1000;

/// Scrolling time-series plot with optional measurement cursors.
pub struct WaveformPanel {
    title: String,
    bound: Vec<Binding>,
    time_window_s: f64,
    cursors: bool,
    /// Cursor positions in absolute ns so they stay put while the plot scrolls.
    cursor_a_ns: Option<i64>,
    cursor_b_ns: Option<i64>,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let names = req_str_array(cfg, "channels", TYPE_NAME)?;
    let bound: Vec<Binding> = names.iter().map(|n| bind(n, reg, ACCEPTED)).collect();
    Ok(Box::new(WaveformPanel {
        title: names.join(", "),
        bound,
        time_window_s: opt_f64(cfg, "time_window_s", 5.0),
        cursors: opt_bool(cfg, "cursors", false),
        cursor_a_ns: None,
        cursor_b_ns: None,
    }))
}

/// Stats over samples with lo <= ts <= hi (both cursors inclusive).
pub(crate) fn selection_stats(ts: &[i64], vals: &[f64], lo: i64, hi: i64) -> Option<Stats> {
    let sel: Vec<f64> = ts
        .iter()
        .zip(vals)
        .filter(|(&t, _)| t >= lo && t <= hi)
        .map(|(_, &v)| v)
        .collect();
    stats(&sel)
}

impl VizPanel for WaveformPanel {
    fn title(&self) -> &str {
        &self.title
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("window [s]:");
            ui.add(egui::Slider::new(&mut self.time_window_s, 0.1..=60.0).logarithmic(true));
            ui.checkbox(&mut self.cursors, "cursors");
            if ui.button("clear cursors").clicked() {
                self.cursor_a_ns = None;
                self.cursor_b_ns = None;
            }
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        for b in &self.bound {
            binding_error(ui, b, TYPE_NAME);
        }
        let end_ns = self
            .bound
            .iter()
            .filter(|b| b.type_ok)
            .filter_map(|b| b.id)
            .filter_map(|id| store.latest(id))
            .map(|(t, _)| t)
            .max();
        let Some(end_ns) = end_ns else {
            ui.label("no data");
            return;
        };
        let span_ns = (self.time_window_s * 1e9) as i64;
        let t0 = end_ns - span_ns;
        let window = TimeWindow { start_ns: t0, end_ns: end_ns + 1 };

        // Snapshots kept for the stats table below the plot.
        let mut snaps: Vec<(usize, Vec<i64>, Vec<f64>)> = Vec::new();
        for (i, b) in self.bound.iter().enumerate() {
            let (Some(id), true) = (b.id, b.type_ok) else { continue };
            let snap = store.snapshot(id, window);
            if let Some((ts, vals)) = snapshot_to_f64(&snap) {
                snaps.push((i, ts.to_vec(), vals));
            }
        }

        let plot = Plot::new(("waveform", &self.title))
            .legend(Legend::default())
            .include_x(0.0)
            .include_x(self.time_window_s);
        let inner = plot.show(ui, |plot_ui| {
            for (i, ts, vals) in &snaps {
                let points = decimate_minmax(ts, vals, t0, MAX_PLOT_BUCKETS);
                let b = &self.bound[*i];
                plot_ui.line(
                    Line::new(PlotPoints::from(points))
                        .color(b.color)
                        .name(&b.name),
                );
            }
            if self.cursors {
                if let Some(a) = self.cursor_a_ns {
                    plot_ui.vline(VLine::new((a - t0) as f64 / 1e9).color(Color32::YELLOW));
                }
                if let Some(b) = self.cursor_b_ns {
                    plot_ui.vline(VLine::new((b - t0) as f64 / 1e9).color(Color32::LIGHT_BLUE));
                }
            }
            plot_ui.pointer_coordinate()
        });
        if self.cursors && inner.response.clicked() {
            if let Some(p) = inner.inner {
                let ts = t0 + (p.x * 1e9) as i64;
                let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
                if ctrl {
                    self.cursor_b_ns = Some(ts);
                } else {
                    self.cursor_a_ns = Some(ts);
                }
            }
        }

        if self.cursors {
            ui.label("click: cursor A — ctrl+click: cursor B");
            if let (Some(a), Some(b)) = (self.cursor_a_ns, self.cursor_b_ns) {
                let (lo, hi) = (a.min(b), a.max(b));
                ui.label(format!("selection: {:.4} s", (hi - lo) as f64 / 1e9));
                egui::Grid::new(("wf-stats", &self.title))
                    .striped(true)
                    .show(ui, |ui| {
                        for h in ["channel", "min", "max", "mean", "rms", "n"] {
                            ui.strong(h);
                        }
                        ui.end_row();
                        for (i, ts, vals) in &snaps {
                            if let Some(s) = selection_stats(ts, vals, lo, hi) {
                                ui.label(&self.bound[*i].name);
                                ui.label(format!("{:.4}", s.min));
                                ui.label(format!("{:.4}", s.max));
                                ui.label(format!("{:.4}", s.mean));
                                ui.label(format!("{:.4}", s.rms));
                                ui.label(s.count.to_string());
                                ui.end_row();
                            }
                        }
                    });
            }
        }
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channels".to_string(),
            toml::Value::Array(
                self.bound
                    .iter()
                    .map(|b| toml::Value::String(b.name.clone()))
                    .collect(),
            ),
        );
        t.insert("time_window_s".to_string(), toml::Value::Float(self.time_window_s));
        t.insert("cursors".to_string(), toml::Value::Boolean(self.cursors));
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
max_rate = 1000
history_s = 10.0

[channels."demo.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap()
    }

    fn entry(src: &str) -> PanelEntry {
        toml::from_str(src).unwrap()
    }

    #[test]
    fn builds_and_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e = entry(
            r#"type = "waveform"
channels = ["demo.sine"]
time_window_s = 5.0
cursors = true"#,
        );
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.title(), "demo.sine");
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn missing_channels_key_is_err() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e = entry(r#"type = "waveform""#);
        assert!(reg.build(&e, &channels).is_err());
    }

    #[test]
    fn defaults_applied() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e = entry(
            r#"type = "waveform"
channels = ["demo.sine"]"#,
        );
        let p = reg.build(&e, &channels).unwrap();
        let cfg = p.serialize();
        assert_eq!(cfg["time_window_s"], toml::Value::Float(5.0));
        assert_eq!(cfg["cursors"], toml::Value::Boolean(false));
    }

    #[test]
    fn selection_stats_over_range() {
        let ts = [0i64, 10, 20, 30, 40];
        let vals = [1.0, 2.0, 3.0, 4.0, 5.0];
        let s = selection_stats(&ts, &vals, 10, 30).unwrap();
        assert_eq!(s.min, 2.0);
        assert_eq!(s.max, 4.0);
        assert_eq!(s.count, 3); // ts 10, 20, 30 inclusive
        assert!(selection_stats(&ts, &vals, 100, 200).is_none());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let sine = channels.id("demo.sine").unwrap();
        for i in 0..100i64 {
            store.write_numeric(sine, i * 1_000_000, NumericVal::Float((i as f64 * 0.1).sin()));
        }
        let reg = PanelRegistry::with_builtins();
        let sources = [
            r#"type = "waveform"
channels = ["demo.sine"]
cursors = true"#,
            r#"type = "waveform"
channels = ["does.not.exist"]"#,
            r#"type = "waveform"
channels = ["demo.log"]"#,
            r#"type = "waveform"
channels = ["demo.sine", "demo.log", "does.not.exist"]"#,
        ];
        for src in sources {
            let mut p = reg.build(&entry(src), &channels).unwrap();
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
