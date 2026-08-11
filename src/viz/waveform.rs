//! Iced-port waveform panel — a scrolling time-series line plot.
//!
//! Each bound channel becomes one `iced_plot::Series` (line-only) tracked by
//! its `ShapeId`. On every `sync` call the full visible window is re-derived
//! from the store via `snapshot` and decimated to at most ~2 points per bucket
//! before pushing into the widget. `view` and `update` simply delegate to the
//! stateful `PlotWidget`.

use iced_plot::{Color, LineStyle, PlotWidget, PlotWidgetBuilder, Series, ShapeId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::decimate::decimate_minmax;
use crate::viz::{PanelMessage, VizPanel};

pub const TYPE_NAME: &str = "waveform";

/// Accepted sample types for waveform panels.
const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int, SampleType::Bool];

/// Max decimation buckets per sync — ~2 points per bucket is what the old
/// panel used (MAX_PLOT_BUCKETS = 1000 -> up to 2000 points).
const MAX_PLOT_BUCKETS: usize = 1000;

/// Default visible time window in seconds.
const DEFAULT_WINDOW_S: f64 = 10.0;

// ---------------------------------------------------------------------------
// Palette (Tableau 10, same values as viz/common.rs PALETTE)
// ---------------------------------------------------------------------------

const PALETTE: &[(f32, f32, f32)] = &[
    (0x1f as f32 / 255.0, 0x77 as f32 / 255.0, 0xb4 as f32 / 255.0), // blue
    (0xff as f32 / 255.0, 0x7f as f32 / 255.0, 0x0e as f32 / 255.0), // orange
    (0x2c as f32 / 255.0, 0xa0 as f32 / 255.0, 0x2c as f32 / 255.0), // green
    (0xd6 as f32 / 255.0, 0x27 as f32 / 255.0, 0x28 as f32 / 255.0), // red
    (0x94 as f32 / 255.0, 0x67 as f32 / 255.0, 0xbd as f32 / 255.0), // purple
    (0x8c as f32 / 255.0, 0x56 as f32 / 255.0, 0x4b as f32 / 255.0), // brown
    (0xe3 as f32 / 255.0, 0x77 as f32 / 255.0, 0xc2 as f32 / 255.0), // pink
    (0xbc as f32 / 255.0, 0xbd as f32 / 255.0, 0x22 as f32 / 255.0), // olive
    (0x17 as f32 / 255.0, 0xbe as f32 / 255.0, 0xcf as f32 / 255.0), // cyan
    (0x7f as f32 / 255.0, 0x7f as f32 / 255.0, 0x7f as f32 / 255.0), // gray
];

fn palette_color(i: usize) -> Color {
    let (r, g, b) = PALETTE[i % PALETTE.len()];
    Color::from_rgb(r, g, b)
}

// ---------------------------------------------------------------------------
// Channel slot: ties a channel name/id to its plot series.
// ---------------------------------------------------------------------------

struct ChannelSlot {
    /// Original configured name (what we serialize back).
    name: String,
    /// Resolved id; `None` if the channel name was unknown at construction.
    id: Option<ChannelId>,
    /// The series id in the plot widget.
    shape_id: ShapeId,
}

// ---------------------------------------------------------------------------
// Waveform
// ---------------------------------------------------------------------------

/// Scrolling waveform panel. Holds a stateful `PlotWidget` and one
/// `ChannelSlot` per bound channel.
pub struct Waveform {
    /// Channel slots in binding order.
    channels: Vec<ChannelSlot>,
    /// Visible time window in seconds.
    window_s: f64,
    /// Stateful GPU-backed plot widget — never rebuilt, only updated.
    widget: PlotWidget,
}

