use eframe::egui::{self, Align2, Color32, FontId, Pos2, Sense, Stroke};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, SampleType};
use crate::viz::common::{
    bind, binding_error, is_light, label_config_row, opt_f64, opt_label, opt_str, outlined_text,
    refresh_binding, sample_as_f64, serialize_label, Binding, ColorThresholds, RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "gauge";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int];

/// The empty-track color behind every gauge fill: a dark trough in the Mocha
/// theme, a light one in Latte (a near-black track looks wrong on the bright
/// background).
fn track_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_gray(40)
    } else {
        Color32::from_gray(205)
    }
}

/// Which shape the gauge draws its value with. `Bar` is the original horizontal
/// fill and the default (so existing configs and their serialization are
/// unchanged); the others are chosen in the panel settings.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum GaugeStyle {
    /// Horizontal bar filling left→right.
    #[default]
    Bar,
    /// Vertical bar filling bottom→top.
    Vertical,
    /// Row of discrete lit segments (VU-meter look).
    Led,
    /// 270° radial dial arc.
    Arc,
}

impl GaugeStyle {
    fn parse(s: &str) -> Self {
        match s {
            "vertical" => GaugeStyle::Vertical,
            "led" => GaugeStyle::Led,
            "arc" => GaugeStyle::Arc,
            _ => GaugeStyle::Bar,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            GaugeStyle::Bar => "bar",
            GaugeStyle::Vertical => "vertical",
            GaugeStyle::Led => "led",
            GaugeStyle::Arc => "arc",
        }
    }
}

/// Single-value gauge showing the latest sample within [min, max]. The `style`
/// field selects the shape; all styles share the min/max range and the color
/// thresholds.
pub struct GaugePanel {
    bound: Binding,
    label: Option<String>,
    min: f64,
    max: f64,
    /// Color cutoffs applied to the fill.
    colors: ColorThresholds,
    style: GaugeStyle,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = opt_str(cfg, "channel");
    let min = opt_f64(cfg, "min", 0.0);
    let max = opt_f64(cfg, "max", 100.0);
    if max <= min {
        anyhow::bail!("{TYPE_NAME} panel: max ({max}) must be greater than min ({min})");
    }
    Ok(Box::new(GaugePanel {
        bound: bind(&name, reg, ACCEPTED),
        label: opt_label(cfg),
        min,
        max,
        colors: ColorThresholds::from_config(cfg),
        style: GaugeStyle::parse(&opt_str(cfg, "style")),
    }))
}

pub(crate) fn fraction(v: f64, min: f64, max: f64) -> f32 {
    (((v - min) / (max - min)).clamp(0.0, 1.0)) as f32
}

/// Point on a circle of radius `r` around `center` at `deg` degrees, measured
/// from the +x axis with angles rising counterclockwise on screen (y is down,
/// so the sine is negated).
fn arc_point(center: Pos2, r: f32, deg: f64) -> Pos2 {
    let a = deg.to_radians();
    egui::pos2(center.x + r * a.cos() as f32, center.y - r * a.sin() as f32)
}

/// Stroke a circular arc from `from_deg` to `to_deg` as a polyline.
fn stroke_arc(
    painter: &egui::Painter,
    center: Pos2,
    r: f32,
    from_deg: f64,
    to_deg: f64,
    width: f32,
    color: Color32,
) {
    const STEPS: usize = 64;
    let pts: Vec<Pos2> = (0..=STEPS)
        .map(|i| {
            let t = i as f64 / STEPS as f64;
            arc_point(center, r, from_deg + (to_deg - from_deg) * t)
        })
        .collect();
    painter.add(egui::Shape::line(pts, Stroke::new(width, color)));
}

