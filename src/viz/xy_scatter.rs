//! Iced-port XY-scatter panel — two channels plotted against each other.
//!
//! `x_channel` and `y_channel` are aligned by sample index from the newest
//! sample backwards (same uniform-rate assumption as the old egui panel).
//! Points are rendered as circles via `Series::circles`. Both axes autoscale
//! via `autoscale_on_updates(true)`; no fixed axis limits are set.

use iced_plot::{Color, PlotWidget, PlotWidgetBuilder, Series, ShapeId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::{PanelMessage, VizPanel};

pub const TYPE_NAME: &str = "xy_scatter";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// Default visible history window in seconds.
const DEFAULT_HISTORY_S: f64 = 10.0;

// ---------------------------------------------------------------------------
// Helper: pair up the newest min(len) samples of both channels.
// ---------------------------------------------------------------------------

/// Align x and y value slices from their newest ends.
/// Returns `[[x0, y0], ...]` using the most recent `min(x.len(), y.len())`
/// samples from each.
pub(crate) fn index_align(x: &[f64], y: &[f64]) -> Vec<[f64; 2]> {
    let n = x.len().min(y.len());
    if n == 0 {
        return vec![];
    }
    x[x.len() - n..]
        .iter()
        .zip(&y[y.len() - n..])
        .map(|(&a, &b)| [a, b])
        .collect()
}

// ---------------------------------------------------------------------------
// Snapshot → f64 values helper
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
// XyScatter panel
// ---------------------------------------------------------------------------

/// XY-scatter panel: two channels plotted against each other as circle markers.
pub struct XyScatter {
    /// Configured X-axis channel name.
    x_name: String,
    /// Configured Y-axis channel name.
    y_name: String,
    /// Resolved X channel id.
    x_id: Option<ChannelId>,
    /// Resolved Y channel id.
    y_id: Option<ChannelId>,
    /// Optional time window (seconds); defaults to `DEFAULT_HISTORY_S`.
    time_window_s: Option<f64>,
    /// Series id for the scatter points.
    shape_id: ShapeId,
    /// Stateful plot widget — never rebuilt in view().
    widget: PlotWidget,
}

impl XyScatter {
    pub fn new(cfg: &toml::Table, ch: &ChannelRegistry) -> Self {
        let x_name = cfg
            .get("x_channel")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let y_name = cfg
            .get("y_channel")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let time_window_s = cfg.get("time_window_s").and_then(|v| v.as_float());

        let x_id = if x_name.is_empty() {
            None
        } else {
            ch.id(&x_name).filter(|&id| ACCEPTED.contains(&ch.meta(id).sample_type))
        };
        let y_id = if y_name.is_empty() {
            None
        } else {
            ch.id(&y_name).filter(|&id| ACCEPTED.contains(&ch.meta(id).sample_type))
        };

        // Orange (Tableau-10 slot 1) for the scatter series.
        let series = Series::circles(vec![[0.0, 0.0]], 2.0).with_color(Color::from_rgb(
            0xff as f32 / 255.0,
            0x7f as f32 / 255.0,
            0x0e as f32 / 255.0,
        ));
        let shape_id = series.id;

        let mut widget = match PlotWidgetBuilder::new()
            .with_autoscale_on_updates(true)
            .add_series(series)
            .build()
        {
            Ok(w) => w,
            Err(e) => {
                eprintln!("xy_scatter: PlotWidgetBuilder::build failed ({e:?}); using empty plot");
                PlotWidget::new()
            }
        };
        widget.autoscale_on_updates(true);

        Self { x_name, y_name, x_id, y_id, time_window_s, shape_id, widget }
    }
}

impl VizPanel for XyScatter {
    fn title(&self) -> String {
        if self.x_name.is_empty() && self.y_name.is_empty() {
            String::new()
        } else {
            format!("{} / {}", self.x_name, self.y_name)
        }
    }

    fn sync(&mut self, store: &dyn ChannelStore) {
        let (x_id, y_id) = match (self.x_id, self.y_id) {
            (Some(x), Some(y)) => (x, y),
            _ => return, // one or both channels unresolved
        };

        let now_ns = store.now_ns();
        let window_s = self.time_window_s.unwrap_or(DEFAULT_HISTORY_S);
        let window_ns = (window_s * 1e9) as i64;
        let start_ns = now_ns - window_ns;

        let win = TimeWindow { start_ns, end_ns: now_ns };
        let xs = store.snapshot(x_id, win);
        let ys = store.snapshot(y_id, win);

        let (Some((_, xv)), Some((_, yv))) = (snapshot_to_f64(&xs), snapshot_to_f64(&ys)) else {
            return;
        };

        let pts = index_align(&xv, &yv);
        if pts.is_empty() {
            return;
        }

        let sid = self.shape_id;
        self.widget
            .update_series(&sid, |s| {
                s.positions.clear();
                s.positions.extend_from_slice(&pts);
            })
            .ok();
        // Both axes autoscale via autoscale_on_updates(true); no set_x_lim.
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
        let label = if self.x_name.is_empty() && self.y_name.is_empty() {
            "No channels bound.".to_string()
        } else {
            format!("X: {}  |  Y: {}", self.x_name, self.y_name)
        };
        container(text(label)).into()
    }

    fn bind_channel(&mut self, _name: &str) -> bool {
        // Multi-channel binding is set at construction via config.
        false
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("x_channel".to_string(), toml::Value::String(self.x_name.clone()));
        t.insert("y_channel".to_string(), toml::Value::String(self.y_name.clone()));
        if let Some(w) = self.time_window_s {
            t.insert("time_window_s".to_string(), toml::Value::Float(w));
        }
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

[channels."demo.counter"]
mqtt_topic = "sensors/demo.counter"
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

    /// Required by the task brief: serialize/deserialize round-trip for
    /// x_channel and y_channel config keys.
    #[test]
    fn xy_scatter_serializes_x_y_channel_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "xy_scatter"
x_channel = "demo.sine"
y_channel = "demo.counter""#,
        )
        .unwrap();
        let p = reg.build(&e, &channels);
        let cfg = p.serialize();
        assert_eq!(cfg["x_channel"], toml::Value::String("demo.sine".into()));
        assert_eq!(cfg["y_channel"], toml::Value::String("demo.counter".into()));
        assert_eq!(p.title(), "demo.sine / demo.counter");
    }

    #[test]
    fn xy_scatter_serializes_with_time_window() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "xy_scatter"
x_channel = "demo.sine"
y_channel = "demo.counter"
time_window_s = 2.0"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels);
        let cfg = p.serialize();
        assert_eq!(cfg["x_channel"], toml::Value::String("demo.sine".into()));
        assert_eq!(cfg["y_channel"], toml::Value::String("demo.counter".into()));
        assert_eq!(cfg["time_window_s"], toml::Value::Float(2.0));
    }

    #[test]
    fn missing_channel_keys_build_empty_panel() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(r#"type = "xy_scatter""#).unwrap();
        let p = reg.build(&e, &channels);
        assert_eq!(p.title(), "");
    }

    #[test]
    fn xy_scatter_requires_both_channels_for_sync() {
        // With only x_channel set (no y_channel), sync must not panic.
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let mut cfg = toml::Table::new();
        cfg.insert("x_channel".into(), toml::Value::String("demo.sine".into()));
        let mut s = XyScatter::new(&cfg, &channels);
        // Should not panic.
        s.sync(&store);
    }

    #[test]
    fn xy_scatter_sync_with_live_data_no_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let x = channels.id("demo.sine").unwrap();
        let y = channels.id("demo.counter").unwrap();
        for i in 0..50i64 {
            store.write_numeric(x, i * 1_000_000, NumericVal::Float((i as f64).sin()));
            store.write_numeric(y, i * 1_000_000, NumericVal::Int(i));
        }
        let mut cfg = toml::Table::new();
        cfg.insert("x_channel".into(), toml::Value::String("demo.sine".into()));
        cfg.insert("y_channel".into(), toml::Value::String("demo.counter".into()));
        let mut s = XyScatter::new(&cfg, &channels);
        // Must not panic.
        s.sync(&store);
    }
}
