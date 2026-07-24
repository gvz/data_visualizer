use std::collections::BTreeMap;

use anyhow::anyhow;
use eframe::egui::{self, Color32};

use crate::config::ChannelRegistry;
use crate::dynamic_channel::{resolve_or_register_drop, MqttTopicMap};
use crate::store::ChannelStore;
use crate::types::{ChannelId, ChannelSnapshot, Sample, SampleType};

/// A panel's link to one channel: resolved id + validity + display metadata.
pub struct Binding {
    pub name: String,
    pub id: Option<ChannelId>,
    pub type_ok: bool,
    pub unit: String,
    pub color: Color32,
    /// True when the channel has no explicit color configured — the panel
    /// should assign one from the palette (see `palette_color`).
    pub auto_color: bool,
}

/// Qualitative palette (Tableau 10) for auto-coloring plotted series. Distinct,
/// colorblind-friendlier than a single default gray.
pub const PALETTE: &[Color32] = &[
    Color32::from_rgb(0x1f, 0x77, 0xb4), // blue
    Color32::from_rgb(0xff, 0x7f, 0x0e), // orange
    Color32::from_rgb(0x2c, 0xa0, 0x2c), // green
    Color32::from_rgb(0xd6, 0x27, 0x28), // red
    Color32::from_rgb(0x94, 0x67, 0xbd), // purple
    Color32::from_rgb(0x8c, 0x56, 0x4b), // brown
    Color32::from_rgb(0xe3, 0x77, 0xc2), // pink
    Color32::from_rgb(0xbc, 0xbd, 0x22), // olive
    Color32::from_rgb(0x17, 0xbe, 0xcf), // cyan
    Color32::from_rgb(0x7f, 0x7f, 0x7f), // gray
];

/// Palette color for the i-th series (wraps around).
pub fn palette_color(i: usize) -> Color32 {
    PALETTE[i % PALETTE.len()]
}

/// The channel-config default color; channels using it get palette colors.
const DEFAULT_CHANNEL_COLOR: &str = "#cccccc";

/// Resolve a binding's line color: its explicit config color, or a palette
/// color keyed by `series` when the channel left the color at the default.
pub fn binding_color(b: &Binding, series: usize) -> Color32 {
    if b.auto_color {
        palette_color(series)
    } else {
        b.color
    }
}

/// Resolve a channel name. Unknown names and wrong types still produce a
/// Binding (panels render the problem inline; ctors must not fail on it).
pub fn bind(name: &str, reg: &ChannelRegistry, accepted: &[SampleType]) -> Binding {
    match reg.id(name) {
        Some(id) => {
            let m = reg.meta(id);
            Binding {
                name: name.to_string(),
                id: Some(id),
                type_ok: accepted.contains(&m.sample_type),
                unit: m.unit.clone(),
                color: parse_hex_color(&m.color),
                auto_color: m.color.trim().eq_ignore_ascii_case(DEFAULT_CHANNEL_COLOR),
            }
        }
        None => Binding {
            name: name.to_string(),
            id: None,
            type_ok: true,
            unit: String::new(),
            color: Color32::GRAY,
            auto_color: true,
        },
    }
}

/// Context for re-resolving a panel's unknown channels against newly-discovered
/// MQTT topics — mirrors the drop path, but keyed by the binding's own name.
pub struct RebindCtx<'a> {
    pub channels: &'a ChannelRegistry,
    pub store: &'a dyn ChannelStore,
    /// Shared routing table + discovered-topic snapshot; `None` outside Live.
    pub mqtt: Option<(&'a MqttTopicMap, &'a BTreeMap<String, String>)>,
}

/// Re-attempt to resolve one binding whose channel is currently unknown. If the
/// binding's name is now a registered channel — or a discovered MQTT topic that
/// can be registered on the fly — the binding is rebound in place. No-op for
/// empty or already-resolved bindings.
pub fn refresh_binding(b: &mut Binding, accepted: &[SampleType], ctx: &RebindCtx) {
    if b.id.is_some() || b.name.is_empty() {
        return;
    }
    if let Some(name) = resolve_or_register_drop(&b.name, ctx.channels, ctx.store, ctx.mqtt) {
        *b = bind(&name, ctx.channels, accepted);
    }
}