impl VizPanel for GaugePanel {
    fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.bound.name)
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        label_config_row(ui, &mut self.label, &self.bound.name);
        ui.horizontal(|ui| {
            ui.label("style:");
            ui.selectable_value(&mut self.style, GaugeStyle::Bar, "Bar");
            ui.selectable_value(&mut self.style, GaugeStyle::Vertical, "Vertical");
            ui.selectable_value(&mut self.style, GaugeStyle::Led, "LED");
            ui.selectable_value(&mut self.style, GaugeStyle::Arc, "Arc");
        });
        ui.horizontal(|ui| {
            ui.label("min:");
            ui.add(egui::DragValue::new(&mut self.min).speed(0.1));
            ui.label("max:");
            ui.add(egui::DragValue::new(&mut self.max).speed(0.1));
            if self.max <= self.min {
                self.max = self.min + 1.0;
            }
        });
        ui.separator();
        self.colors.config_ui(ui);
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
        // In sync mode read the value at the shared zoom window's end.
        let at = crate::viz::common::linked_window(ui.ctx())
            .map(|(_, end)| end)
            .unwrap_or_else(|| store.now_ns());
        let value = store.latest_at(id, at).and_then(|(_, s)| sample_as_f64(&s));
        match self.style {
            GaugeStyle::Bar => self.draw_bar(ui, value),
            GaugeStyle::Vertical => self.draw_vertical(ui, value),
            GaugeStyle::Led => self.draw_led(ui, value),
            GaugeStyle::Arc => self.draw_arc(ui, value),
        }
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        t.insert("min".to_string(), toml::Value::Float(self.min));
        t.insert("max".to_string(), toml::Value::Float(self.max));
        // Omit the default so bar gauges serialize exactly as before.
        if self.style != GaugeStyle::Bar {
            t.insert("style".to_string(), toml::Value::String(self.style.as_str().to_string()));
        }
        serialize_label(&mut t, &self.label);
        self.colors.write_config(&mut t);
        t
    }

    fn drop_channel(&mut self, name: &str, reg: &crate::config::ChannelRegistry) {
        self.bound = bind(name, reg, ACCEPTED);
    }

    fn refresh_bindings(&mut self, ctx: &RebindCtx) {
        refresh_binding(&mut self.bound, ACCEPTED, ctx);
    }

    fn data_channels(&self) -> Vec<ChannelId> {
        self.bound.id.into_iter().collect()
    }
}

impl GaugePanel {
    /// "value unit" for a reading, or an em dash when there is no sample.
    fn readout(&self, value: Option<f64>) -> String {
        match value {
            Some(v) => format!("{v:.3} {}", self.bound.unit),
            None => "—".to_string(),
        }
    }

    /// Fill color: the highest matching threshold, else the base color, else the
    /// channel's binding color.
    fn fill_color(&self, ui: &egui::Ui, value: Option<f64>) -> Color32 {
        value.and_then(|v| self.colors.color_for(v)).unwrap_or_else(|| {
            crate::viz::common::binding_color(&self.bound, ui.visuals().dark_mode)
        })
    }

    /// Text + outline that stays legible over a fill of `on` color: black on
    /// light fills, white on dark, outlined with the opposite so the part over
    /// the empty track reads too.
    fn readout_colors(on: Color32) -> (Color32, Color32) {
        if is_light(on) {
            (Color32::BLACK, Color32::WHITE)
        } else {
            (Color32::WHITE, Color32::BLACK)
        }
    }

