use eframe::egui::{self, Color32};
use egui_plot::{Line, Plot, PlotPoints};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{SampleType, TimeWindow};
use crate::viz::common::{
    bind, binding_error, label_config_row, opt_i64, opt_label, opt_str, refresh_binding,
    serialize_label, snapshot_to_f64, Binding, RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "spectrum";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// FFT of the newest `fft_size` samples, drawn as magnitude in dB over Hz.
pub struct SpectrumPanel {
    bound: Binding,
    label: Option<String>,
    fft_size: usize,
    hann_window: bool,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = opt_str(cfg, "channel");
    let fft_size = (opt_i64(cfg, "fft_size", 1024).max(1) as usize)
        .next_power_of_two()
        .clamp(64, 65_536);
    let hann_window = match cfg.get("window").and_then(|v| v.as_str()) {
        None | Some("hann") => true,
        Some("none") => false,
        Some(other) => anyhow::bail!("{TYPE_NAME} panel: unknown window `{other}`"),
    };
    Ok(Box::new(SpectrumPanel {
        bound: bind(&name, reg, ACCEPTED),
        label: opt_label(cfg),
        fft_size,
        hann_window,
    }))
}

/// Periodic Hann window, peak 1.0 at n/2.
pub(crate) fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 * (1.0 - (std::f64::consts::TAU * i as f64 / n as f64).cos()))
        .collect()
}

/// (freq_hz, magnitude_db) for bins 0..n/2.
pub(crate) fn spectrum_db(samples: &[f64], window: &[f64], sample_rate: f64) -> Vec<[f64; 2]> {
    let n = samples.len();
    debug_assert_eq!(n, window.len());
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(n);
    let mut buf: Vec<Complex<f64>> = samples
        .iter()
        .zip(window)
        .map(|(&s, &w)| Complex::new(s * w, 0.0))
        .collect();
    fft.process(&mut buf);
    let scale = 2.0 / n as f64;
    (0..n / 2)
        .map(|i| {
            let mag = buf[i].norm() * scale;
            [
                i as f64 * sample_rate / n as f64,
                20.0 * mag.max(1e-12).log10(),
            ]
        })
        .collect()
}

/// Sample rate from the median inter-sample gap. Second value is false when
/// any gap deviates from the median by more than 10% (non-uniform sampling).
pub(crate) fn estimate_rate(ts: &[i64]) -> Option<(f64, bool)> {
    if ts.len() < 2 {
        return None;
    }
    let mut dts: Vec<i64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    dts.sort_unstable();
    let median = dts[dts.len() / 2];
    if median <= 0 {
        return None;
    }
    let tol = median / 10;
    let uniform = (median - dts[0]) <= tol && (dts[dts.len() - 1] - median) <= tol;
    Some((1e9 / median as f64, uniform))
}

impl VizPanel for SpectrumPanel {
    fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.bound.name)
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        label_config_row(ui, &mut self.label, &self.bound.name);
        ui.horizontal(|ui| {
            ui.label("fft size:");
            for size in [256usize, 1024, 4096, 16_384] {
                ui.selectable_value(&mut self.fft_size, size, size.to_string());
            }
            ui.checkbox(&mut self.hann_window, "hann window");
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
        // Anchor on the store clock so the live scrub slider / replay position
        // select which window is analyzed, not always the newest sample.
        let end_ns = store.now_ns();
        let snap = store.snapshot(id, TimeWindow { start_ns: i64::MIN, end_ns: end_ns + 1 });
        let Some((ts, vals)) = snapshot_to_f64(&snap) else {
            return;
        };
        if vals.len() < self.fft_size {
            ui.label(format!("collecting\u{2026} {}/{}", vals.len(), self.fft_size));
            return;
        }
        let tail_ts = &ts[ts.len() - self.fft_size..];
        let tail = &vals[vals.len() - self.fft_size..];
        let Some((rate, uniform)) = estimate_rate(tail_ts) else {
            ui.label("cannot estimate sample rate");
            return;
        };
        if !uniform {
            ui.colored_label(
                Color32::YELLOW,
                "warning: non-uniform sample timestamps \u{2014} spectrum may be distorted",
            );
        }
        let window = if self.hann_window {
            hann(self.fft_size)
        } else {
            vec![1.0; self.fft_size]
        };
        let bins = spectrum_db(tail, &window, rate);
        Plot::new(("spectrum", &self.bound.name))
            .x_axis_label("Hz")
            .y_axis_label("dB")
            .show(ui, |plot_ui| {
                plot_ui.line(
                    Line::new(PlotPoints::from(bins))
                        .color(crate::viz::common::binding_color(&self.bound, 0)),
                );
            });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        t.insert("fft_size".to_string(), toml::Value::Integer(self.fft_size as i64));
        t.insert(
            "window".to_string(),
            toml::Value::String(if self.hann_window { "hann" } else { "none" }.to_string()),
        );
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
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
max_rate = 10000
history_s = 2.0
"#,
        )
        .unwrap()
    }

    #[test]
    fn hann_window_shape() {
        let w = hann(8);
        assert_eq!(w.len(), 8);
        assert!(w[0].abs() < 1e-12); // starts at 0
        assert!((w[4] - 1.0).abs() < 1e-12); // peak at n/2
    }

    #[test]
    fn spectrum_peak_at_sine_frequency() {
        // 1000 Hz sine sampled at 8192 Hz, n=1024 → peak at bin 125.
        let rate = 8192.0;
        let n = 1024;
        let samples: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate).sin())
            .collect();
        let bins = spectrum_db(&samples, &hann(n), rate);
        assert_eq!(bins.len(), n / 2);
        let peak = bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1[1].total_cmp(&b.1[1]))
            .unwrap()
            .0;
        assert_eq!(peak, 125);
        assert!((bins[125][0] - 1000.0).abs() < 1.0); // freq axis in Hz
    }

    #[test]
    fn rate_estimation_and_uniformity() {
        let uniform: Vec<i64> = (0..100).map(|i| i * 1_000_000).collect(); // 1 kHz
        let (rate, ok) = estimate_rate(&uniform).unwrap();
        assert!((rate - 1000.0).abs() < 1e-6);
        assert!(ok);

        let mut jittered = uniform.clone();
        jittered[50] += 500_000; // 50% off
        let (_, ok) = estimate_rate(&jittered).unwrap();
        assert!(!ok);

        assert!(estimate_rate(&[42]).is_none());
    }

    #[test]
    fn builds_serializes_and_clamps() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "spectrum"
channel = "demo.sine"
fft_size = 1000
window = "hann""#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        let cfg = p.serialize();
        assert_eq!(cfg["channel"], toml::Value::String("demo.sine".into()));
        assert_eq!(cfg["fft_size"], toml::Value::Integer(1024)); // clamped up to pow2
        assert_eq!(cfg["window"], toml::Value::String("hann".into()));
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        for i in 0..2048i64 {
            store.write_numeric(id, i * 100_000, NumericVal::Float((i as f64 * 0.3).sin()));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "spectrum"
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
