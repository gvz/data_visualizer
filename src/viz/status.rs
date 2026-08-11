//! Iced-port status panel — coloured dot + label showing the latest discrete
//! channel state (Text / Int / Bool), mapped via a configurable key→color table.
//!
//! `sync` calls `store.latest` and caches `(dot_color, display_text)`.
//! `view` renders a `row![colored_dot, label]`.  No store access in `view`.

use iced::Color;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelId, Sample};
use crate::viz::{PanelMessage, VizPanel};

// ---------------------------------------------------------------------------
// Helpers (inlined per-panel)
// ---------------------------------------------------------------------------

fn opt_str(cfg: &toml::Table, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// Parse a `#rrggbb` hex string into an iced `Color`.  Returns `None` on
/// parse failure so callers can skip malformed entries.
fn hex_to_color(s: &str) -> Option<Color> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::from_rgb(
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
    ))
}

/// Encode an iced `Color` back to a `#rrggbb` hex string.
fn color_to_hex(c: Color) -> String {
    let r = (c.r * 255.0).round() as u8;
    let g = (c.g * 255.0).round() as u8;
    let b = (c.b * 255.0).round() as u8;
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Fallback dot color when the sample value has no configured state entry.
const UNMAPPED_COLOR: Color = Color { r: 0.27, g: 0.27, b: 0.27, a: 1.0 };

/// The string match key for a sample: `Text` as-is, `Int`/`Bool` stringified.
/// `Float` returns `None` (no discrete key).
pub(crate) fn sample_to_key(s: &Sample) -> Option<String> {
    match s {
        Sample::Text(t) => Some(t.clone()),
        Sample::Int(i)  => Some(i.to_string()),
        Sample::Bool(b) => Some(b.to_string()),
        Sample::Float(_) => None,
    }
}

// ---------------------------------------------------------------------------
// StateEntry / StateMap
// ---------------------------------------------------------------------------

/// One configured state: a raw-value match key, optional display label, and
/// the dot colour to show when the channel value matches `match_key`.
pub(crate) struct StateEntry {
    pub match_key: String,
    pub label: Option<String>,
    pub color: Color,
}

impl StateEntry {
    /// Text shown in the label column for this entry.
    pub(crate) fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.match_key)
    }
}

/// User-configured value→(label, color) map for the status dot.
#[derive(Default)]
pub(crate) struct StateMap {
    pub entries: Vec<StateEntry>,
}

impl StateMap {
    /// Parse the `states` array of `{ match, label?, color }`.
    /// Entries missing `match` or a parseable `color` are silently skipped.
    pub(crate) fn from_config(cfg: &toml::Table) -> Self {
        let entries = cfg
            .get("states")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let t = item.as_table()?;
                        let match_key = t.get("match")?.as_str()?.to_string();
                        let color = hex_to_color(t.get("color")?.as_str()?)?;
                        let label = t
                            .get("label")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        Some(StateEntry { match_key, label, color })
                    })
                    .collect()
            })
            .unwrap_or_default();
        StateMap { entries }
    }

    /// Write the `states` array into `t`; omitted entirely when empty.
    pub(crate) fn write_config(&self, t: &mut toml::Table) {
        if self.entries.is_empty() {
            return;
        }
        let arr = self
            .entries
            .iter()
            .map(|e| {
                let mut tt = toml::Table::new();
                tt.insert("match".to_string(), toml::Value::String(e.match_key.clone()));
                if let Some(l) = &e.label {
                    tt.insert("label".to_string(), toml::Value::String(l.clone()));
                }
                tt.insert("color".to_string(), toml::Value::String(color_to_hex(e.color)));
                toml::Value::Table(tt)
            })
            .collect();
        t.insert("states".to_string(), toml::Value::Array(arr));
    }

    /// First entry whose key matches `key` exactly.
    pub(crate) fn lookup(&self, key: &str) -> Option<&StateEntry> {
        self.entries.iter().find(|e| e.match_key == key)
    }
}

// ---------------------------------------------------------------------------
// Status panel
// ---------------------------------------------------------------------------

pub const TYPE_NAME: &str = "status";

/// Single-value badge showing a channel's current discrete state, coloured
/// per the configured value→colour map.
pub struct Status {
    channel_name: String,
    channel_id: Option<ChannelId>,
    states: StateMap,
    /// Cached from last `sync`: the dot colour and display text.
    dot_color: Color,
    display: String,
}

impl Status {
    /// Construct from a TOML config table and a channel registry.
    ///
    /// `config` must contain `channel = "<name>"` and optionally a `states`
    /// array.  An unknown channel is not an error — the panel shows "—" until
    /// data arrives.
    pub fn new(cfg: &toml::Table, ch: &ChannelRegistry) -> Self {
        let channel_name = opt_str(cfg, "channel");
        let channel_id = ch.id(&channel_name);
        Status {
            channel_name,
            channel_id,
            states: StateMap::from_config(cfg),
            dot_color: UNMAPPED_COLOR,
            display: "\u{2014}".to_string(), // em-dash
        }
    }
}

impl VizPanel for Status {
    fn title(&self) -> String {
        if self.channel_name.is_empty() {
            "Status".to_string()
        } else {
            self.channel_name.clone()
        }
    }

    fn sync(&mut self, store: &dyn ChannelStore) {
        let Some(id) = self.channel_id else {
            self.dot_color = UNMAPPED_COLOR;
            self.display = "\u{2014}".to_string();
            return;
        };
        let sample = store.latest(id).map(|(_, s)| s);
        let key = sample.as_ref().and_then(sample_to_key);
        let (text, color) = match &key {
            Some(k) => match self.states.lookup(k) {
                Some(e) => (e.display().to_string(), e.color),
                None => (k.clone(), UNMAPPED_COLOR),
            },
            None => ("\u{2014}".to_string(), UNMAPPED_COLOR),
        };
        self.display = text;
        self.dot_color = color;
    }

