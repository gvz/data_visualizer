//! Iced-port state-graph panel — Grafana-style colored bands over time.
//!
//! `sync` fetches a snapshot from the store, computes contiguous value-run
//! segments, and caches them.  `view` renders the cached segments via an iced
//! `Canvas` that draws colored horizontal rectangles + a time axis + a legend.

use std::collections::BTreeMap;

use iced::widget::canvas::{self, Canvas, Frame};
use iced::{Color, Length, Point, Rectangle, Renderer, Theme, mouse};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::canvas_util::centered_text;
use crate::viz::{PanelMessage, VizPanel};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn opt_str(cfg: &toml::Table, key: &str) -> String {
    cfg.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn opt_f64(cfg: &toml::Table, key: &str) -> Option<f64> {
    cfg.get(key).and_then(|v| v.as_float())
}

// ---------------------------------------------------------------------------
// Color palette (matches the old egui state_graph exactly)
// ---------------------------------------------------------------------------

pub const TYPE_NAME: &str = "state_graph";

#[allow(dead_code)]
const ACCEPTED: &[SampleType] = &[SampleType::Bool, SampleType::Int, SampleType::Text];

/// Six-color palette, same hues as the old egui panel.
pub(crate) const PALETTE: &[Color] = &[
    Color { r: 0x4c as f32 / 255.0, g: 0xaf as f32 / 255.0, b: 0x50 as f32 / 255.0, a: 1.0 }, // green
    Color { r: 0x21 as f32 / 255.0, g: 0x96 as f32 / 255.0, b: 0xf3 as f32 / 255.0, a: 1.0 }, // blue
    Color { r: 0xff as f32 / 255.0, g: 0x98 as f32 / 255.0, b: 0x00 as f32 / 255.0, a: 1.0 }, // orange
    Color { r: 0xf4 as f32 / 255.0, g: 0x43 as f32 / 255.0, b: 0x36 as f32 / 255.0, a: 1.0 }, // red
    Color { r: 0x9c as f32 / 255.0, g: 0x27 as f32 / 255.0, b: 0xb0 as f32 / 255.0, a: 1.0 }, // purple
    Color { r: 0x00 as f32 / 255.0, g: 0xbc as f32 / 255.0, b: 0xd4 as f32 / 255.0, a: 1.0 }, // cyan
];

/// Deterministic, repeating color for a value code.
pub(crate) fn color_for(value: i64) -> Color {
    PALETTE[(value.rem_euclid(PALETTE.len() as i64)) as usize]
}

// ---------------------------------------------------------------------------
// Segment helpers (pure math — lifted unchanged from the old panel)
// ---------------------------------------------------------------------------

/// Contiguous runs of equal values: (start_ts, end_ts, value).  The final
/// segment ends at the last timestamp.
pub(crate) fn segments(ts: &[i64], vals: &[i64], breaks: &[i64]) -> Vec<(i64, i64, i64)> {
    if ts.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let (mut start, mut cur) = (ts[0], vals[0]);
    for i in 1..ts.len() {
        let broken = breaks.iter().any(|&b| b > ts[i - 1] && b <= ts[i]);
        if broken {
            out.push((start, ts[i - 1], cur));
            start = ts[i];
            cur = vals[i];
        } else if vals[i] != cur {
            out.push((start, ts[i], cur));
            start = ts[i];
            cur = vals[i];
        }
    }
    out.push((start, *ts.last().unwrap(), cur));
    out
}

/// Segments stretched to cover the whole `[t0, end_ns]` window so the strip
/// tiles the full width even when samples cover only part of the window.
pub(crate) fn painted_spans(
    ts: &[i64],
    vals: &[i64],
    t0: i64,
    end_ns: i64,
    breaks: &[i64],
) -> Vec<(i64, i64, i64)> {
    let mut segs = segments(ts, vals, breaks);
    if let Some(first) = segs.first_mut() {
        first.0 = first.0.min(t0);
    }
    if let Some(last) = segs.last_mut() {
        last.1 = last.1.max(end_ns);
    }
    segs
}

/// Nice time axis tick step (ns) snapped to 1/2/5 × 10^n, targeting ~`target`
/// ticks across `span_ns`.
pub(crate) fn nice_time_step_ns(span_ns: i64, target: i64) -> i64 {
    let rough = (span_ns / target.max(1)).max(1) as f64;
    let mag = 10f64.powf(rough.log10().floor());
    let norm = rough / mag;
    let mult = if norm <= 1.5 {
        1.0
    } else if norm <= 3.0 {
        2.0
    } else if norm <= 7.0 {
        5.0
    } else {
        10.0
    };
    ((mag * mult) as i64).max(1)
}

// ---------------------------------------------------------------------------
// Cached render data
// ---------------------------------------------------------------------------

/// One rendered segment: time range + color + optional label.
#[derive(Clone)]
struct Segment {
    t_start: i64,
    t_end: i64,
    color: Color,
    label: String,
}

/// Legend entry: color swatch + label text.
#[derive(Clone)]
struct LegendEntry {
    color: Color,
    label: String,
}

// ---------------------------------------------------------------------------
// StateGraph panel struct
// ---------------------------------------------------------------------------

/// Grafana-style colored-band state graph panel.
pub struct StateGraph {
    channel_name: String,
    channel_id: Option<ChannelId>,
    /// Explicit code→label map from config (numeric/bool channels).
    states: BTreeMap<i64, String>,
    /// Runtime interning for Text channels.
    text_codes: BTreeMap<String, i64>,
    text_labels: BTreeMap<i64, String>,
    /// Default time window in seconds (30 s when `None`).
    time_window_s: Option<f64>,
    // Cached render data filled by `sync`.
    cached_segments: Vec<Segment>,
    cached_legend: Vec<LegendEntry>,
    cached_t0: i64,
    cached_end: i64,
}

const DEFAULT_WINDOW_S: f64 = 30.0;

impl StateGraph {
    /// Construct from a TOML config table and channel registry.
    pub fn new(cfg: &toml::Table, ch: &ChannelRegistry) -> Self {
        let channel_name = opt_str(cfg, "channel");
        let channel_id = ch.id(&channel_name);
        let mut states = BTreeMap::new();
        if let Some(tbl) = cfg.get("states").and_then(|v| v.as_table()) {
            for (k, v) in tbl {
                if let Ok(key) = k.parse::<i64>() {
                    states.insert(key, v.as_str().unwrap_or_default().to_string());
                }
            }
        }
        let time_window_s = opt_f64(cfg, "time_window_s");
        StateGraph {
            channel_name,
            channel_id,
            states,
            text_codes: BTreeMap::new(),
            text_labels: BTreeMap::new(),
            time_window_s,
            cached_segments: Vec::new(),
            cached_legend: Vec::new(),
            cached_t0: 0,
            cached_end: 1,
        }
    }

    /// Intern a string to a stable integer code (first-seen order).
    fn intern(&mut self, s: &str) -> i64 {
        if let Some(&code) = self.text_codes.get(s) {
            return code;
        }
        let code = self.text_codes.len() as i64;
        self.text_codes.insert(s.to_string(), code);
        self.text_labels.insert(code, s.to_string());
        code
    }

    fn label_for(&self, v: i64, store_labels: &Option<Vec<String>>) -> String {
        if let Some(lbls) = store_labels {
            if let Some(s) = lbls.get(v as usize) {
                return s.clone();
            }
        }
        if let Some(s) = self.text_labels.get(&v) {
            return s.clone();
        }
        if let Some(s) = self.states.get(&v) {
            return s.clone();
        }
        v.to_string()
    }
}

// ---------------------------------------------------------------------------
// VizPanel implementation
// ---------------------------------------------------------------------------

impl VizPanel for StateGraph {
    fn title(&self) -> String {
        if self.channel_name.is_empty() {
            "State Graph".to_string()
        } else {
            self.channel_name.clone()
        }
    }

    fn sync(&mut self, store: &dyn ChannelStore) {
        let Some(id) = self.channel_id else {
            self.cached_segments.clear();
            self.cached_legend.clear();
            return;
        };
        if store.latest(id).is_none() {
            self.cached_segments.clear();
            self.cached_legend.clear();
            return;
        }
        let window_ns = ((self.time_window_s.unwrap_or(DEFAULT_WINDOW_S)) * 1e9) as i64;
        let end_ns = store.now_ns();
        let t0 = end_ns - window_ns;
        self.cached_t0 = t0;
        self.cached_end = end_ns;

        let snap = store.snapshot(id, TimeWindow { start_ns: t0, end_ns: end_ns + 1 });
        let store_labels = store.state_labels(id);

        let mut text_mode = false;
        let (ts, vals): (Vec<i64>, Vec<i64>) = match &snap {
            ChannelSnapshot::Int { ts, vals } => (ts.clone(), vals.clone()),
            ChannelSnapshot::Bool { ts, vals } => {
                (ts.clone(), vals.iter().map(|&v| v as i64).collect())
            }
            ChannelSnapshot::Text { lines } => {
                text_mode = true;
                let ts: Vec<i64> = lines.iter().map(|(t, _)| *t).collect();
                let vals: Vec<i64> = lines.iter().map(|(_, s)| self.intern(s)).collect();
                (ts, vals)
            }
            _ => {
                self.cached_segments.clear();
                self.cached_legend.clear();
                return;
            }
        };

        let breaks = store.break_times();
        let spans = painted_spans(&ts, &vals, t0, end_ns, breaks);

        // Build legend: distinct values in encounter order.
        let mut seen: Vec<i64> = Vec::new();
        for &(_, _, v) in &spans {
            if !seen.contains(&v) {
                seen.push(v);
            }
        }
        // Also pull from store labels / states for known-but-absent codes.
        if let Some(lbls) = &store_labels {
            for (i, _) in lbls.iter().enumerate() {
                let code = i as i64;
                if !seen.contains(&code) {
                    seen.push(code);
                }
            }
        } else if !text_mode {
            for &code in self.states.keys() {
                if !seen.contains(&code) {
                    seen.push(code);
                }
            }
        }

        self.cached_segments = spans
            .into_iter()
            .map(|(s, e, v)| Segment {
                t_start: s,
                t_end: e,
                color: color_for(v),
                label: self.label_for(v, &store_labels),
            })
            .collect();

        self.cached_legend = seen
            .iter()
            .map(|&v| LegendEntry {
                color: color_for(v),
                label: self.label_for(v, &store_labels),
            })
            .collect();
    }

    fn view(&self) -> iced::Element<'_, PanelMessage> {
        if self.channel_name.is_empty() {
            use iced::widget::{container, text};
            return container(text("Drop a channel here"))
                .width(Length::Fill)
                .height(Length::Fill)
                .into();
        }
        Canvas::new(StateGraphProgram {
            segments: self.cached_segments.clone(),
            legend: self.cached_legend.clone(),
            t0: self.cached_t0,
            t_end: self.cached_end,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn update(&mut self, _msg: PanelMessage) {}

    fn config_view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, text};
        let label = if self.channel_name.is_empty() {
            "No channel bound — drag a channel here.".to_string()
        } else {
            format!("Channel: {}", self.channel_name)
        };
        container(text(label)).into()
    }

    fn bind_channel(&mut self, name: &str) -> bool {
        // Accept Bool/Int/Text channels.  We don't have a registry here, so we
        // store the name and leave id as None; the next registry-aware construction
        // (or a future bind_channel_with_registry) would resolve it.
        // For the minimal contract: store the name, return true.
        self.channel_name = name.to_string();
        self.channel_id = None; // caller should reconstruct from config
        true
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channel".to_string(),
            toml::Value::String(self.channel_name.clone()),
        );
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
        t
    }
}

