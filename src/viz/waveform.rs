use eframe::egui::{self, Color32};
use egui_phosphor::regular as icon;
use egui_plot::{Line, MarkerShape, Plot, PlotPoints, Points, VLine};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{SampleType, TimeWindow};
use crate::viz::common::{
    bind, binding_color, binding_error, effective_window_s, format_time_of_day, label_config_row,
    opt_bool, opt_f64_opt, opt_label, opt_str, opt_str_array, refresh_binding, serialize_label,
    shorten_common_prefix, snapshot_to_f64, window_config_row, Binding, RebindCtx,
};
use crate::viz::decimate::decimate_minmax;
use crate::viz::measure::{stats, Stats};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "waveform";

const ACCEPTED: &[SampleType] = &[SampleType::Float, SampleType::Int, SampleType::Bool];

/// Points fed to egui_plot per channel; ~2 per horizontal pixel is plenty.
const MAX_PLOT_BUCKETS: usize = 1000;

/// Minimum drag travel (screen pixels) on an axis before a box-drag zooms it,
/// so a barely-dragged click doesn't collapse the view to a near-zero span.
const MIN_DRAG_PX: f32 = 5.0;

/// Scrolling time-series plot with optional measurement cursors.
pub struct WaveformPanel {
    title: String,
    /// Custom tab label; `None` falls back to the first channel dropped in.
    label: Option<String>,
    bound: Vec<Binding>,
    /// Visible span in seconds; `None` follows the global default.
    time_window_s: Option<f64>,
    cursors: bool,
    /// Draw a marker on every actual sample, not just the connecting line.
    dots: bool,
    /// Unit suffix appended to Y-axis tick labels (and the cursor readout).
    /// Empty means no suffix.
    y_unit: String,
    /// Cursor positions in absolute ns so they stay put while the plot scrolls.
    cursor_a_ns: Option<i64>,
    cursor_b_ns: Option<i64>,
    /// Channel names toggled off via the legend (left-click). In-memory only.
    hidden: std::collections::HashSet<String>,
    /// Active horizontal time-zoom as an absolute-ns [start, end] range. When
    /// set the panel freezes live scrolling: both the data-fetch window and the
    /// plot x-bounds follow this range. In-memory only; double-click clears it.
    zoom: Option<(i64, i64)>,
    /// Active vertical zoom as an absolute Y-value [lo, hi] range. When set, the
    /// plot's y-bounds follow this range instead of auto-fitting. Unlike the X
    /// zoom it does NOT freeze horizontal scrolling — data keeps scrolling under
    /// a fixed Y window. In-memory only; double-click clears it.
    y_zoom: Option<(f64, f64)>,
    /// Screen position where the current zoom drag began, captured on
    /// drag-start. Needed on the release frame to know the box's opposite corner
    /// (the press origin is gone once the button is up). In-memory only.
    zoom_drag_origin: Option<egui::Pos2>,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let names = opt_str_array(cfg, "channels");
    let bound: Vec<Binding> = names.iter().map(|n| bind(n, reg, ACCEPTED)).collect();
    Ok(Box::new(WaveformPanel {
        title: names.join(", "),
        label: opt_label(cfg),
        bound,
        time_window_s: opt_f64_opt(cfg, "time_window_s"),
        cursors: opt_bool(cfg, "cursors", false),
        dots: opt_bool(cfg, "dots", false),
        y_unit: opt_str(cfg, "y_unit"),
        cursor_a_ns: None,
        cursor_b_ns: None,
        hidden: std::collections::HashSet::new(),
        zoom: None,
        y_zoom: None,
        zoom_drag_origin: None,
    }))
}