    /// Horizontal bar filling left→right, with min/max labels beneath.
    fn draw_bar(&mut self, ui: &mut egui::Ui, value: Option<f64>) {
        let bar_color = self.fill_color(ui, value);
        let track = track_color(ui.visuals().dark_mode);
        let desired = egui::vec2(ui.available_width().max(80.0), 32.0);
        let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, track);
        if let Some(v) = value {
            let mut fill = rect;
            fill.set_width(rect.width() * fraction(v, self.min, self.max));
            painter.rect_filled(fill, 4.0, bar_color);
        }
        let (fg, outline) = Self::readout_colors(bar_color);
        outlined_text(
            painter,
            rect.center(),
            &self.readout(value),
            FontId::proportional(16.0),
            fg,
            outline,
        );
        self.range_row(ui);
    }

    /// Vertical bar filling bottom→top, with max/min labels beside the ends.
    fn draw_vertical(&mut self, ui: &mut egui::Ui, value: Option<f64>) {
        let bar_color = self.fill_color(ui, value);
        let track_col = track_color(ui.visuals().dark_mode);
        let text_col = ui.visuals().text_color();
        let avail = ui.available_size();
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(avail.x.max(48.0), avail.y.max(80.0)),
            Sense::hover(),
        );
        let bar_w = (rect.width() * 0.5).clamp(24.0, 64.0);
        let track = egui::Rect::from_center_size(
            egui::pos2(rect.center().x, rect.center().y),
            egui::vec2(bar_w, rect.height()),
        );
        let painter = ui.painter();
        painter.rect_filled(track, 4.0, track_col);
        if let Some(v) = value {
            let fill_h = track.height() * fraction(v, self.min, self.max);
            let fill = egui::Rect::from_min_max(
                egui::pos2(track.min.x, track.max.y - fill_h),
                track.max,
            );
            painter.rect_filled(fill, 4.0, bar_color);
        }
        let (fg, outline) = Self::readout_colors(bar_color);
        outlined_text(
            painter,
            track.center(),
            &self.readout(value),
            FontId::proportional(16.0),
            fg,
            outline,
        );
        let font = FontId::proportional(12.0);
        painter.text(
            egui::pos2(track.right() + 6.0, track.top()),
            Align2::LEFT_TOP,
            format!("{:.1}", self.max),
            font.clone(),
            text_col,
        );
        painter.text(
            egui::pos2(track.right() + 6.0, track.bottom()),
            Align2::LEFT_BOTTOM,
            format!("{:.1}", self.min),
            font,
            text_col,
        );
    }

    /// Row of discrete lit segments (VU meter). Each segment is colored by the
    /// threshold at the value it represents, so a green→amber→red ramp emerges
    /// naturally; unlit segments are dim.
    fn draw_led(&mut self, ui: &mut egui::Ui, value: Option<f64>) {
        const N: usize = 20;
        let fallback = self.fill_color(ui, value);
        let dark = ui.visuals().dark_mode;
        let desired = egui::vec2(ui.available_width().max(80.0), 32.0);
        let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, track_color(dark));
        let frac = value.map(|v| fraction(v, self.min, self.max)).unwrap_or(0.0);
        let lit = (frac * N as f32).round() as usize;
        let gap = 3.0;
        let seg_w = (rect.width() - gap * (N as f32 + 1.0)) / N as f32;
        for i in 0..N {
            let x0 = rect.left() + gap + i as f32 * (seg_w + gap);
            let seg = egui::Rect::from_min_size(
                egui::pos2(x0, rect.top() + gap),
                egui::vec2(seg_w.max(1.0), rect.height() - 2.0 * gap),
            );
            let col = if i < lit {
                let seg_val = self.min + ((i as f64 + 0.5) / N as f64) * (self.max - self.min);
                self.colors.color_for(seg_val).unwrap_or(fallback)
            } else {
                // Unlit segment: darker than the trough in Mocha, a touch
                // darker than it in Latte so it still reads as "off".
                if dark { Color32::from_gray(64) } else { Color32::from_gray(180) }
            };
            painter.rect_filled(seg, 2.0, col);
        }
        let (fg, outline) = Self::readout_colors(fallback);
        outlined_text(
            painter,
            rect.center(),
            &self.readout(value),
            FontId::proportional(16.0),
            fg,
            outline,
        );
        self.range_row(ui);
    }

    /// 270° dial arc sweeping from the lower-left up over the top to the
    /// lower-right, with the reading in the hub and min/max at the arc ends.
    fn draw_arc(&mut self, ui: &mut egui::Ui, value: Option<f64>) {
        // Sweep: lower-left (225°) clockwise to lower-right (-45°).
        const START: f64 = 225.0;
        const SWEEP: f64 = 270.0;
        let bar_color = self.fill_color(ui, value);
        let dark = ui.visuals().dark_mode;
        let text_col = ui.visuals().text_color();
        let avail = ui.available_size();
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(avail.x.max(80.0), avail.y.max(80.0)),
            Sense::hover(),
        );
        // Center below the geometric middle so the 270° arc's bottom gap fits.
        let center = egui::pos2(rect.center().x, rect.top() + rect.height() * 0.58);
        let radius = (rect.width().min(rect.height()) * 0.40).max(20.0);
        let thickness = (radius * 0.22).clamp(8.0, 22.0);
        let track = track_color(dark);
        let painter = ui.painter();
        stroke_arc(painter, center, radius, START, START - SWEEP, thickness, track);
        if let Some(v) = value {
            let end = START - SWEEP * fraction(v, self.min, self.max) as f64;
            stroke_arc(painter, center, radius, START, end, thickness, bar_color);
        }
        let (fg, outline) = if dark {
            (Color32::WHITE, Color32::BLACK)
        } else {
            (Color32::BLACK, Color32::WHITE)
        };
        outlined_text(
            painter,
            center,
            &self.readout(value),
            FontId::proportional(18.0),
            fg,
            outline,
        );
        let font = FontId::proportional(12.0);
        let lo = arc_point(center, radius, START);
        let hi = arc_point(center, radius, START - SWEEP);
        painter.text(
            lo + egui::vec2(0.0, thickness),
            Align2::CENTER_TOP,
            format!("{:.1}", self.min),
            font.clone(),
            text_col,
        );
        painter.text(
            hi + egui::vec2(0.0, thickness),
            Align2::CENTER_TOP,
            format!("{:.1}", self.max),
            font,
            text_col,
        );
    }

    /// The min (left) / max (right) label row beneath the horizontal styles.
    fn range_row(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("{:.1}", self.min));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{:.1}", self.max));
            });
        });
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
    fn light_bars_get_dark_text() {
        assert!(is_light(Color32::YELLOW));
        assert!(is_light(Color32::WHITE));
        assert!(!is_light(Color32::from_rgb(0xd6, 0x27, 0x28))); // palette red
        assert!(!is_light(Color32::BLACK));
    }

    #[test]
    fn thresholds_round_trip_through_config() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r##"type = "gauge"
