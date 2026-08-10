//! Iced-port spectrum panel — FFT magnitude line plot over frequency.
//!
//! A single channel is analysed with a Hann-windowed FFT each `sync` call.
//! The magnitude bins are pushed as `[freq_hz, magnitude_db]` into a
//! `line_only` series in the stateful `PlotWidget`. Both axes autoscale;
//! no fixed X limit is set (frequency range follows the bin count / sample
//! rate, which is estimated from the channel timestamps each `sync`).

use iced_plot::{Color, LineStyle, PlotWidget, PlotWidgetBuilder, Series, ShapeId};
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::{PanelMessage, VizPanel};

pub const TYPE_NAME: &str = "spectrum";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// Default FFT size (power of two, clamped to [64, 65536]).
const DEFAULT_FFT_SIZE: usize = 1024;

/// Default history window for sample collection (seconds).
const DEFAULT_HISTORY_S: f64 = 10.0;

// ---------------------------------------------------------------------------
// Pure FFT computation — lifted unchanged from the old egui panel.
// ---------------------------------------------------------------------------

/// Periodic Hann window, peak 1.0 at n/2.
pub(crate) fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 * (1.0 - (std::f64::consts::TAU * i as f64 / n as f64).cos()))
        .collect()
}

/// Sample rate from the median inter-sample gap. Second value is false when
/// any gap deviates from the median by more than 10% (non-uniform sampling).
/// Returns `None` when fewer than two timestamps are provided.
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

/// Compute `(freq_hz, magnitude_db)` for bins 0..n/2.
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

// ---------------------------------------------------------------------------
// Snapshot → (timestamps, f64 values) helper
// ---------------------------------------------------------------------------