/// Render the standard inline error for a broken binding.
/// Returns true if the binding is unusable (caller should skip the channel).
pub fn binding_error(ui: &mut egui::Ui, b: &Binding, panel: &str) -> bool {
    if b.id.is_none() {
        ui.colored_label(Color32::RED, format!("unknown channel `{}`", b.name));
        return true;
    }
    if !b.type_ok {
        ui.colored_label(
            Color32::RED,
            format!("channel `{}` type not supported by {panel} panel", b.name),
        );
        return true;
    }
    false
}

/// "#rrggbb" → Color32; anything unparsable → gray.
pub fn parse_hex_color(s: &str) -> Color32 {
    let hex = s.trim_start_matches('#');
    if hex.len() == 6 {
        if let (Ok(r), Ok(g), Ok(b)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        ) {
            return Color32::from_rgb(r, g, b);
        }
    }
    Color32::GRAY
}

/// Numeric snapshot as (borrowed ts, owned f64 values). None for Text.
pub fn snapshot_to_f64(snap: &ChannelSnapshot) -> Option<(&[i64], Vec<f64>)> {
    match snap {
        ChannelSnapshot::Float { ts, vals } => Some((ts, vals.clone())),
        ChannelSnapshot::Int { ts, vals } => {
            Some((ts, vals.iter().map(|&v| v as f64).collect()))
        }
        ChannelSnapshot::Bool { ts, vals } => {
            Some((ts, vals.iter().map(|&v| v as f64).collect()))
        }
        ChannelSnapshot::Text { .. } => None,
    }
}

pub fn sample_as_f64(s: &Sample) -> Option<f64> {
    match s {
        Sample::Float(v) => Some(*v),
        Sample::Int(v) => Some(*v as f64),
        Sample::Bool(b) => Some(u8::from(*b) as f64),
        Sample::Text(_) => None,
    }
}

/// "HH:MM:SS.mmm" (UTC time of day) from ns since Unix epoch.
pub fn format_time_of_day(ts_ns: i64) -> String {
    let secs = ts_ns.div_euclid(1_000_000_000);
    let millis = ts_ns.rem_euclid(1_000_000_000) / 1_000_000;
    let s = secs.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600, (s % 3600) / 60, s % 60, millis)
}

// ---- legend name shortening ----

/// Split `s` into `(segment, separator)` tokens on `/` and `.` (MQTT topics and
/// dotted channel ids). The final token's separator is `'\0'`. Keeping the
/// separator per token lets a name be split on shared segments and rebuilt
/// without guessing which delimiter joined them.
fn tokenize_path(s: &str) -> Vec<(&str, char)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, c) in s.char_indices() {
        if c == '/' || c == '.' {
            out.push((&s[start..i], c));
            start = i + c.len_utf8();
        }
    }
    out.push((&s[start..], '\0'));
    out
}

/// Factor out the path prefix shared by every name so a legend can show it once
/// instead of on each entry. Returns `(shared_prefix, shorts)` where `shorts[i]`
/// is `names[i]` with that prefix removed and `format!("{shared_prefix}{short}")`
/// reproduces the original. Only whole leading segments count, and each name
/// always keeps its final segment, so `shorts` never has an empty entry.
/// `shared_prefix` is `""` (and `shorts` mirrors `names`) when fewer than two
/// names are given or they share no leading segment.
pub fn shorten_common_prefix(names: &[&str]) -> (String, Vec<String>) {
    if names.len() < 2 {
        return (String::new(), names.iter().map(|s| s.to_string()).collect());
    }
    let toks: Vec<Vec<(&str, char)>> = names.iter().map(|s| tokenize_path(s)).collect();
    // Each name must keep its last token, so only len-1 tokens are shareable.
    let cap = toks.iter().map(|t| t.len().saturating_sub(1)).min().unwrap_or(0);
    let mut shared = 0;
    while shared < cap && toks[1..].iter().all(|t| t[shared] == toks[0][shared]) {
        shared += 1;
    }
    if shared == 0 {
        return (String::new(), names.iter().map(|s| s.to_string()).collect());
    }
    let render = |toks: &[(&str, char)]| -> String {
        toks.iter()
            .map(|(seg, sep)| if *sep == '\0' { seg.to_string() } else { format!("{seg}{sep}") })
            .collect()
    };
    let prefix = render(&toks[0][..shared]);
    let shorts = toks.iter().map(|t| render(&t[shared..])).collect();
    (prefix, shorts)
}