// ---------------------------------------------------------------------------
// Canvas Program
// ---------------------------------------------------------------------------

/// Snapshot data passed into the iced Canvas for a single frame.
struct StateGraphProgram {
    segments: Vec<Segment>,
    legend: Vec<LegendEntry>,
    t0: i64,
    t_end: i64,
}

impl canvas::Program<PanelMessage> for StateGraphProgram {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());

        // Background
        frame.fill_rectangle(
            Point::ORIGIN,
            bounds.size(),
            Color::from_rgb(0.118, 0.118, 0.118), // #1e1e1e
        );

        if self.segments.is_empty() {
            centered_text(
                &mut frame,
                Point::new(bounds.width * 0.5, bounds.height * 0.5),
                "no data",
                14.0,
                Color::from_rgb(0.6, 0.6, 0.6),
            );
            return vec![frame.into_geometry()];
        }

        let t_span = (self.t_end - self.t0).max(1) as f64;

        // Reserve legend height at bottom (approx 20px), band gets the rest.
        let legend_h = 20.0f32;
        let band_h = (bounds.height - legend_h).max(10.0);

        // Draw colored band segments.
        for seg in &self.segments {
            let x0 = bounds.width * ((seg.t_start - self.t0) as f32 / t_span as f32);
            let x1 = bounds.width * ((seg.t_end - self.t0) as f32 / t_span as f32);
            let w = (x1 - x0).max(1.0);
            frame.fill_rectangle(
                Point::new(x0.max(0.0), 0.0),
                iced::Size::new(w, band_h),
                seg.color,
            );
            // Label inside segment if wide enough.
            if w > 40.0 {
                centered_text(
                    &mut frame,
                    Point::new(x0 + w * 0.5, band_h * 0.5),
                    seg.label.clone(),
                    12.0,
                    Color::BLACK,
                );
            }
        }

        // Time axis: tick marks at the bottom of the band.
        let axis_y = band_h;
        let tick_color = Color::from_rgb(0.43, 0.43, 0.43);
        let step_ns = nice_time_step_ns((self.t_end - self.t0).max(1), 5);
        let t_span_ns = self.t_end - self.t0;
        if step_ns > 0 && t_span_ns > 0 {
            let mut t = (self.t0 / step_ns) * step_ns;
            if t < self.t0 {
                t += step_ns;
            }
            while t <= self.t_end {
                let x = bounds.width * ((t - self.t0) as f32 / t_span as f32);
                // Tick line
                let tick_path = canvas::Path::line(
                    Point::new(x, axis_y),
                    Point::new(x, axis_y + 4.0),
                );
                frame.stroke(
                    &tick_path,
                    canvas::Stroke::default().with_color(tick_color).with_width(1.0),
                );
                // Label
                let label = format_time_of_day(t);
                centered_text(
                    &mut frame,
                    Point::new(x, axis_y + 10.0),
                    label,
                    9.0,
                    Color::from_rgb(0.63, 0.63, 0.63),
                );
                t += step_ns;
            }
        }

        // Legend: colored dot + label, laid out horizontally below axis.
        let legend_y = band_h + legend_h * 0.5 + 2.0;
        let mut x_cursor = 4.0f32;
        for entry in &self.legend {
            if x_cursor + 60.0 > bounds.width {
                break;
            }
            // Colored square swatch (8×8).
            frame.fill_rectangle(
                Point::new(x_cursor, legend_y - 4.0),
                iced::Size::new(8.0, 8.0),
                entry.color,
            );
            x_cursor += 11.0;
            centered_text(
                &mut frame,
                Point::new(x_cursor + 20.0, legend_y),
                entry.label.clone(),
                11.0,
                Color::from_rgb(0.85, 0.85, 0.85),
            );
            x_cursor += 46.0;
        }

        vec![frame.into_geometry()]
    }
}