fn snapshot_to_f64(snap: &ChannelSnapshot) -> Option<(Vec<i64>, Vec<f64>)> {
    match snap {
        ChannelSnapshot::Float { ts, vals } => Some((ts.clone(), vals.clone())),
        ChannelSnapshot::Int { ts, vals } => {
            Some((ts.clone(), vals.iter().map(|&v| v as f64).collect()))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Spectrum panel
// ---------------------------------------------------------------------------

/// Spectrum panel: Hann-windowed FFT magnitude of a single channel, drawn as
/// a line plot over frequency (Hz).
pub struct Spectrum {
    /// Configured channel name.
    channel_name: String,
    /// Resolved channel id; `None` if name was empty or unknown at construction.
    channel_id: Option<ChannelId>,
    /// FFT size (power of two, 64–65536).
    fft_size: usize,
    /// Whether to apply a Hann window before the FFT.
    hann_window: bool,
    /// History window for collecting samples (seconds).
    history_s: f64,
    /// Series id for the single magnitude line.
    shape_id: ShapeId,
    /// Stateful plot widget — never rebuilt in view().
    widget: PlotWidget,
}

impl Spectrum {
    pub fn new(cfg: &toml::Table, ch: &ChannelRegistry) -> Self {
        let channel_name = cfg
            .get("channel")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let fft_size = cfg
            .get("fft_size")
            .and_then(|v| v.as_integer())
            .map(|n| (n.max(1) as usize).next_power_of_two().clamp(64, 65_536))
            .unwrap_or(DEFAULT_FFT_SIZE);

        let hann_window = match cfg.get("window").and_then(|v| v.as_str()) {
            None | Some("hann") => true,
            Some("none") => false,
            Some(other) => {
                eprintln!("spectrum: unknown window `{other}`; defaulting to hann");
                true
            }
        };

        let history_s = cfg
            .get("history_s")
            .and_then(|v| v.as_float())
            .unwrap_or(DEFAULT_HISTORY_S);

        let channel_id = if channel_name.is_empty() {
            None
        } else {
            ch.id(&channel_name)
                .filter(|&id| ACCEPTED.contains(&ch.meta(id).sample_type))
        };

        // Blue (Tableau-10 slot 0) for the single FFT line.
        let series = Series::line_only(vec![[0.0, 0.0]], LineStyle::solid())
            .with_color(Color::from_rgb(
                0x1f as f32 / 255.0,
                0x77 as f32 / 255.0,
                0xb4 as f32 / 255.0,
            ))
            .with_label(channel_name.clone());
        let shape_id = series.id;

        let mut widget = match PlotWidgetBuilder::new()
            .with_autoscale_on_updates(true)
            .add_series(series)
            .build()
        {
            Ok(w) => w,
            Err(e) => {
                eprintln!("spectrum: PlotWidgetBuilder::build failed ({e:?}); using empty plot");
                PlotWidget::new()
            }
        };
        widget.autoscale_on_updates(true);

        Self { channel_name, channel_id, fft_size, hann_window, history_s, shape_id, widget }
    }
}

impl VizPanel for Spectrum {
    fn title(&self) -> String {
        if self.channel_name.is_empty() {
            "Spectrum".to_string()
        } else {
            format!("Spectrum: {}", self.channel_name)
        }
    }

    fn sync(&mut self, store: &dyn ChannelStore) {
        let id = match self.channel_id {
            Some(id) => id,
            None => return, // no channel bound — leave series at sentinel
        };

        let now_ns = store.now_ns();
        let history_ns = (self.history_s * 1e9) as i64;
        let start_ns = now_ns - history_ns;

        let snap = store.snapshot(id, TimeWindow { start_ns, end_ns: now_ns });
        let Some((ts, vals)) = snapshot_to_f64(&snap) else {
            return;
        };

        if vals.len() < self.fft_size {
            // Not enough samples yet — leave the series as-is.
            return;
        }

        let tail_ts = &ts[ts.len() - self.fft_size..];
        let tail = &vals[vals.len() - self.fft_size..];

        let sample_rate = match estimate_rate(tail_ts) {
            Some((rate, _)) => rate,
            None => return,
        };

        let win = if self.hann_window { hann(self.fft_size) } else { vec![1.0; self.fft_size] };
        let bins = spectrum_db(tail, &win, sample_rate);

        let sid = self.shape_id;
        self.widget
            .update_series(&sid, |s| {
                s.positions.clear();
                s.positions.extend_from_slice(&bins);
            })
            .ok();
        // X autoscales to the bin range — no set_x_lim call (frequency axis).
    }

    fn view(&self) -> iced::Element<'_, PanelMessage> {
        self.widget.view().map(PanelMessage::Plot)
    }

    fn update(&mut self, msg: PanelMessage) {
        if let PanelMessage::Plot(m) = msg {
            self.widget.update(m);
        }
    }

    fn config_view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, text};
        let label = if self.channel_name.is_empty() {
            "No channel bound.".to_string()
        } else {
            format!(
                "Channel: {}  |  FFT: {}pt  |  Window: {}",
                self.channel_name,
                self.fft_size,
                if self.hann_window { "hann" } else { "none" }
            )
        };
        container(text(label)).into()
    }

    fn bind_channel(&mut self, _name: &str) -> bool {
        // Single-channel panel; binding is set at construction via config.
        false
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.channel_name.clone()));
        t.insert("fft_size".to_string(), toml::Value::Integer(self.fft_size as i64));
        t.insert(
            "window".to_string(),
            toml::Value::String(if self.hann_window { "hann" } else { "none" }.to_string()),
        );
        t
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
mqtt_topic = "sensors/demo.sine"
type = "float"
max_rate = 10000
history_s = 2.0
"#,
        )
        .unwrap()
    }

    // -----------------------------------------------------------------------
    // FFT unit tests (ported unchanged from the old egui panel)
    // -----------------------------------------------------------------------

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

    /// Required by the task brief: spectrum_db produces a non-empty magnitude
    /// vector from a known sine input.
    #[test]
    fn spectrum_produces_nonempty_magnitude_from_sine() {
        let rate = 4096.0_f64;
        let n = 512_usize;
        let samples: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * 100.0 * i as f64 / rate).sin())
            .collect();
        let bins = spectrum_db(&samples, &hann(n), rate);
        assert!(!bins.is_empty(), "magnitude vector must be non-empty");
        assert_eq!(bins.len(), n / 2);
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

    // -----------------------------------------------------------------------
    // Panel-level tests
    // -----------------------------------------------------------------------

    #[test]
    fn spectrum_constructs_from_channel_a() {
        let ch = ChannelRegistry::from_toml_str(
            r#"
[channels."a"]
mqtt_topic = "sensors/a"
type = "float"
"#,
        )
        .unwrap();
        let mut cfg = toml::Table::new();
        cfg.insert("channel".into(), toml::Value::String("a".into()));
        let s = Spectrum::new(&cfg, &ch);
        assert_eq!(s.channel_name, "a");
        assert!(s.channel_id.is_some());
    }

    #[test]
    fn spectrum_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "spectrum"
channel = "demo.sine"
fft_size = 1000
window = "hann""#,
        )
        .unwrap();
        let p = reg.build(&e, &channels);
        let cfg = p.serialize();
        assert_eq!(cfg["channel"], toml::Value::String("demo.sine".into()));
        assert_eq!(cfg["fft_size"], toml::Value::Integer(1024)); // clamped to pow2
        assert_eq!(cfg["window"], toml::Value::String("hann".into()));
    }

    #[test]
    fn spectrum_sync_with_live_data_no_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        // Write 2048 sine samples at 10 kHz (100 µs spacing).
        for i in 0..2048i64 {
            store.write_numeric(id, i * 100_000, NumericVal::Float((i as f64 * 0.3).sin()));
        }
        let mut cfg = toml::Table::new();
        cfg.insert("channel".into(), toml::Value::String("demo.sine".into()));
        let mut s = Spectrum::new(&cfg, &channels);
        // Must not panic.
        s.sync(&store);
    }
}