// ---- panel-config accessors (ctor helpers) ----

pub fn req_str(cfg: &toml::Table, key: &str, panel: &str) -> anyhow::Result<String> {
    cfg.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{panel} panel: missing string key `{key}`"))
}

pub fn req_str_array(cfg: &toml::Table, key: &str, panel: &str) -> anyhow::Result<Vec<String>> {
    let arr = cfg
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("{panel} panel: missing array key `{key}`"))?;
    let names: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    if names.is_empty() {
        return Err(anyhow!("{panel} panel: `{key}` is empty"));
    }
    Ok(names)
}

pub fn opt_f64(cfg: &toml::Table, key: &str, default: f64) -> f64 {
    match cfg.get(key) {
        Some(toml::Value::Float(f)) => *f,
        Some(toml::Value::Integer(i)) => *i as f64,
        _ => default,
    }
}

/// Like `opt_f64` but returns `None` when the key is absent — used for panel
/// settings that fall back to a global default rather than a fixed literal.
pub fn opt_f64_opt(cfg: &toml::Table, key: &str) -> Option<f64> {
    match cfg.get(key) {
        Some(toml::Value::Float(f)) => Some(*f),
        Some(toml::Value::Integer(i)) => Some(*i as f64),
        _ => None,
    }
}

// ---- global visible-time-window default ----

/// Fallback visible span (seconds) when neither the panel nor the app set one.
pub const DEFAULT_WINDOW_S: f64 = 10.0;

fn global_window_id() -> egui::Id {
    egui::Id::new("datavis_global_window_s")
}

/// The app-wide default visible time span in seconds. The app publishes this
/// into egui's ctx data each frame; panels read it here (no signature change).
pub fn global_window_s(ctx: &egui::Context) -> f64 {
    ctx.data(|d| d.get_temp::<f64>(global_window_id()))
        .unwrap_or(DEFAULT_WINDOW_S)
}

/// Publish the app-wide default visible time span so panels can read it.
pub fn set_global_window_s(ctx: &egui::Context, secs: f64) {
    ctx.data_mut(|d| d.insert_temp(global_window_id(), secs));
}

fn linked_zoom_enabled_id() -> egui::Id {
    egui::Id::new("datavis_linked_zoom_enabled")
}

fn linked_zoom_range_id() -> egui::Id {
    egui::Id::new("datavis_linked_zoom_range")
}

/// Whether linked time-zoom is armed (the toolbar checkbox). The app
/// republishes this into ctx data each frame; time-based panels read it during
/// render. Absent (e.g. headless panel tests) → false.
pub fn linked_zoom_enabled(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(linked_zoom_enabled_id())).unwrap_or(false)
}

/// Publish whether linked time-zoom is armed.
pub fn set_linked_zoom_enabled(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(linked_zoom_enabled_id(), on));
}

/// The shared absolute-ns time window `[start, end]` while linked, or `None`
/// when armed but not yet zoomed. A waveform's zoom gesture writes it; every
/// participating panel reads it. Absent → None.
pub fn linked_zoom_range(ctx: &egui::Context) -> Option<(i64, i64)> {
    ctx.data(|d| d.get_temp::<Option<(i64, i64)>>(linked_zoom_range_id())).flatten()
}

/// Publish (or clear, with `None`) the shared linked time window.
pub fn set_linked_zoom_range(ctx: &egui::Context, range: Option<(i64, i64)>) {
    ctx.data_mut(|d| d.insert_temp(linked_zoom_range_id(), range));
}

/// The absolute-ns window `[start, end]` a panel should evaluate against this
/// frame, or `None` for its own live/trailing view. `Some` only in sync mode
/// (the toolbar link-zoom checkbox armed) with an active shared range: then
/// every panel reads the same window, so a numeric/gauge shows the value at
/// `end`, a spectrum/xy_scatter computes over `[start, end]`, and a log lists
/// only lines in `[start, end]` — all consistent with the zoomed waveform.
pub fn linked_window(ctx: &egui::Context) -> Option<(i64, i64)> {
    if linked_zoom_enabled(ctx) {
        linked_zoom_range(ctx)
    } else {
        None
    }
}

fn frame_clock_id() -> egui::Id {
    egui::Id::new("datavis_frame_clock_ns")
}

