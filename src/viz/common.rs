use anyhow::anyhow;
use eframe::egui::{self, Color32};

use crate::config::ChannelRegistry;
use crate::types::{ChannelId, ChannelSnapshot, Sample, SampleType};

/// A panel's link to one channel: resolved id + validity + display metadata.
pub struct Binding {
    pub name: String,
    pub id: Option<ChannelId>,
    pub type_ok: bool,
    pub unit: String,
    pub color: Color32,
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
            }
        }
        None => Binding {
            name: name.to_string(),
            id: None,
            type_ok: true,
            unit: String::new(),
            color: Color32::GRAY,
        },
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
}