impl Waveform {
    /// Construct from a TOML config table and the channel registry.
    ///
    /// Reads `channels = ["name", ...]`. Unknown channel names are tolerated:
    /// the slot is kept with `id = None` and the series will have no data
    /// until the channel appears in the registry.
    pub fn new(cfg: &toml::Table, ch: &ChannelRegistry) -> Self {
        let channel_names: Vec<String> = cfg
            .get("channels")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        let window_s = cfg
            .get("window_s")
            .and_then(|v| v.as_float())
            .unwrap_or(DEFAULT_WINDOW_S);

        let mut builder = PlotWidgetBuilder::new().with_autoscale_on_updates(true);
        let mut channels = Vec::with_capacity(channel_names.len());

        for (i, name) in channel_names.into_iter().enumerate() {
            let id = ch.id(&name).filter(|&id| ACCEPTED.contains(&ch.meta(id).sample_type));
            let color = palette_color(i);
            // PlotWidget requires at least one point — use a sentinel that
            // sync() will overwrite immediately on the first tick.
            let series = Series::line_only(vec![[0.0, 0.0]], LineStyle::solid())
                .with_color(color)
                .with_label(name.clone());
            let shape_id = series.id;
            builder = builder.add_series(series);
            channels.push(ChannelSlot { name, id, shape_id });
        }

        // PlotWidgetBuilder::build fails with SeriesError::Empty when no series
        // were added. Fall back to an empty PlotWidget for the zero-channel
        // case, and for any unexpected build error — construction must never
        // panic (a failed build would otherwise crash the whole app).
        let mut widget = if channels.is_empty() {
            PlotWidget::new()
        } else {
            match builder.build() {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("waveform: PlotWidgetBuilder::build failed ({e:?}); using empty plot");
                    PlotWidget::new()
                }
            }
        };
        widget.autoscale_on_updates(true);

        Self { channels, window_s, widget }
    }

    /// Internal: append a channel slot and add a matching series to the widget.
    fn add_channel_slot(&mut self, name: &str, id: Option<ChannelId>) {
        let i = self.channels.len();
        let color = palette_color(i);
        // Sentinel point required — PlotWidget rejects empty positions.
        // sync() will replace it on the next tick.
        let series = Series::line_only(vec![[0.0, 0.0]], LineStyle::solid())
            .with_color(color)
            .with_label(name.to_string());
        let shape_id = series.id;
        // add_series after build — PlotWidget supports this.
        self.widget.add_series(series).ok();
        self.channels.push(ChannelSlot { name: name.to_string(), id, shape_id });
    }
}

// ---------------------------------------------------------------------------
// VizPanel implementation
// ---------------------------------------------------------------------------

impl VizPanel for Waveform {
    fn title(&self) -> String {
        if self.channels.is_empty() {
            "Waveform".to_string()
        } else {
            self.channels.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
        }
    }

    fn sync(&mut self, store: &dyn ChannelStore) {
        let now_ns = store.now_ns();
        let window_ns = (self.window_s * 1e9) as i64;
        let start_ns = now_ns - window_ns;
        // t0 anchors X coordinates so (ts - t0) / 1e9 stays in a small range
        // and double-precision f64 doesn't lose sub-second detail.
        let t0 = start_ns;

        // Fixed rolling X window: points are shifted to x = (ts - t0)/1e9, so
        // the visible window is always [0, window_s]. Pinning the X limits
        // keeps the time axis stable instead of letting autoscale zoom it to
        // the data extent. Y is still autoscaled via autoscale_on_updates.
        self.widget.set_x_lim(0.0, self.window_s);

        for slot in &self.channels {
            let id = match slot.id {
                Some(id) => id,
                None => continue, // unknown channel — leave series empty
            };

            let window = TimeWindow { start_ns, end_ns: now_ns };
            let snap = store.snapshot(id, window);
            let points = snapshot_to_points(&snap, t0, MAX_PLOT_BUCKETS);

            let sid = slot.shape_id;
            self.widget
                .update_series(&sid, |s| {
                    s.positions.clear();
                    s.positions.extend_from_slice(&points);
                })
                .ok();
        }
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
        let label = if self.channels.is_empty() {
            "No channels bound — drag a channel here.".to_string()
        } else {
            format!(
                "Channels: {}  |  Window: {}s",
                self.channels.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "),
                self.window_s
            )
        };
        container(text(label)).into()
    }

    fn bind_channel(&mut self, name: &str) -> bool {
        // Duplicate guard.
        if self.channels.iter().any(|s| s.name == name) {
            return false;
        }
        // No registry access here; id stays None. The serialize contract
        // requires only that the name appears in the serialized channels list.
        self.add_channel_slot(name, None);
        true
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        let names: Vec<toml::Value> = self
            .channels
            .iter()
            .map(|s| toml::Value::String(s.name.clone()))
            .collect();
        t.insert("channels".to_string(), toml::Value::Array(names));
        if (self.window_s - DEFAULT_WINDOW_S).abs() > 0.001 {
            t.insert("window_s".to_string(), toml::Value::Float(self.window_s));
        }
        t
    }
}