/// The active store clock for this frame, published once by the app so every
/// panel shares one value instead of each sampling `now_ns()` independently.
/// `None` when unpublished (e.g. headless panel tests); callers fall back to
/// their own `store.now_ns()`.
pub fn frame_clock(ctx: &egui::Context) -> Option<i64> {
    ctx.data(|d| d.get_temp::<i64>(frame_clock_id()))
}

/// Publish the shared per-frame clock. Called once per frame by the app.
pub fn set_frame_clock(ctx: &egui::Context, ns: i64) {
    ctx.data_mut(|d| d.insert_temp(frame_clock_id(), ns));
}

fn shared_epoch_id() -> egui::Id {
    egui::Id::new("datavis_shared_epoch_ns")
}

/// A whole-second plot origin shared by every waveform and frozen for the
/// session: seeded once from `seed` (the first caller's clock, floored to a
/// whole second) and returned unchanged thereafter.
///
/// It must be BOTH shared and frozen. Shared so all panels plot against the
/// same origin and their grid lines coincide. Frozen so the origin never moves
/// between frames: a per-frame origin would jump by one second at each
/// whole-second boundary, and for grid steps that do not divide one second
/// (2 s, 5 s, ...) that shifts every grid line to a different absolute time —
/// the grid appears to change. A fixed origin keeps grid lines pinned to
/// absolute time; the tick labels add the origin back, so they stay correct
/// regardless of the origin's phase. Absent (headless tests) → seeded on first
/// call.
pub fn shared_epoch_ns(ctx: &egui::Context, seed: i64) -> i64 {
    let id = shared_epoch_id();
    if let Some(v) = ctx.data(|d| d.get_temp::<i64>(id)) {
        return v;
    }
    let anchor = seed - seed.rem_euclid(1_000_000_000);
    ctx.data_mut(|d| d.insert_temp(id, anchor));
    anchor
}

/// Effective window for a panel: its explicit override, else the global default.
pub fn effective_window_s(ctx: &egui::Context, override_s: Option<f64>) -> f64 {
    override_s.unwrap_or_else(|| global_window_s(ctx))
}

/// Config-UI row for a panel's time window: a checkbox toggling between the
/// global default and a per-panel override slider (`range` in seconds).
pub fn window_config_row(
    ui: &mut egui::Ui,
    window: &mut Option<f64>,
    range: std::ops::RangeInclusive<f64>,
) {
    let g = global_window_s(ui.ctx());
    let mut override_on = window.is_some();
    if ui.checkbox(&mut override_on, "override window").changed() {
        *window = override_on.then_some(g);
    }
    match window {
        Some(w) => {
            ui.add(egui::Slider::new(w, range).logarithmic(true).suffix(" s"));
        }
        None => {
            ui.label(format!("global: {g:.1} s"));
        }
    }
}

pub fn opt_i64(cfg: &toml::Table, key: &str, default: i64) -> i64 {
    match cfg.get(key) {
        Some(toml::Value::Integer(i)) => *i,
        _ => default,
    }
}

pub fn opt_bool(cfg: &toml::Table, key: &str, default: bool) -> bool {
    match cfg.get(key) {
        Some(toml::Value::Boolean(b)) => *b,
        _ => default,
    }
}