channel = "demo.sine"
min = 0.0
max = 100.0

[[thresholds]]
value = 90.0
color = "#ff0000""##,
        )
        .unwrap();
        let out = reg.build(&e, &channels).unwrap().serialize();
        let arr = out.get("thresholds").and_then(|v| v.as_array()).unwrap();
        assert_eq!(arr[0].as_table().unwrap().get("color").and_then(|v| v.as_str()), Some("#ff0000"));
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
    fn style_round_trips_and_bar_is_omitted() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        // A non-default style serializes its key and round-trips.
        let e: PanelEntry = toml::from_str(
            r#"type = "gauge"
channel = "demo.sine"
min = 0.0
max = 100.0
style = "arc""#,
        )
        .unwrap();
        let out = reg.build(&e, &channels).unwrap().serialize();
        assert_eq!(out.get("style").and_then(|v| v.as_str()), Some("arc"));
        assert_eq!(out, e.config);

        // The default bar style omits the key entirely (config stability).
        let e: PanelEntry = toml::from_str(
            r#"type = "gauge"
channel = "demo.sine"
min = 0.0
max = 100.0
style = "bar""#,
        )
        .unwrap();
        let out = reg.build(&e, &channels).unwrap().serialize();
        assert!(out.get("style").is_none());
    }

    #[test]
    fn every_style_renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        store.write_numeric(id, 1, NumericVal::Float(3.0));
        let reg = PanelRegistry::with_builtins();
        for style in ["bar", "vertical", "led", "arc"] {
            let e: PanelEntry = toml::from_str(&format!(
                "type = \"gauge\"\nchannel = \"demo.sine\"\nstyle = \"{style}\""
            ))
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
}