/// Decide which axes a completed zoom-drag box applies to.
///
/// `dx`/`dy` are the box's pixel width/height; `free` is true when Shift is
/// held. Plain drags snap to the dominant axis (X on a tie); free drags zoom
/// each axis independently. An axis only zooms if its travel clears
/// `MIN_DRAG_PX`. Returns `(zoom_x, zoom_y)`.
fn zoom_axes(dx: f32, dy: f32, free: bool) -> (bool, bool) {
    if free {
        (dx >= MIN_DRAG_PX, dy >= MIN_DRAG_PX)
    } else if dx >= dy {
        (dx >= MIN_DRAG_PX, false)
    } else {
        (false, dy >= MIN_DRAG_PX)
    }
}

/// Nearest actual sample timestamp to `ts` across all plotted channels, so
/// measurement cursors snap onto real samples instead of floating between them.
/// `None` only when nothing is plotted.
pub(crate) fn nearest_sample_ts(snaps: &[(usize, Vec<i64>, Vec<f64>)], ts: i64) -> Option<i64> {
    snaps
        .iter()
        .flat_map(|(_, tss, _)| tss.iter().copied())
        .min_by_key(|&t| (t - ts).abs())
}

/// Nearest sample (timestamp, value) within a single channel to `target`, used
/// to place a cursor marker directly on that channel's curve.
pub(crate) fn nearest_point(ts: &[i64], vals: &[f64], target: i64) -> Option<(i64, f64)> {
    ts.iter()
        .zip(vals)
        .min_by_key(|(&t, _)| (t - target).abs())
        .map(|(&t, &v)| (t, v))
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
        self.label
            .as_deref()
            .unwrap_or_else(|| self.bound.first().map(|b| b.name.as_str()).unwrap_or(""))
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        let default = self.bound.first().map(|b| b.name.clone()).unwrap_or_default();
        label_config_row(ui, &mut self.label, &default);
        ui.horizontal(|ui| {
            ui.label("y unit:");
            ui.add(
                egui::TextEdit::singleline(&mut self.y_unit)
                    .desired_width(80.0)
                    .hint_text("e.g. V"),
            );
        });
        ui.horizontal(|ui| {
            window_config_row(ui, &mut self.time_window_s, 0.1..=60.0);
            ui.checkbox(&mut self.cursors, "cursors");
            ui.checkbox(&mut self.dots, "dots");
            if ui.button("clear cursors").clicked() {
                self.cursor_a_ns = None;
                self.cursor_b_ns = None;
            }
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if self.bound.is_empty() {
            ui.label(egui::RichText::new("Drop channels here").weak());
            return;
        }
        for b in &self.bound {
            binding_error(ui, b, TYPE_NAME);
        }

        // Custom legend: left-click an entry toggles its visibility, right-click
        // opens a menu to remove the channel from the panel. (egui_plot's built-in
        // legend exposes no per-item right-click, so we draw our own.)
        let mut toggle: Option<String> = None;
        let mut remove_idx: Option<usize> = None;
        // Drop the path prefix common to every channel and show it once, so
        // entries carry only the part that distinguishes them.
        let names: Vec<&str> = self.bound.iter().map(|b| b.name.as_str()).collect();
        let (prefix, shorts) = shorten_common_prefix(&names);
        ui.horizontal_wrapped(|ui| {
            if !prefix.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(&prefix).weak()).selectable(false))
                    .on_hover_text("prefix shared by all channels");
            }
            for (i, b) in self.bound.iter().enumerate() {
                let color = binding_color(b);
                let hidden = self.hidden.contains(&b.name);
                let swatch = if hidden { Color32::GRAY } else { color };
                let text = egui::RichText::new(&shorts[i]).color(swatch);
                let resp = ui
                    .add(egui::Button::new(text).small().frame(false))
                    .on_hover_text("click: show/hide — right-click: remove");
                if resp.clicked() {
                    toggle = Some(b.name.clone());
                }
                resp.context_menu(|ui| {
                    if ui.button(format!("{} Remove channel", icon::TRASH)).clicked() {
                        remove_idx = Some(i);
                        ui.close_menu();
                    }
                });
            }
        });
        if let Some(name) = toggle {
            if !self.hidden.remove(&name) {
                self.hidden.insert(name);
            }
        }
        if let Some(i) = remove_idx {
            let b = self.bound.remove(i);
            self.hidden.remove(&b.name);
            self.title =
                self.bound.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ");
        }
        if self.bound.is_empty() {
            ui.label(egui::RichText::new("Drop channels here").weak());
            return;
        }

        let has_data = self
            .bound
            .iter()
            .filter(|b| b.type_ok)
            .filter_map(|b| b.id)
            .any(|id| store.latest(id).is_some());
        if !has_data {
            ui.label("no data");
            return;
        }
        // A horizontal zoom freezes the scrolling window and drives both the
        // data-fetch range and the plot x-bounds. Otherwise the window's right
        // edge tracks the store clock so the live scrub slider (and replay
        // position) drive the view instead of pinning to the newest sample.
        let win_s = effective_window_s(ui.ctx(), self.time_window_s);
        // One shared clock per frame, published by the app, so every waveform
        // uses the same live end and the same grid origin: equal windows then
        // start at the same time and grid lines coincide. Fall back to this
        // store when unpublished (e.g. headless panel tests).
        let clock =
            crate::viz::common::frame_clock(ui.ctx()).unwrap_or_else(|| store.now_ns());
        // When the toolbar link is armed, a shared time window (once any
        // waveform has zoomed) overrides this panel's own zoom; before the
        // first shared zoom (`linked == None`) the panel keeps its own view.
        let linked = if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
            crate::viz::common::linked_zoom_range(ui.ctx())
        } else {
            None
        };
        let (t0, end_ns) = match linked.or(self.zoom) {
            Some((a, b)) => (a, b),
            None => (clock - (win_s * 1e9) as i64, clock),
        };
        let window = TimeWindow { start_ns: t0, end_ns: end_ns + 1 };

        // Grid origin: a whole-second value shared by every waveform and frozen
        // for the session (seeded once from the shared clock). Shared so grid
        // lines coincide across panels; frozen so they never move — a per-frame
        // origin would jump a second at each whole-second boundary and shift the
        // grid for steps that do not divide one second. All x values are
        // (ns - anchor)/1e9; the include_x bounds, sample positions, and x-axis
        // formatter all add the origin back, so the visible window and tick
        // labels stay correct regardless of the origin's phase.
        let anchor = crate::viz::common::shared_epoch_ns(ui.ctx(), clock);
        let x_of = move |ns: i64| (ns - anchor) as f64 / 1e9;

        // Snapshots kept for the stats table below the plot.
        let mut snaps: Vec<(usize, Vec<i64>, Vec<f64>)> = Vec::new();
        for (i, b) in self.bound.iter().enumerate() {
            let (Some(id), true) = (b.id, b.type_ok) else { continue };
            let snap = store.snapshot(id, window);
            if let Some((ts, vals)) = snapshot_to_f64(&snap) {
                snaps.push((i, ts.to_vec(), vals));
            }
        }

        // Plot fills available height by default, which pushes the readout and
        // stats below it off the bottom of the pane. Reserve a strip so the
        // footer labels stay visible.
        let footer_h = if self.cursors { 120.0 } else { 24.0 };
        // Suffix ("" when unset) captured by the tick and hover formatters below.
        let unit = self.y_unit.clone();
        let unit_hover = unit.clone();
        let mut plot = Plot::new(("waveform", &self.title))
            // egui_plot's own pan/scroll/zoom are disabled so left-drag is free
            // for our horizontal time-zoom and the include_x bounds stay
            // authoritative (built-in zoom would flip auto_bounds off).
            .allow_drag(false)
            .allow_scroll(false)
            .allow_zoom(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false)
            .include_x(x_of(t0))
            .include_x(x_of(end_ns))
            // X is plotted relative to the fixed anchor; label the ticks with the
            // absolute UTC time of day so grid lines read as wall-clock time.
            .x_axis_formatter(move |mark, _| {
                format_time_of_day(anchor + (mark.value * 1e9) as i64)
            })
            .label_formatter(move |name, p| {
                let t = format_time_of_day(anchor + (p.x * 1e9) as i64);
                let suffix =
                    if unit_hover.is_empty() { String::new() } else { format!(" {unit_hover}") };
                if name.is_empty() {
                    format!("{t}\n{:.4}{suffix}", p.y)
                } else {
                    format!("{name}\n{t}\n{:.4}{suffix}", p.y)
                }
            })
            .height((ui.available_height() - footer_h).max(80.0));
        if !unit.is_empty() {
            // Append the unit to each Y tick, using decimals implied by the grid
            // step so float error in the tick position doesn't leak into the label.
            plot = plot.y_axis_formatter(move |mark, _| {
                let decimals = (-mark.step_size.log10().floor()).max(0.0) as usize;
                format!("{:.*} {unit}", decimals, mark.value)
            });
        }
        // A vertical zoom pins the y-bounds; without it Y stays auto-fit.
        if let Some((lo, hi)) = self.y_zoom {
            plot = plot.include_y(lo).include_y(hi);
        }
        let inner = plot.show(ui, |plot_ui| {
            for (i, ts, vals) in &snaps {
                let b = &self.bound[*i];
                if self.hidden.contains(&b.name) {
                    continue;
                }
                let points = decimate_minmax(ts, vals, anchor, MAX_PLOT_BUCKETS);
                let color = binding_color(b);
                plot_ui.line(Line::new(PlotPoints::from(points)).color(color).name(&b.name));
                if self.dots {
                    // Marker on each real sample in the window.
                    let pts: Vec<[f64; 2]> = ts
                        .iter()
                        .zip(vals)
                        .map(|(&t, &v)| [x_of(t), v])
                        .collect();
                    plot_ui.points(
                        Points::new(pts).color(color).shape(MarkerShape::Circle).radius(3.0_f32),
                    );
                }
            }
            if self.cursors {
                for (cur, color) in [
                    (self.cursor_a_ns, Color32::YELLOW),
                    (self.cursor_b_ns, Color32::LIGHT_BLUE),
                ] {
                    let Some(c) = cur else { continue };
                    plot_ui.vline(VLine::new(x_of(c)).color(color));
                    // Dot on each curve at its sample nearest the cursor, so the
                    // snap to real samples is visible instead of a bare line.
                    for (_, ts, vals) in &snaps {
                        if let Some((t, v)) = nearest_point(ts, vals, c) {
                            plot_ui.points(
                                Points::new(vec![[x_of(t), v]])
                                    .color(color)
                                    .shape(MarkerShape::Circle)
                                    .radius(4.0_f32),
                            );
                        }
                    }
                }
            }
            plot_ui.pointer_coordinate()
        });

        // Left-drag draws a selection box; releasing zooms. A plain drag snaps
        // to the dominant axis (full-height band → X, full-width band → Y); a
        // Shift-drag is a free both-axis box zoom. Double-click clears both
        // zooms. A plain click is not a drag, so this never fires cursor
        // placement below.
        let tf = &inner.transform;
        let frame = *tf.frame();
        let primary = egui::PointerButton::Primary;
        if inner.response.drag_started_by(primary) {
            self.zoom_drag_origin = inner.response.interact_pointer_pos();
        }
        if let (Some(p0), Some(cur)) =
            (self.zoom_drag_origin, inner.response.interact_pointer_pos())
        {
            let free = ui.input(|i| i.modifiers.shift);
            let x0 = p0.x.min(cur.x).clamp(frame.left(), frame.right());
            let x1 = p0.x.max(cur.x).clamp(frame.left(), frame.right());
            let y0 = p0.y.min(cur.y).clamp(frame.top(), frame.bottom());
            let y1 = p0.y.max(cur.y).clamp(frame.top(), frame.bottom());
            let (dx, dy) = (x1 - x0, y1 - y0);
            let (zx, zy) = zoom_axes(dx, dy, free);

            // Preview the region that will apply: full 2D box under Shift, else
            // a band on whichever axis is dominant. Drawn while dragging
            // regardless of threshold so the intent is visible immediately.
            if inner.response.dragged_by(primary) {
                let rect = if free {
                    egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
                } else if dx >= dy {
                    egui::Rect::from_min_max(
                        egui::pos2(x0, frame.top()),
                        egui::pos2(x1, frame.bottom()),
                    )
                } else {
                    egui::Rect::from_min_max(
                        egui::pos2(frame.left(), y0),
                        egui::pos2(frame.right(), y1),
                    )
                };
                ui.painter().rect_filled(rect, 0.0, Color32::from_white_alpha(24));
                ui.painter().rect_stroke(rect, 0.0, egui::Stroke::new(1.0_f32, Color32::WHITE));
            }
            if inner.response.drag_stopped_by(primary) {
                if zx {
                    let ns_at = |x: f32| {
                        anchor
                            + (tf.value_from_position(egui::pos2(x, frame.center().y)).x * 1e9)
                                as i64
                    };
                    let (a, b) = (ns_at(x0), ns_at(x1));
                    let new = Some((a.min(b), a.max(b)));
                    // While linked, propagate to every participant instead of
                    // zooming this panel alone.
                    if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
                        crate::viz::common::set_linked_zoom_range(ui.ctx(), new);
                    } else {
                        self.zoom = new;
                    }
                }
                if zy {
                    let val_at =
                        |y: f32| tf.value_from_position(egui::pos2(frame.center().x, y)).y;
                    // Screen y grows downward, so y0 (top) is the larger value.
                    let (a, b) = (val_at(y0), val_at(y1));
                    self.y_zoom = Some((a.min(b), a.max(b)));
                }
                self.zoom_drag_origin = None;
            }
        }
        if inner.response.double_clicked() {
            // While linked, releasing clears the shared window so every
            // participant returns to its own view together.
            if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
                crate::viz::common::set_linked_zoom_range(ui.ctx(), None);
            } else {
                self.zoom = None;
            }
            self.y_zoom = None; // Y is always local
        }

        if self.cursors && inner.response.clicked() {
            if let Some(p) = inner.inner {
                let raw_ts = anchor + (p.x * 1e9) as i64;
                let ts = nearest_sample_ts(&snaps, raw_ts).unwrap_or(raw_ts);
                let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
                if ctrl {
                    self.cursor_b_ns = Some(ts);
                } else {
                    self.cursor_a_ns = Some(ts);
                }
            }
        }

        // Readout of the placed cursors' coordinates: the snapped sample time
        // and, per channel, the value at that sample.
        for (cur, name) in [(self.cursor_a_ns, "A"), (self.cursor_b_ns, "B")] {
            let Some(c) = cur else { continue };
            let mut line = format!("cursor {name}: t = {} UTC", format_time_of_day(c));
            for (i, ts, vals) in &snaps {
                if let Some((_, v)) = nearest_point(ts, vals, c) {
                    line.push_str(&format!("  {} = {:.4}", self.bound[*i].name, v));
                }
            }
            ui.label(line);
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
        if let Some(w) = self.time_window_s {
            t.insert("time_window_s".to_string(), toml::Value::Float(w));
        }
        t.insert("cursors".to_string(), toml::Value::Boolean(self.cursors));
        t.insert("dots".to_string(), toml::Value::Boolean(self.dots));
        if !self.y_unit.is_empty() {
            t.insert("y_unit".to_string(), toml::Value::String(self.y_unit.clone()));
        }
        serialize_label(&mut t, &self.label);
        t
    }

    fn drop_channel(&mut self, name: &str, reg: &crate::config::ChannelRegistry) {
        if self.bound.iter().any(|b| b.name == name) {
            return;
        }
        self.bound.push(bind(name, reg, ACCEPTED));
        self.title = self.bound.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ");
    }

    fn refresh_bindings(&mut self, ctx: &RebindCtx) {
        for b in &mut self.bound {
            refresh_binding(b, ACCEPTED, ctx);
        }
    }

    fn freeze_time_zoom(&mut self, range: (i64, i64)) {
        self.zoom = Some(range);
    }

    fn reset_zoom(&mut self) {
        self.zoom = None;
        self.y_zoom = None;
        self.zoom_drag_origin = None;
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
cursors = true
dots = true"#,
        );
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.title(), "demo.sine");
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn missing_channels_key_builds_empty_panel() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e = entry(r#"type = "waveform""#);
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.title(), "");
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
        // No explicit window → key omitted so the panel follows the global default.
        assert!(!cfg.contains_key("time_window_s"));
        assert_eq!(cfg["cursors"], toml::Value::Boolean(false));
        assert_eq!(cfg["dots"], toml::Value::Boolean(false));
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
    fn cursor_snaps_to_nearest_sample() {
        let snaps = vec![
            (0usize, vec![0i64, 100, 200], vec![0.0, 1.0, 2.0]),
            (1usize, vec![50i64, 250], vec![9.0, 9.0]),
        ];
        // Between samples → snaps to the closest across all channels.
        assert_eq!(nearest_sample_ts(&snaps, 120), Some(100));
        assert_eq!(nearest_sample_ts(&snaps, 40), Some(50));
        assert_eq!(nearest_sample_ts(&snaps, 240), Some(250));
        // Exact hit stays put; empty input yields None.
        assert_eq!(nearest_sample_ts(&snaps, 200), Some(200));
        assert_eq!(nearest_sample_ts(&[], 10), None);
    }

    #[test]
    fn nearest_point_picks_closest_sample() {
        let ts = [0i64, 100, 200];
        let vals = [10.0, 11.0, 12.0];
        assert_eq!(nearest_point(&ts, &vals, 120), Some((100, 11.0)));
        assert_eq!(nearest_point(&ts, &vals, 190), Some((200, 12.0)));
        assert_eq!(nearest_point(&ts, &vals, 200), Some((200, 12.0)));
        assert_eq!(nearest_point(&[], &[], 5), None);
    }

    #[test]
    fn zoomed_window_fetches_frozen_range_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let sine = channels.id("demo.sine").unwrap();
        for i in 0..1000i64 {
            store.write_numeric(sine, i * 1_000_000, NumericVal::Float((i as f64 * 0.1).sin()));
        }
        let mut p = WaveformPanel {
            title: "demo.sine".into(),
            label: None,
            bound: vec![bind("demo.sine", &channels, ACCEPTED)],
            time_window_s: None,
            cursors: false,
            dots: false,
            y_unit: String::new(),
            cursor_a_ns: None,
            cursor_b_ns: None,
            hidden: std::collections::HashSet::new(),
            // Frozen sub-range in the middle of the written data.
            zoom: Some((200_000_000, 400_000_000)),
            y_zoom: None,
            zoom_drag_origin: None,
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| p.render(ui, &store));
        });
        // Zoom range is independent of the store clock (no now_ns pinning).
        assert_eq!(p.zoom, Some((200_000_000, 400_000_000)));
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

    #[test]
    fn zoom_axes_snap_and_free() {
        // Plain drag snaps to the dominant axis only.
        assert_eq!(zoom_axes(20.0, 3.0, false), (true, false)); // horizontal
        assert_eq!(zoom_axes(3.0, 20.0, false), (false, true)); // vertical
        // Tie resolves to X.
        assert_eq!(zoom_axes(10.0, 10.0, false), (true, false));
        // Dominant axis under threshold → nothing zooms.
        assert_eq!(zoom_axes(3.0, 2.0, false), (false, false));
        // Free drag zooms each axis independently over threshold.
        assert_eq!(zoom_axes(20.0, 20.0, true), (true, true));
        // Free drag with one axis under threshold zooms only the other.
        assert_eq!(zoom_axes(20.0, 2.0, true), (true, false));
        assert_eq!(zoom_axes(2.0, 20.0, true), (false, true));
    }

    #[test]
    fn y_zoomed_render_preserves_range_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let sine = channels.id("demo.sine").unwrap();
        for i in 0..1000i64 {
            store.write_numeric(sine, i * 1_000_000, NumericVal::Float((i as f64 * 0.1).sin()));
        }
        let mut p = WaveformPanel {
            title: "demo.sine".into(),
            label: None,
            bound: vec![bind("demo.sine", &channels, ACCEPTED)],
            time_window_s: None,
            cursors: false,
            dots: false,
            y_unit: String::new(),
            cursor_a_ns: None,
            cursor_b_ns: None,
            hidden: std::collections::HashSet::new(),
            zoom: None,
            // Vertical zoom set; horizontal scroll stays live.
            y_zoom: Some((-0.5, 0.5)),
            zoom_drag_origin: None,
        };
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| p.render(ui, &store));
        });
        // Y zoom is independent of the store clock, so it survives a render.
        assert_eq!(p.y_zoom, Some((-0.5, 0.5)));
    }

    #[test]
    fn linked_window_render_uses_shared_range_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        for i in 0..100i64 {
            store.write_numeric(id, i * 1_000_000, NumericVal::Float((i as f64).sin()));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            "type = \"waveform\"\nchannels = [\"demo.sine\"]",
        )
        .unwrap();
        let mut p = reg.build(&e, &channels).unwrap();

        let ctx = egui::Context::default();
        crate::viz::common::set_linked_zoom_enabled(&ctx, true);
        crate::viz::common::set_linked_zoom_range(&ctx, Some((10_000_000, 50_000_000)));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
        // A pure render must not disturb the shared range.
        assert_eq!(
            crate::viz::common::linked_zoom_range(&ctx),
            Some((10_000_000, 50_000_000))
        );
    }

    #[test]
    fn freeze_time_zoom_sets_local_zoom() {
        let mut p = WaveformPanel {
            title: String::new(),
            label: None,
            bound: Vec::new(),
            time_window_s: None,
            cursors: false,
            dots: false,
            y_unit: String::new(),
            cursor_a_ns: None,
            cursor_b_ns: None,
            hidden: std::collections::HashSet::new(),
            zoom: None,
            y_zoom: None,
            zoom_drag_origin: None,
        };
        p.freeze_time_zoom((5, 9));
        assert_eq!(p.zoom, Some((5, 9)));
    }

    #[test]
    fn shared_frame_clock_render_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        for i in 0..100i64 {
            store.write_numeric(id, i * 1_000_000, NumericVal::Float((i as f64).sin()));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry =
            toml::from_str("type = \"waveform\"\nchannels = [\"demo.sine\"]").unwrap();
        let mut p = reg.build(&e, &channels).unwrap();

        let ctx = egui::Context::default();
        crate::viz::common::set_frame_clock(&ctx, 1_700_000_000_500_000_000);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| p.render(ui, &store));
        });
        // A pure render must not disturb the published clock.
        assert_eq!(
            crate::viz::common::frame_clock(&ctx),
            Some(1_700_000_000_500_000_000)
        );
    }

    #[test]
    fn shared_grid_origin_is_whole_second_and_step_preserving() {
        // The grid origin is the whole-second floor of the shared clock. Two clocks
        // one whole second apart floor to origins one whole second apart, and any
        // origin is itself a whole second — the property that makes grid lines
        // coincide across panels sharing one clock.
        let floor = |c: i64| c - c.rem_euclid(1_000_000_000);
        let a = 1_700_000_000_500_000_000i64;
        let b = a + 1_000_000_000;
        assert_eq!(floor(a) % 1_000_000_000, 0);
        assert_eq!(floor(b) - floor(a), 1_000_000_000);
        // Sub-second jitter does not move the origin.
        assert_eq!(floor(a), floor(a + 123_456));
    }
}