    fn view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, row, text};
        use iced::{Length, alignment};

        // A coloured "●" bullet acts as a simple status dot.
        let dot = text("\u{25cf}")
            .color(self.dot_color)
            .size(20);

        let label = text(self.display.clone()).size(16);

        container(
            row![dot, label]
                .spacing(8)
                .align_y(alignment::Vertical::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_y(Length::Fill)
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
            format!("Channel: {}", self.channel_name)
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
        self.states.write_config(&mut t);
        t
    }
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
[channels."motor.state"]
mqtt_topic = "sensors/motor"
type = "int"

[channels."motor.mode"]
mqtt_topic = "sensors/mode"
type = "text"
"#;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(CHANNEL_TOML).unwrap()
    }

    // --- required tests from the brief ---

    #[test]
    fn status_constructs_and_serialize_roundtrips() {
        let ch = registry();
        let cfg: toml::Table = toml::from_str(
            r##"
channel = "motor.state"

[[states]]
match = "2"
label = "FAULT"
color = "#d62728"

[[states]]
match = "1"
color = "#2ca02c"
"##,
        )
        .unwrap();
        let p = Status::new(&cfg, &ch);
        assert_eq!(p.title(), "motor.state");

        let out = p.serialize();
        // channel key round-trips
        assert_eq!(
            out.get("channel").and_then(|v| v.as_str()),
            Some("motor.state"),
        );
        // states array survives round-trip
        let m2 = StateMap::from_config(&out);
        assert_eq!(m2.entries.len(), 2);
        assert_eq!(m2.entries[0].match_key, "2");
        assert_eq!(m2.entries[0].label.as_deref(), Some("FAULT"));
        // color round-trips (within u8 rounding)
        let hex = color_to_hex(m2.entries[0].color);
        assert_eq!(hex, "#d62728");
        assert_eq!(m2.entries[1].label, None);
    }

    #[test]
    fn status_title_default_when_no_channel() {
        let ch = registry();
        let p = Status::new(&toml::Table::new(), &ch);
        assert_eq!(p.title(), "Status");
    }

    #[test]
    fn status_sync_resolves_state_entry() {
        let ch = registry();
        let store = LiveStore::from_registry(&ch);
        let motor_id = ch.id("motor.state").unwrap();
        store.write_numeric(motor_id, 1_000_000, NumericVal::Int(2));

        let cfg: toml::Table = toml::from_str(
            r##"
channel = "motor.state"

[[states]]
match = "2"
label = "FAULT"
color = "#d62728"
"##,
        )
        .unwrap();
        let mut p = Status::new(&cfg, &ch);
        p.sync(&store);
        assert_eq!(p.display, "FAULT");
    }

    #[test]
    fn status_sync_unmapped_value_shows_key() {
        let ch = registry();
        let store = LiveStore::from_registry(&ch);
        let motor_id = ch.id("motor.state").unwrap();
        store.write_numeric(motor_id, 1_000_000, NumericVal::Int(99));

        let cfg: toml::Table = toml::from_str(r#"channel = "motor.state""#).unwrap();
        let mut p = Status::new(&cfg, &ch);
        p.sync(&store);
        assert_eq!(p.display, "99");
    }

    #[test]
    fn status_sync_no_data_shows_dash() {
        let ch = registry();
        let store = LiveStore::from_registry(&ch);
        let cfg: toml::Table = toml::from_str(r#"channel = "motor.state""#).unwrap();
        let mut p = Status::new(&cfg, &ch);
        p.sync(&store);
        assert_eq!(p.display, "\u{2014}");
    }

    #[test]
    fn sample_key_per_type() {
        assert_eq!(sample_to_key(&Sample::Text("RUN".into())), Some("RUN".to_string()));
        assert_eq!(sample_to_key(&Sample::Int(2)), Some("2".to_string()));
        assert_eq!(sample_to_key(&Sample::Bool(true)), Some("true".to_string()));
        assert_eq!(sample_to_key(&Sample::Bool(false)), Some("false".to_string()));
        assert_eq!(sample_to_key(&Sample::Float(1.5)), None);
    }

    #[test]
    fn statemap_lookup_matches_exact_key() {
        let cfg: toml::Table = toml::from_str(
            r##"
[[states]]
match = "2"
label = "FAULT"
color = "#d62728"

[[states]]
match = "1"
color = "#2ca02c"
"##,
        )
        .unwrap();
        let m = StateMap::from_config(&cfg);
        assert_eq!(m.entries.len(), 2);
        let fault = m.lookup("2").unwrap();
        assert_eq!(fault.display(), "FAULT");
        assert_eq!(m.lookup("1").unwrap().display(), "1");
        assert!(m.lookup("0").is_none());
    }

    #[test]
    fn malformed_state_entry_is_skipped() {
        let cfg: toml::Table = toml::from_str(
            r##"
[[states]]
match = "1"

[[states]]
color = "#ffffff"

[[states]]
match = "2"
color = "#000000"
"##,
        )
        .unwrap();
        let m = StateMap::from_config(&cfg);
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].match_key, "2");
    }

    #[test]
    fn empty_map_writes_nothing() {
        let m = StateMap::default();
        let mut out = toml::Table::new();
        m.write_config(&mut out);
        assert!(out.get("states").is_none());
    }

    #[test]
    fn hex_roundtrip() {
        let c = hex_to_color("#d62728").unwrap();
        assert_eq!(color_to_hex(c), "#d62728");
    }
}
