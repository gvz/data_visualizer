//! Iced-port gauge panel — arc + needle drawn on an iced `Canvas`.
//!
//! The gauge shows the latest scalar value within a [min, max] range as a
//! sweep of an arc (like a classic analog dial).  The canvas `Program`
//! pattern established here is the template for Tasks 9 and 11.

use iced::widget::canvas::{self, Canvas, Frame, Path, Stroke};
use iced::{Color, Length, Point, Radians, Rectangle, Renderer, Theme, mouse};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, Sample, SampleType};
use crate::viz::canvas_util::centered_text;
use crate::viz::{PanelMessage, VizPanel};

// ---------------------------------------------------------------------------
// Helpers (inlined per-panel)
// ---------------------------------------------------------------------------

fn opt_str(cfg: &toml::Table, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn opt_f64(cfg: &toml::Table, key: &str, default: f64) -> f64 {
    match cfg.get(key) {
        Some(toml::Value::Float(f)) => *f,
        Some(toml::Value::Integer(i)) => *i as f64,
        _ => default,
    }
}

fn sample_as_f64(s: &Sample) -> Option<f64> {
    match s {
        Sample::Float(v) => Some(*v),
        Sample::Int(v) => Some(*v as f64),
        Sample::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Sample::Text(_) => None,
    }
}

pub const TYPE_NAME: &str = "gauge";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

// ---------------------------------------------------------------------------
// Gauge arc geometry constants
//
// The gauge arc spans from START_ANGLE to END_ANGLE (clockwise, in radians,
// measured from the positive x-axis as per iced's convention).
// We want a roughly 240-degree sweep: from 7 o'clock to 5 o'clock.
//   7 o'clock ≈ 210° from x-axis = 210° clockwise = 210 * π/180
//   5 o'clock ≈ 330° from x-axis = 330 * π/180
// But iced measures clockwise from east (x+), so:
//   start = 210°, end = 330° (330-210 = 120° gap at the bottom — perfect).
// ---------------------------------------------------------------------------

const START_ANGLE: f32 = std::f32::consts::PI * 7.0 / 6.0; // 210°
const END_ANGLE: f32 = std::f32::consts::PI * 11.0 / 6.0; // 330°
// Total sweep of the arc (240°)
const SWEEP: f32 = END_ANGLE - START_ANGLE; // 2π * 2/3

// ---------------------------------------------------------------------------
// Gauge struct
// ---------------------------------------------------------------------------

/// Arc-style gauge panel.
pub struct Gauge {
    channel_name: String,
    channel_id: Option<ChannelId>,
    min: f64,
    max: f64,
    /// Latest cached scalar, updated each `sync` call.
    value: Option<f64>,
}

impl Gauge {
    /// Construct from a TOML config table and a channel registry.
    pub fn new(cfg: &toml::Table, ch: &ChannelRegistry) -> Self {
        let channel_name = opt_str(cfg, "channel");
        let channel_id = ch.id(&channel_name).filter(|&id| {
            ACCEPTED.contains(&ch.meta(id).sample_type)
        });
        let min = opt_f64(cfg, "min", 0.0);
        let max = opt_f64(cfg, "max", 100.0);
        // Clamp to avoid degenerate gauges (max == min would cause NaN).
        let max = if max <= min { min + 1.0 } else { max };
        Gauge {
            channel_name,
            channel_id,
            min,
            max,
            value: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Canvas Program
// ---------------------------------------------------------------------------

/// Snapshot passed into the `Canvas` during `view()`.  Carries the data the
/// `Program::draw` call needs — value and range — without holding a reference
/// to `Gauge` itself (enabling `'static` lifetimes as iced requires).
struct GaugeProgram {
    value: Option<f64>,
    min: f64,
    max: f64,
}

impl GaugeProgram {
    /// Map a value in [min, max] to [0.0, 1.0], clamped. Single source of
    /// truth for the gauge's value→fraction math (the draw path uses this).
    fn fraction(&self, v: f64) -> f32 {
        ((v - self.min) / (self.max - self.min)).clamp(0.0, 1.0) as f32
    }

    /// Angle (in radians, iced convention) for a given fraction in [0,1].
    /// Single source of truth for the gauge's fraction→angle math.
    fn angle_for_fraction(&self, frac: f32) -> f32 {
        START_ANGLE + frac * SWEEP
    }
}

impl canvas::Program<PanelMessage> for GaugeProgram {
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

        // Geometry: center slightly above middle, radius fits the frame.
        let cx = bounds.width * 0.5;
        let cy = bounds.height * 0.55;
        let center = Point::new(cx, cy);
        let radius = (bounds.width.min(bounds.height) * 0.4).max(10.0);

        // --- Background arc (full sweep, dark grey) ---
        let bg_arc = Path::new(|b| {
            b.arc(canvas::path::Arc {
                center,
                radius,
                start_angle: Radians(START_ANGLE),
                end_angle: Radians(END_ANGLE),
            });
        });
        frame.stroke(
            &bg_arc,
            Stroke::default()
                .with_color(Color::from_rgb(0.25, 0.25, 0.25))
                .with_width(radius * 0.18),
        );

        // --- Value arc (filled portion, teal) ---
        if let Some(v) = self.value {
            let frac = self.fraction(v);
            if frac > 0.001 {
                let value_end = self.angle_for_fraction(frac);
                let value_arc = Path::new(|b| {
                    b.arc(canvas::path::Arc {
                        center,
                        radius,
                        start_angle: Radians(START_ANGLE),
                        end_angle: Radians(value_end),
                    });
                });
                frame.stroke(
                    &value_arc,
                    Stroke::default()
                        .with_color(Color::from_rgb(0.18, 0.65, 0.73))
                        .with_width(radius * 0.18),
                );
            }
        }

        // --- Needle ---
        {
            let frac = self.value.map(|v| self.fraction(v)).unwrap_or(0.0);
            let angle = self.angle_for_fraction(frac);
            let (sin_a, cos_a) = angle.sin_cos();
            let needle_len = radius * 0.85;
            let tip = Point::new(cx + cos_a * needle_len, cy + sin_a * needle_len);
            let needle = Path::new(|b| {
                b.move_to(center);
                b.line_to(tip);
            });
            frame.stroke(
                &needle,
                Stroke::default()
                    .with_color(Color::WHITE)
                    .with_width(2.0),
            );
        }

        // --- Center dot ---
        {
            let dot = Path::circle(center, radius * 0.06);
            frame.fill(&dot, Color::from_rgb(0.8, 0.8, 0.8));
        }

        // --- Min / max labels ---
        {
            let label_offset = radius * 1.1;
            let label_color = Color::from_rgb(0.7, 0.7, 0.7);
            let label_size = radius * 0.22;

            let (sin_start, cos_start) = START_ANGLE.sin_cos();
            let min_pos = Point::new(
                cx + cos_start * label_offset,
                cy + sin_start * label_offset,
            );
            centered_text(&mut frame, min_pos, format!("{:.1}", self.min), label_size, label_color);

            let (sin_end, cos_end) = END_ANGLE.sin_cos();
            let max_pos = Point::new(
                cx + cos_end * label_offset,
                cy + sin_end * label_offset,
            );
            centered_text(&mut frame, max_pos, format!("{:.1}", self.max), label_size, label_color);
        }

        // --- Value readout in centre ---
        {
            let text = self
                .value
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "\u{2014}".to_string());
            centered_text(
                &mut frame,
                Point::new(cx, cy - radius * 0.35),
                text,
                radius * 0.28,
                Color::WHITE,
            );
        }

        vec![frame.into_geometry()]
    }
}

// ---------------------------------------------------------------------------
// VizPanel implementation
// ---------------------------------------------------------------------------

impl VizPanel for Gauge {
    fn title(&self) -> String {
        if self.channel_name.is_empty() {
            "Gauge".to_string()
        } else {
            self.channel_name.clone()
        }
    }

    fn sync(&mut self, store: &dyn ChannelStore) {
        let Some(id) = self.channel_id else {
            self.value = None;
            return;
        };
        self.value = store
            .latest(id)
            .and_then(|(_, s)| sample_as_f64(&s));
    }

    fn view(&self) -> iced::Element<'_, PanelMessage> {
        Canvas::new(GaugeProgram {
            value: self.value,
            min: self.min,
            max: self.max,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    fn update(&mut self, _msg: PanelMessage) {
        // No interactive messages for this panel.
    }

    fn config_view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, text};
        let label = if self.channel_name.is_empty() {
            "No channel bound — drag a channel here.".to_string()
        } else {
            format!(
                "Channel: {}  |  Range: [{:.1}, {:.1}]",
                self.channel_name, self.min, self.max
            )
        };
        container(text(label)).into()
    }

    fn bind_channel(&mut self, name: &str) -> bool {
        self.channel_name = name.to_string();
        // No registry access here; id stays None until next construction.
        true
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channel".to_string(),
            toml::Value::String(self.channel_name.clone()),
        );
        t.insert("min".to_string(), toml::Value::Float(self.min));
        t.insert("max".to_string(), toml::Value::Float(self.max));
        t
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL_TOML: &str = r#"
[channels."a"]
mqtt_topic = "sensors/a"
type = "float"
unit = ""
"#;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(CHANNEL_TOML).unwrap()
    }

    fn make_panel() -> Gauge {
        let ch = registry();
        let mut cfg = toml::Table::new();
        cfg.insert("channel".into(), toml::Value::String("a".into()));
        Gauge::new(&cfg, &ch)
    }

    // --- required tests from the brief ---

    #[test]
    fn title_reflects_channel() {
        let p = make_panel();
        assert_eq!(p.title(), "a");
    }

    #[test]
    fn serialize_roundtrips_channel_key() {
        let p = make_panel();
        let out = p.serialize();
        assert_eq!(
            out.get("channel").and_then(|v| v.as_str()),
            Some("a"),
            "serialize must persist the channel name"
        );
    }

    #[test]
    fn serialize_includes_min_max() {
        let p = make_panel();
        let out = p.serialize();
        assert!(out.get("min").is_some(), "serialize must include min");
        assert!(out.get("max").is_some(), "serialize must include max");
    }

    // --- geometry helpers ---
    //
    // These exercise `GaugeProgram`'s helpers directly — the same code the
    // `draw` path calls — so a change to the math can't silently diverge from
    // what is rendered.

    fn program(min: f64, max: f64) -> GaugeProgram {
        GaugeProgram { value: None, min, max }
    }

    #[test]
    fn fraction_clamps() {
        let p = program(0.0, 100.0);
        assert!((p.fraction(50.0) - 0.5).abs() < 1e-6);
        assert_eq!(p.fraction(-1.0), 0.0);
        assert_eq!(p.fraction(101.0), 1.0);
    }

    #[test]
    fn angle_for_fraction_bounds() {
        let p = program(0.0, 100.0);
        let start = p.angle_for_fraction(0.0);
        let end = p.angle_for_fraction(1.0);
        assert!((start - START_ANGLE).abs() < 1e-6);
        assert!((end - END_ANGLE).abs() < 1e-6);
    }
}