/// Format a nanosecond timestamp as `HH:MM:SS` (UTC).
fn format_time_of_day(ts_ns: i64) -> String {
    let ts_s = ts_ns / 1_000_000_000;
    let h = (ts_s / 3600) % 24;
    let m = (ts_s / 60) % 60;
    let s = ts_s % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::store::LiveStore;
    use crate::types::NumericVal;

    const CHANNEL_TOML: &str = r#"
[channels."a"]
mqtt_topic = "sensors/a"
type = "int"

[channels."b"]
mqtt_topic = "sensors/b"
type = "bool"

[channels."t"]
mqtt_topic = "sensors/t"
type = "text"
"#;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(CHANNEL_TOML).unwrap()
    }

    fn make_panel_cfg(channel: &str) -> (toml::Table, ChannelRegistry) {
        let ch = registry();
        let mut cfg = toml::Table::new();
        cfg.insert("channel".into(), toml::Value::String(channel.into()));
        (cfg, ch)
    }

    // --- required tests (brief) ---

    #[test]
    fn constructs_from_channel_config() {
        let (cfg, ch) = make_panel_cfg("a");
        let p = StateGraph::new(&cfg, &ch);
        assert_eq!(p.channel_name, "a");
        assert!(p.channel_id.is_some(), "channel 'a' must resolve");
    }

    #[test]
    fn serialize_roundtrips_channel_key() {
        let (cfg, ch) = make_panel_cfg("a");
        let p = StateGraph::new(&cfg, &ch);
        let out = p.serialize();
        assert_eq!(
            out.get("channel").and_then(|v| v.as_str()),
            Some("a"),
            "serialize must round-trip channel key"
        );
    }

    #[test]
    fn color_for_is_deterministic() {
        // Same input → same color.
        assert_eq!(color_for(0), color_for(0));
        assert_eq!(color_for(3), color_for(3));
        // Different values → different colors (palette has 6 distinct entries).
        let colors: Vec<Color> = (0..6).map(color_for).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    (colors[i].r, colors[i].g, colors[i].b),
                    (colors[j].r, colors[j].g, colors[j].b),
                    "values {i} and {j} must map to distinct colors"
                );
            }
        }
    }

    #[test]
    fn segments_merge_consecutive_values() {
        let ts = [0i64, 1, 2, 3, 4];
        let vals = [0i64, 0, 1, 1, 0];
        assert_eq!(segments(&ts, &vals, &[]), vec![(0, 2, 0), (2, 4, 1), (4, 4, 0)]);
        assert!(segments(&[], &[], &[]).is_empty());
        assert_eq!(segments(&[7], &[3], &[]), vec![(7, 7, 3)]);
    }

    #[test]
    fn segments_break_splits_run() {
        let ts = [0i64, 1, 2, 3];
        let vals = [5i64, 5, 5, 5];
        assert_eq!(segments(&ts, &vals, &[2]), vec![(0, 1, 5), (2, 3, 5)]);
    }

    #[test]
    fn painted_spans_fill_whole_window() {
        let (t0, end) = (0i64, 10_000i64);
        let ts = [9_990i64, 9_995, 10_000];
        let vals = [1i64, 2, 2];
        let spans = painted_spans(&ts, &vals, t0, end, &[]);
        assert_eq!(spans.first().unwrap().0, t0);
        assert_eq!(spans.last().unwrap().1, end);
        let mut cursor = t0;
        for (s, e, _) in &spans {
            assert_eq!(*s, cursor);
            cursor = *e;
        }
        assert_eq!(cursor, end);
    }

    #[test]
    fn painted_spans_empty_without_data() {
        assert!(painted_spans(&[], &[], 0, 10, &[]).is_empty());
    }

    #[test]
    fn nice_time_step_snaps_to_1_2_5() {
        assert_eq!(nice_time_step_ns(10_000_000_000, 5), 2_000_000_000);
        assert_eq!(nice_time_step_ns(1_000_000_000, 5), 200_000_000);
        assert_eq!(nice_time_step_ns(60_000_000_000, 5), 10_000_000_000);
        assert!(nice_time_step_ns(1, 5) >= 1);
    }

    #[test]
    fn sync_and_view_without_panic() {
        let (cfg, ch) = make_panel_cfg("a");
        let store = LiveStore::from_registry(&ch);
        let id = ch.id("a").unwrap();
        for i in 0..10i64 {
            store.write_numeric(id, i * 1_000_000, NumericVal::Int(i / 3));
        }
        let mut p = StateGraph::new(&cfg, &ch);
        p.sync(&store);
        let _ = p.view(); // must not panic
    }

    #[test]
    fn title_reflects_channel() {
        let (cfg, ch) = make_panel_cfg("a");
        let p = StateGraph::new(&cfg, &ch);
        assert_eq!(p.title(), "a");
    }

    #[test]
    fn title_default_when_no_channel() {
        let ch = registry();
        let p = StateGraph::new(&toml::Table::new(), &ch);
        assert_eq!(p.title(), "State Graph");
    }

    #[test]
    fn serialize_roundtrips_states_and_window() {
        let ch = registry();
        let cfg: toml::Table = toml::from_str(
            r#"channel = "a"
time_window_s = 30.0
[states]
0 = "IDLE"
1 = "RUN"
"#,
        )
        .unwrap();
        let p = StateGraph::new(&cfg, &ch);
        let out = p.serialize();
        assert_eq!(out.get("channel").and_then(|v| v.as_str()), Some("a"));
        assert!(out.get("time_window_s").is_some());
        let states_tbl = out.get("states").and_then(|v| v.as_table()).unwrap();
        assert_eq!(states_tbl.get("0").and_then(|v| v.as_str()), Some("IDLE"));
        assert_eq!(states_tbl.get("1").and_then(|v| v.as_str()), Some("RUN"));
    }
}