// ---------------------------------------------------------------------------
// Helper: convert a ChannelSnapshot to decimated [f64; 2] points
// ---------------------------------------------------------------------------

fn snapshot_to_points(snap: &ChannelSnapshot, t0: i64, max_buckets: usize) -> Vec<[f64; 2]> {
    match snap {
        ChannelSnapshot::Float { ts, vals } => decimate_minmax(ts, vals, t0, max_buckets),
        ChannelSnapshot::Int { ts, vals } => {
            let fvals: Vec<f64> = vals.iter().map(|&v| v as f64).collect();
            decimate_minmax(ts, &fvals, t0, max_buckets)
        }
        ChannelSnapshot::Bool { ts, vals } => {
            let fvals: Vec<f64> = vals.iter().map(|&v| v as f64).collect();
            decimate_minmax(ts, &fvals, t0, max_buckets)
        }
        ChannelSnapshot::Text { .. } => vec![],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal two-channel config for tests.
    const SAMPLE_TWO_CHANNELS: &str = r#"
[channels."a"]
mqtt_topic = "sensors/a"
type = "float"

[channels."b"]
mqtt_topic = "sensors/b"
type = "float"
"#;

    #[test]
    fn waveform_binds_and_serializes() {
        let ch = crate::config::ChannelRegistry::from_toml_str(SAMPLE_TWO_CHANNELS).unwrap();
        let mut cfg = toml::Table::new();
        cfg.insert("channels".into(), toml::Value::Array(vec!["a".into()]));
        let mut w = Waveform::new(&cfg, &ch);
        assert!(w.bind_channel("b"));
        let out = w.serialize();
        let chans = out.get("channels").unwrap().as_array().unwrap();
        assert_eq!(chans.len(), 2); // a + newly bound b
    }

    #[test]
    fn waveform_empty_config_constructs() {
        let ch = crate::config::ChannelRegistry::from_toml_str("").unwrap();
        let cfg = toml::Table::new();
        let w = Waveform::new(&cfg, &ch);
        assert_eq!(w.title(), "Waveform");
        let out = w.serialize();
        let chans = out.get("channels").unwrap().as_array().unwrap();
        assert!(chans.is_empty());
    }

    #[test]
    fn waveform_duplicate_bind_returns_false() {
        let ch = crate::config::ChannelRegistry::from_toml_str(SAMPLE_TWO_CHANNELS).unwrap();
        let mut cfg = toml::Table::new();
        cfg.insert("channels".into(), toml::Value::Array(vec!["a".into()]));
        let mut w = Waveform::new(&cfg, &ch);
        assert!(!w.bind_channel("a"));
        let out = w.serialize();
        assert_eq!(out.get("channels").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn waveform_unknown_channel_survives_new() {
        let ch = crate::config::ChannelRegistry::from_toml_str("").unwrap();
        let mut cfg = toml::Table::new();
        cfg.insert(
            "channels".into(),
            toml::Value::Array(vec!["does.not.exist".into()]),
        );
        // must not panic
        let w = Waveform::new(&cfg, &ch);
        assert_eq!(w.title(), "does.not.exist");
    }

    #[test]
    fn waveform_sync_with_live_data() {
        use crate::store::LiveStore;
        use crate::types::NumericVal;

        let ch = crate::config::ChannelRegistry::from_toml_str(SAMPLE_TWO_CHANNELS).unwrap();
        let store = LiveStore::from_registry(&ch);
        let id_a = ch.id("a").unwrap();

        for i in 0..100i64 {
            store.write_numeric(
                id_a,
                i * 10_000_000,
                NumericVal::Float((i as f64 * 0.1).sin()),
            );
        }

        let mut cfg = toml::Table::new();
        cfg.insert("channels".into(), toml::Value::Array(vec!["a".into()]));
        let mut w = Waveform::new(&cfg, &ch);
        // sync must not panic
        w.sync(&store);
    }
}