pub fn opt_str(cfg: &toml::Table, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

// ---- customizable panel label ----

/// Optional custom panel label from config. Empty string counts as unset so
/// the panel falls back to its default (the first channel dropped in).
pub fn opt_label(cfg: &toml::Table) -> Option<String> {
    cfg.get("label")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Config-UI row for a panel's custom label. The field is pre-filled with the
/// current label, or the `default` when none is set, so the default text is
/// there to edit. Clearing it (or typing the default back) reverts to default.
pub fn label_config_row(ui: &mut egui::Ui, label: &mut Option<String>, default: &str) {
    ui.horizontal(|ui| {
        ui.label("label:");
        let mut text = label.clone().unwrap_or_else(|| default.to_string());
        if ui
            .add(egui::TextEdit::singleline(&mut text).hint_text("panel label"))
            .changed()
        {
            *label = if text.trim().is_empty() || text == default {
                None
            } else {
                Some(text)
            };
        }
    });
}

/// Write the `label` key when a custom label is set (omitted when default).
pub fn serialize_label(t: &mut toml::Table, label: &Option<String>) {
    if let Some(l) = label {
        t.insert("label".to_string(), toml::Value::String(l.clone()));
    }
}

pub fn opt_str_array(cfg: &toml::Table, key: &str) -> Vec<String> {
    cfg.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SampleType;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r##"
[channels."a.float"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
unit = "V"
color = "#ff0000"

[channels."d.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"##,
        )
        .unwrap()
    }

    #[test]
    fn parse_hex_colors() {
        assert_eq!(parse_hex_color("#ff0000"), Color32::from_rgb(255, 0, 0));
        assert_eq!(parse_hex_color("00ff00"), Color32::from_rgb(0, 255, 0));
        assert_eq!(parse_hex_color("garbage"), Color32::GRAY);
        assert_eq!(parse_hex_color(""), Color32::GRAY);
    }

    #[test]
    fn bind_resolves_and_checks_type() {
        let reg = registry();
        let b = bind("a.float", &reg, &[SampleType::Float]);
        assert!(b.id.is_some() && b.type_ok);
        assert_eq!(b.unit, "V");
        assert_eq!(b.color, Color32::from_rgb(255, 0, 0));

        let wrong = bind("d.log", &reg, &[SampleType::Float]);
        assert!(wrong.id.is_some() && !wrong.type_ok);

        let unknown = bind("nope", &reg, &[SampleType::Float]);
        assert!(unknown.id.is_none());
    }

    #[test]
    fn snapshot_conversions() {
        let snap = ChannelSnapshot::Int { ts: vec![1, 2], vals: vec![10, 20] };
        let (ts, vals) = snapshot_to_f64(&snap).unwrap();
        assert_eq!(ts, &[1, 2]);
        assert_eq!(vals, vec![10.0, 20.0]);
        assert!(snapshot_to_f64(&ChannelSnapshot::Text { lines: vec![] }).is_none());
        assert_eq!(sample_as_f64(&Sample::Bool(true)), Some(1.0));
        assert_eq!(sample_as_f64(&Sample::Text("x".into())), None);
    }

    #[test]
    fn time_of_day_formatting() {
        // 1970-01-01 01:02:03.456 UTC
        let ns = (3_723 * 1_000_000_000i64) + 456_000_000;
        assert_eq!(format_time_of_day(ns), "01:02:03.456");
    }

    #[test]
    fn config_accessors() {
        let cfg: toml::Table =
            toml::from_str(r#"s = "x"
arr = ["a"]
f = 2
b = true"#).unwrap();
        assert_eq!(req_str(&cfg, "s", "test").unwrap(), "x");
        assert!(req_str(&cfg, "missing", "test").is_err());
        assert_eq!(req_str_array(&cfg, "arr", "test").unwrap(), vec!["a"]);
        assert!(req_str_array(&cfg, "missing", "test").is_err());
        assert_eq!(opt_f64(&cfg, "f", 0.0), 2.0); // Integer accepted as f64
        assert_eq!(opt_f64(&cfg, "missing", 7.5), 7.5);
        assert_eq!(opt_i64(&cfg, "f", 0), 2);
        assert!(opt_bool(&cfg, "b", false));
    }

    #[test]
    fn shortens_shared_path_prefix() {
        // Shared leading segments factored out; each keeps its final segment.
        let (prefix, shorts) = shorten_common_prefix(&[
            "site/plant/line1/voltage",
            "site/plant/line1/current",
        ]);
        assert_eq!(prefix, "site/plant/line1/");
        assert_eq!(shorts, vec!["voltage", "current"]);

        // Diverging mid-path stops at the common part.
        let (prefix, shorts) =
            shorten_common_prefix(&["a.b.c.x", "a.b.d.y"]);
        assert_eq!(prefix, "a.b.");
        assert_eq!(shorts, vec!["c.x", "d.y"]);

        // Rebuild reproduces the originals.
        for (s, name) in shorts.iter().zip(["a.b.c.x", "a.b.d.y"]) {
            assert_eq!(format!("{prefix}{s}"), name);
        }

        // No common segment → unchanged.
        let (prefix, shorts) = shorten_common_prefix(&["foo/bar", "baz/qux"]);
        assert_eq!(prefix, "");
        assert_eq!(shorts, vec!["foo/bar", "baz/qux"]);

        // Single name is left whole (nothing to factor against).
        let (prefix, shorts) = shorten_common_prefix(&["a/b/c"]);
        assert_eq!(prefix, "");
        assert_eq!(shorts, vec!["a/b/c"]);

        // Identical names keep their last segment rather than vanishing.
        let (prefix, shorts) = shorten_common_prefix(&["a/b", "a/b"]);
        assert_eq!(prefix, "a/");
        assert_eq!(shorts, vec!["b", "b"]);
    }

    #[test]
    fn refresh_binding_resolves_discovered_mqtt_topic() {
        use crate::store::LiveStore;
        use std::collections::HashMap;
        use std::sync::RwLock;

        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        // A binding to an unconfigured MQTT topic is unresolved at first.
        let mut b = bind("home/sensors/temp", &reg, &[SampleType::Float]);
        assert!(b.id.is_none());

        // No discovered snapshot yet → refresh is a no-op.
        refresh_binding(
            &mut b,
            &[SampleType::Float],
            &RebindCtx { channels: &reg, store: &store, mqtt: None },
        );
        assert!(b.id.is_none());

        // Topic now discovered → registered on the fly and the binding resolves.
        let topic_map: MqttTopicMap = RwLock::new(HashMap::new());
        let mut snap = BTreeMap::new();
        snap.insert("home/sensors/temp".to_string(), "21.5".to_string());
        refresh_binding(
            &mut b,
            &[SampleType::Float],
            &RebindCtx { channels: &reg, store: &store, mqtt: Some((&topic_map, &snap)) },
        );
        assert!(b.id.is_some());
        assert!(b.type_ok);
        assert_eq!(reg.meta(b.id.unwrap()).sample_type, SampleType::Float);
    }

    #[test]
    fn linked_zoom_round_trips_through_ctx() {
        let ctx = egui::Context::default();
        // Defaults when nothing has been published.
        assert!(!linked_zoom_enabled(&ctx));
        assert_eq!(linked_zoom_range(&ctx), None);

        set_linked_zoom_enabled(&ctx, true);
        assert!(linked_zoom_enabled(&ctx));

        set_linked_zoom_range(&ctx, Some((100, 200)));
        assert_eq!(linked_zoom_range(&ctx), Some((100, 200)));

        set_linked_zoom_range(&ctx, None);
        assert_eq!(linked_zoom_range(&ctx), None);
    }

    #[test]
    fn linked_window_gates_on_enabled_and_range() {
        let ctx = egui::Context::default();
        // Not armed, no range: no shared window.
        assert_eq!(linked_window(&ctx), None);

        // Range set but checkbox off: still no shared window.
        set_linked_zoom_range(&ctx, Some((100, 200)));
        assert_eq!(linked_window(&ctx), None);

        // Armed but no range yet (armed, not zoomed): none.
        set_linked_zoom_range(&ctx, None);
        set_linked_zoom_enabled(&ctx, true);
        assert_eq!(linked_window(&ctx), None);

        // Armed with a range: that window governs every panel.
        set_linked_zoom_range(&ctx, Some((100, 200)));
        assert_eq!(linked_window(&ctx), Some((100, 200)));
    }

    #[test]
    fn frame_clock_round_trips_through_ctx() {
        let ctx = egui::Context::default();
        // Unpublished default is None.
        assert_eq!(frame_clock(&ctx), None);
        set_frame_clock(&ctx, 1_700_000_000_000_000_000);
        assert_eq!(frame_clock(&ctx), Some(1_700_000_000_000_000_000));
    }

    #[test]
    fn shared_epoch_seeds_once_floors_and_freezes() {
        let ctx = egui::Context::default();
        // First call seeds from the whole-second floor of the seed.
        let seed = 1_700_000_000_500_000_000i64;
        let epoch = shared_epoch_ns(&ctx, seed);
        assert_eq!(epoch, 1_700_000_000_000_000_000);
        assert_eq!(epoch.rem_euclid(1_000_000_000), 0);
        // Later calls with a different (advanced) seed return the frozen value,
        // so the grid origin never moves between frames.
        assert_eq!(shared_epoch_ns(&ctx, seed + 3_000_000_000), epoch);
        assert_eq!(shared_epoch_ns(&ctx, seed - 9_999), epoch);
    }
}
