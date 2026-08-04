use std::collections::BTreeMap;

use eframe::egui::{self, Align2, Color32, FontId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::common::{
    bind, binding_error, effective_window_s, format_time_of_day, frame_clock, label_config_row,
    opt_f64_opt, opt_label, opt_str, refresh_binding, serialize_label, window_config_row, Binding,
    RebindCtx, PLOT_MARGIN_FRAC,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "state_graph";

const ACCEPTED: &[SampleType] = &[SampleType::Bool, SampleType::Int, SampleType::Text];

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
    /// Explicit code→label map from config, used for numeric (Int/Bool)
    /// channels. Empty for Text channels — their labels are the strings.
    states: BTreeMap<i64, String>,
    /// Runtime interning for Text channels: each distinct string is assigned a
    /// stable code in first-seen order so it maps onto the integer coloring and
    /// segment machinery. In-memory only; never serialized.
    text_codes: BTreeMap<String, i64>,
    text_labels: BTreeMap<i64, String>,
    /// Visible span in seconds; `None` follows the global default.
    time_window_s: Option<f64>,
    /// Active absolute-ns time window `[start, end]`. Set by the linked-zoom
    /// freeze (or a linked follow), it overrides the trailing window. This
    /// panel has no drag-zoom of its own — it only follows and freezes.
    /// In-memory only; not serialized.
    zoom: Option<(i64, i64)>,
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
        text_codes: BTreeMap::new(),
        text_labels: BTreeMap::new(),
        time_window_s: opt_f64_opt(cfg, "time_window_s"),
        zoom: None,
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

/// Runs from [`segments`], stretched to cover the whole `[t0, end_ns]` window:
/// the first run fills back to the left edge and the last run forward to
/// `end_ns`. A state is piecewise-constant, so the earliest known value holds
/// for the unpainted region before it, and the latest value holds up to "now".
///
/// Without the left-fill, a channel whose samples cover only part of the window
/// — e.g. a high-rate `Text` channel whose fixed-line ring holds just the last
/// few milliseconds — paints a thin sliver at the right edge and leaves the
/// rest black.
pub(crate) fn painted_spans(
    ts: &[i64],
    vals: &[i64],
    t0: i64,
    end_ns: i64,
) -> Vec<(i64, i64, i64)> {
    let mut segs = segments(ts, vals);
    if let Some(first) = segs.first_mut() {
        first.0 = first.0.min(t0);
    }
    if let Some(last) = segs.last_mut() {
        last.1 = last.1.max(end_ns);
    }
    segs
}

/// A "nice" axis tick step (ns) for `span_ns`, aiming for ~`target` ticks,
/// snapped to a 1/2/5 × 10ⁿ value so ticks land on round times — the same
/// mantissa progression egui_plot uses for the waveform x-axis.
pub(crate) fn nice_time_step_ns(span_ns: i64, target: i64) -> i64 {
    let rough = (span_ns / target.max(1)).max(1) as f64;
    let mag = 10f64.powf(rough.log10().floor());
    let norm = rough / mag; // 1.0..10.0
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

impl StateGraphPanel {
    /// Map a Text value to a stable integer code, assigning the next code in
    /// first-seen order on first sight. Codes persist for the panel's lifetime,
    /// so a given string keeps its color and legend slot across renders.
    fn intern(&mut self, s: &str) -> i64 {
        if let Some(&code) = self.text_codes.get(s) {
            return code;
        }
        let code = self.text_codes.len() as i64;
        self.text_codes.insert(s.to_string(), code);
        self.text_labels.insert(code, s.to_string());
        code
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
        // Discovered text arrives as interned codes on the ring path (an `Int`
        // snapshot) with this code→label table; `None` for numeric channels and
        // for verbatim `TextBuf` channels (which come through as `Text`).
        let coded = store.state_labels(id);
        // When the toolbar link is armed, a shared time window overrides this
        // panel's trailing view; before any shared zoom (`linked == None`) it
        // keeps its own frozen zoom, else the live trailing window.
        let linked = if crate::viz::common::linked_zoom_enabled(ui.ctx()) {
            crate::viz::common::linked_zoom_range(ui.ctx())
        } else {
            None
        };
        let (t0, end_ns) = match linked.or(self.zoom) {
            Some((a, b)) => (a, b),
            None => {
                // Same right edge as the waveforms: the app's shared frame clock.
                let end = frame_clock(ui.ctx()).unwrap_or_else(|| store.now_ns());
                let span = (effective_window_s(ui.ctx(), self.time_window_s) * 1e9) as i64;
                (end - span, end)
            }
        };
        // Visible x-range = the data window plus egui_plot's margin on each side,
        // so bands and ticks line up with — and scroll at the same rate as — the
        // waveforms. `vt0..vend` drives x mapping; the data window `t0..end_ns`
        // still bounds the snapshot and the painted bands.
        let span = (end_ns - t0).max(1);
        let margin = (span as f64 * PLOT_MARGIN_FRAC as f64) as i64;
        let (vt0, vend) = (t0 - margin, end_ns + margin);
        let vspan = (vend - vt0).max(1);
        let snap = store.snapshot(id, TimeWindow { start_ns: t0, end_ns: end_ns + 1 });
        let mut text_mode = false;
        let (ts, vals): (Vec<i64>, Vec<i64>) = match &snap {
            ChannelSnapshot::Int { ts, vals } => (ts.clone(), vals.clone()),
            ChannelSnapshot::Bool { ts, vals } => {
                (ts.clone(), vals.iter().map(|&v| v as i64).collect())
            }
            ChannelSnapshot::Text { lines } => {
                text_mode = true;
                let ts = lines.iter().map(|(t, _)| *t).collect();
                let vals = lines.iter().map(|(_, s)| self.intern(s)).collect();
                (ts, vals)
            }
            _ => return,
        };
        // Legend / label source, in priority: the discovered coded-text table
        // (indexed by code), then interned strings for a verbatim `Text`
        // channel, then the config `states` map for a numeric channel.
        let legend: Vec<(i64, String)> = if let Some(lbls) = &coded {
            lbls.iter().enumerate().map(|(i, s)| (i as i64, s.clone())).collect()
        } else if text_mode {
            self.text_labels.iter().map(|(k, v)| (*k, v.clone())).collect()
        } else {
            self.states.iter().map(|(k, v)| (*k, v.clone())).collect()
        };
        let label_for = |v: i64| -> String {
            legend
                .iter()
                .find(|(code, _)| *code == v)
                .map(|(_, s)| s.clone())
                .unwrap_or_else(|| v.to_string())
        };
        let desired = egui::vec2(ui.available_width().max(80.0), 40.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 2.0, Color32::from_gray(30));
        let x_of = |t: i64| {
            rect.left() + rect.width() * ((t - vt0) as f32 / vspan as f32)
        };
        for (s, e, v) in painted_spans(&ts, &vals, t0, end_ns) {
            let seg = egui::Rect::from_min_max(
                egui::pos2(x_of(s), rect.top()),
                egui::pos2(x_of(e), rect.bottom()),
            );
            painter.rect_filled(seg, 0.0, color_for(v));
            if seg.width() > 40.0 {
                painter.text(
                    seg.center(),
                    Align2::CENTER_CENTER,
                    label_for(v),
                    FontId::proportional(12.0),
                    Color32::BLACK,
                );
            }
        }
        // Time axis: ticks at round UTC times (like the waveform x-axis), so
        // they stay put and slide left as the live window advances instead of
        // jittering at fixed fractions of the span.
        let (axis_rect, _) =
            ui.allocate_exact_size(egui::vec2(rect.width(), 15.0), egui::Sense::hover());
        let axis_painter = ui.painter();
        let step = nice_time_step_ns(vspan, 5);
        let mut t = vt0.div_euclid(step) * step;
        if t < vt0 {
            t += step;
        }
        while t <= vend {
            let x = x_of(t);
            axis_painter.line_segment(
                [egui::pos2(x, axis_rect.top()), egui::pos2(x, axis_rect.top() + 3.0)],
                egui::Stroke::new(1.0_f32, Color32::from_gray(110)),
            );
            // Edge-anchor the labels nearest each border so they don't clip.
            let anchor = if x - rect.left() < 24.0 {
                Align2::LEFT_TOP
            } else if rect.right() - x < 24.0 {
                Align2::RIGHT_TOP
            } else {
                Align2::CENTER_TOP
            };
            axis_painter.text(
                egui::pos2(x, axis_rect.top() + 3.0),
                anchor,
                format_time_of_day(t),
                FontId::proportional(9.0),
                Color32::from_gray(160),
            );
            t += step;
        }
        // Legend of known states.
        ui.horizontal_wrapped(|ui| {
            for (v, label) in &legend {
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

    fn freeze_time_zoom(&mut self, range: (i64, i64)) {
        self.zoom = Some(range);
    }

    fn reset_zoom(&mut self) {
        self.zoom = None;
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

[channels."motor.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
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
    fn painted_spans_fill_whole_window_from_late_sliver() {
        // Samples only in the tail of a wide window (a high-rate ring holds
        // just the last few ms). The painted spans must still tile [t0, end]
        // with no gap — otherwise the panel is black except a right-edge sliver.
        let (t0, end) = (0i64, 10_000i64);
        let ts = [9_990i64, 9_995, 10_000];
        let vals = [1i64, 2, 2];
        let spans = painted_spans(&ts, &vals, t0, end);
        assert_eq!(spans.first().unwrap().0, t0, "first span must reach left edge");
        assert_eq!(spans.last().unwrap().1, end, "last span must reach right edge");
        // Contiguous coverage, no gaps.
        let mut cursor = t0;
        for (s, e, _) in &spans {
            assert_eq!(*s, cursor, "gap in coverage");
            cursor = *e;
        }
        assert_eq!(cursor, end);
    }

    #[test]
    fn painted_spans_empty_without_data() {
        assert!(painted_spans(&[], &[], 0, 10).is_empty());
    }

    #[test]
    fn nice_time_step_snaps_to_1_2_5() {
        // 10s window, ~5 ticks → 2s step.
        assert_eq!(nice_time_step_ns(10_000_000_000, 5), 2_000_000_000);
        // 1s window → 200ms.
        assert_eq!(nice_time_step_ns(1_000_000_000, 5), 200_000_000);
        // 60s window → 10s.
        assert_eq!(nice_time_step_ns(60_000_000_000, 5), 10_000_000_000);
        // Never zero on a tiny span.
        assert!(nice_time_step_ns(1, 5) >= 1);
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

    /// Bare panel bound to a channel, no config `states` map — for unit-testing
    /// the interner directly (the trait object hides the concrete type).
    fn panel_for(channel: &str, reg: &ChannelRegistry) -> StateGraphPanel {
        StateGraphPanel {
            bound: bind(channel, reg, ACCEPTED),
            label: None,
            states: BTreeMap::new(),
            text_codes: BTreeMap::new(),
            text_labels: BTreeMap::new(),
            time_window_s: None,
            zoom: None,
        }
    }

    #[test]
    fn text_channel_binds() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        // ACCEPTED includes Text, so a text channel resolves rather than erroring.
        let e: PanelEntry =
            toml::from_str("type = \"state_graph\"\nchannel = \"motor.log\"").unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.title(), "motor.log");
    }

    #[test]
    fn intern_assigns_stable_first_seen_codes() {
        let channels = registry();
        let mut p = panel_for("motor.log", &channels);
        assert_eq!(p.intern("idle"), 0);
        assert_eq!(p.intern("running"), 1);
        assert_eq!(p.intern("error"), 2);
        // Repeats return the same code; codes and labels stay consistent.
        assert_eq!(p.intern("idle"), 0);
        assert_eq!(p.intern("running"), 1);
        assert_eq!(p.text_labels.get(&0).map(String::as_str), Some("idle"));
        assert_eq!(p.text_labels.get(&2).map(String::as_str), Some("error"));
    }

    #[test]
    fn renders_text_channel_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let log = channels.id("motor.log").unwrap();
        for (i, s) in ["idle", "idle", "running", "error", "idle"].iter().enumerate() {
            store.write_text(log, i as i64 * 1_000_000, s.to_string());
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry =
            toml::from_str("type = \"state_graph\"\nchannel = \"motor.log\"").unwrap();
        let mut p = reg.build(&e, &channels).unwrap();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
    }

    #[test]
    fn text_channel_in_trailing_window_draws_bands() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let log = channels.id("motor.log").unwrap();
        // Timestamps within the last second so they land inside the trailing
        // window the panel derives from `store.now_ns()`.
        let now = store.now_ns();
        for (i, s) in ["idle", "idle", "running", "error", "idle"].iter().enumerate() {
            store.write_text(log, now - (5 - i as i64) * 100_000_000, s.to_string());
        }
        let mut p = panel_for("motor.log", &channels);
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
        // If the Text snapshot reached the panel, the strings were interned.
        assert!(!p.text_codes.is_empty(), "no text interned — panel drew nothing");
        assert_eq!(p.text_codes.len(), 3, "idle/running/error");
    }

    #[test]
    fn discovered_text_topic_binds_and_draws() {
        use crate::dynamic_channel::MqttTopicMap;
        use std::collections::HashMap;
        use std::sync::RwLock;

        // Mirrors the config panel `channel = "load/state0"`: the channel does
        // not exist until the ws/influx stream discovers it live.
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let mut p = panel_for("load/state0", &channels);
        assert!(p.bound.id.is_none(), "topic must start unresolved");

        // First string arrives on the wire → discovery snapshot carries it.
        let topic_map: MqttTopicMap = RwLock::new(HashMap::new());
        let mut snap = BTreeMap::new();
        snap.insert("load/state0".to_string(), "idle".to_string());
        p.refresh_bindings(&RebindCtx {
            channels: &channels,
            store: &store,
            mqtt: Some((&topic_map, &snap)),
        });
        assert!(p.bound.id.is_some(), "discovered text topic did not bind");
        assert!(p.bound.type_ok, "state graph rejected the text channel");

        // A couple more in-window transitions on top of the seeded value.
        let id = p.bound.id.unwrap();
        let now = store.now_ns();
        store.write_text(id, now - 300_000_000, "running".to_string());
        store.write_text(id, now - 100_000_000, "error".to_string());
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
        // Discovered text is stored as interned codes on the ring, so the labels
        // live on the store (not the panel's `text_codes`, which only backs the
        // verbatim `TextBuf` path).
        let labels = store.state_labels(id).expect("discovered text must be coded");
        assert_eq!(labels, vec!["idle", "running", "error"]);
    }

    #[test]
    fn linked_window_render_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let motor = channels.id("motor.state").unwrap();
        for i in 0..50i64 {
            store.write_numeric(motor, i * 1_000_000, NumericVal::Int(i / 20));
        }
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            "type = \"state_graph\"\nchannel = \"motor.state\"",
        )
        .unwrap();
        let mut p = reg.build(&e, &channels).unwrap();

        let ctx = egui::Context::default();
        crate::viz::common::set_linked_zoom_enabled(&ctx, true);
        crate::viz::common::set_linked_zoom_range(&ctx, Some((5_000_000, 30_000_000)));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
    }

    #[test]
    fn freeze_then_reset_zoom() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            "type = \"state_graph\"\nchannel = \"motor.state\"",
        )
        .unwrap();
        let mut p = reg.build(&e, &channels).unwrap();
        p.freeze_time_zoom((5, 9));
        // reset_zoom must clear it back to the trailing/live view.
        p.reset_zoom();
        // A render after reset must still not panic (no lingering bad range).
        let store = LiveStore::from_registry(&channels);
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                p.render(ui, &store);
            });
        });
    }
}
