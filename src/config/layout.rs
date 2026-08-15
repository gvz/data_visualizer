use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Layout half of config.toml — screens with panel lists. Panel-specific
/// settings stay an opaque toml::Table here; the viz PanelRegistry interprets
/// them. Channels live in the same file but are parsed by ChannelRegistry;
/// this parser ignores the `[defaults]`/`[channels]` sections.
fn default_window_s() -> f64 {
    10.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutConfig {
    /// App-wide default visible time span (seconds) for time-based panels.
    /// Declared before `screens`: TOML scalars must serialize before tables.
    #[serde(default = "default_window_s")]
    pub default_window_s: f64,
    #[serde(default)]
    pub screens: BTreeMap<String, ScreenConfig>,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self { default_window_s: default_window_s(), screens: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ScreenConfig {
    #[serde(default)]
    pub panels: Vec<PanelEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelEntry {
    #[serde(rename = "type")]
    pub panel_type: String,
    /// Normalized canvas geometry `[fx, fy, fw, fh]`, each `0..1`. Absent on
    /// hand-written configs and pre-canvas layouts; the workspace auto-arranges
    /// panels that lack it. A named field so it never enters `config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<[f32; 4]>,
    #[serde(flatten)]
    pub config: toml::Table,
}

impl LayoutConfig {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        toml::from_str(s).context("parsing config.toml layout")
    }

    pub fn to_toml_string(&self) -> anyhow::Result<String> {
        let raw = toml::to_string_pretty(self).context("serializing layout")?;
        let mut doc: toml_edit::DocumentMut =
            raw.parse().context("reformatting serialized layout")?;
        if let Some(screens) = doc.get_mut("screens").and_then(|i| i.as_table_mut()) {
            for (_, screen) in screens.iter_mut() {
                let Some(panels) = screen
                    .as_table_mut()
                    .and_then(|t| t.get_mut("panels"))
                    .and_then(|i| i.as_array_of_tables_mut())
                else {
                    continue;
                };
                for panel in panels.iter_mut() {
                    for (mut key, item) in panel.iter_mut() {
                        if inline_item(item) {
                            // A former `[..]`/`[[..]]` header key carries no
                            // ` ` before the `=`; give it the standard spacing.
                            key.leaf_decor_mut().set_suffix(" ");
                        }
                    }
                }
            }
        }
        Ok(doc.to_string())
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::from_toml_str(&s)
    }

    /// Write the layout (`default_window_s` + `[screens]`) into `path` while
    /// preserving every other section — `[defaults]`, `[channels]`, comments,
    /// and formatting — verbatim. config.toml is shared with the hand-authored
    /// channel list, so a blind re-serialize would clobber it.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let out = self.merge_into(&existing)?;
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Build the config.toml text: the layout portion (`default_window_s` +
    /// `[screens]`) regenerated fresh and placed first (a root scalar must
    /// precede every table header), followed by every other section from
    /// `existing` — `[defaults]`, `[channels]`, and their comments — kept
    /// verbatim by stripping only the two layout-owned keys with toml_edit.
    fn merge_into(&self, existing: &str) -> anyhow::Result<String> {
        let mut doc: toml_edit::DocumentMut =
            existing.parse().context("parsing existing config.toml")?;
        doc.remove("default_window_s");
        doc.remove("screens");
        let preserved = doc.to_string();
        let preserved = preserved.trim();

        let layout = self.to_toml_string()?;
        if preserved.is_empty() {
            Ok(layout)
        } else {
            Ok(format!("{}\n\n{}\n", layout.trim_end(), preserved))
        }
    }
}

/// Collapse a panel's child structures onto one line so the hierarchy reads at
/// a glance: nested tables (`aliases`, `colors`) become inline tables, arrays
/// of tables (`thresholds`, status `states`) become arrays of inline tables,
/// and scalar arrays (`frame`, `channels`) lose their per-element line breaks.
/// The default `toml::to_string_pretty` renders each of these as a separate
/// `[screens.main.panels.aliases]`-style header that visually detaches from its
/// owning panel; inlining keeps them nested where they belong.
/// Returns `true` when the item was a table or array-of-tables that got
/// rewritten as an inline value — the caller then fixes up the key's spacing,
/// since a former header key carries no ` ` before its `=`.
fn inline_item(item: &mut toml_edit::Item) -> bool {
    use toml_edit::{Item, Value};
    match item {
        Item::Table(t) => {
            let mut inline = std::mem::take(t).into_inline_table();
            inline.fmt();
            *item = Item::Value(Value::InlineTable(inline));
            true
        }
        Item::ArrayOfTables(aot) => {
            let mut arr = toml_edit::Array::new();
            for t in aot.iter() {
                let mut inline = t.clone().into_inline_table();
                inline.fmt();
                arr.push(Value::InlineTable(inline));
            }
            *item = Item::Value(Value::Array(arr));
            true
        }
        Item::Value(Value::Array(a)) => {
            a.fmt();
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[screens.main]
[[screens.main.panels]]
type = "waveform"
channels = ["sensor.accel.x", "sensor.accel.y"]
time_window_s = 5.0
cursors = true

[[screens.main.panels]]
type = "log"
channels = ["system.log"]
max_lines = 500
"#;

    #[test]
    fn parses_screens_and_panels() {
        let l = LayoutConfig::from_toml_str(EXAMPLE).unwrap();
        assert_eq!(l.screens.len(), 1);
        let main = &l.screens["main"];
        assert_eq!(main.panels.len(), 2);
        assert_eq!(main.panels[0].panel_type, "waveform");
        assert_eq!(
            main.panels[0].config["time_window_s"],
            toml::Value::Float(5.0)
        );
        assert_eq!(main.panels[0].config["cursors"], toml::Value::Boolean(true));
        assert!(!main.panels[0].config.contains_key("type"));
        assert_eq!(main.panels[1].panel_type, "log");
    }

    #[test]
    fn round_trips() {
        let l = LayoutConfig::from_toml_str(EXAMPLE).unwrap();
        let s = l.to_toml_string().unwrap();
        let l2 = LayoutConfig::from_toml_str(&s).unwrap();
        assert_eq!(l, l2);
    }

    #[test]
    fn empty_screen_is_ok() {
        let l = LayoutConfig::from_toml_str("[screens.empty]\n").unwrap();
        assert!(l.screens["empty"].panels.is_empty());
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join("datavis_layout_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layout.toml");
        let l = LayoutConfig::from_toml_str(EXAMPLE).unwrap();
        l.save(&path).unwrap();
        let l2 = LayoutConfig::load(&path).unwrap();
        assert_eq!(l, l2);
    }


    #[test]
    fn save_preserves_channels_and_comments() {
        // A shared config.toml with a hand-authored, commented channel section.
        let src = r#"default_window_s = 30.0

[defaults]
max_rate = 100000  # global fallback

# Voltage on phase 1
[channels."volt.l1"]
mqtt_topic = "home/volt/l1"
type = "float"

[screens.main]
[[screens.main.panels]]
type = "numeric"
channel = "volt.l1"
"#;
        let dir = std::env::temp_dir().join("datavis_merge_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, src).unwrap();

        // Load layout, change a layout-owned value, save back.
        let mut l = LayoutConfig::load(&path).unwrap();
        l.default_window_s = 12.5;
        l.save(&path).unwrap();

        let out = std::fs::read_to_string(&path).unwrap();
        // Channel section and its comments survive verbatim.
        assert!(out.contains("# Voltage on phase 1"));
        assert!(out.contains("max_rate = 100000  # global fallback"));
        assert!(out.contains(r#"[channels."volt.l1"]"#));
        // Layout value was rewritten; a root scalar still leads the file.
        assert!(out.contains("default_window_s = 12.5"));
        assert!(out.trim_start().starts_with("default_window_s"));

        // Channels still parse from the rewritten file.
        let reg = crate::config::ChannelRegistry::from_toml_str(&out).unwrap();
        assert!(reg.id("volt.l1").is_some());
        // Layout re-reads with the updated value.
        let l2 = LayoutConfig::load(&path).unwrap();
        assert_eq!(l2.default_window_s, 12.5);
        assert_eq!(l2.screens["main"].panels.len(), 1);
    }


    #[test]
    fn stale_tiles_json_key_is_ignored() {
        let src = r#"
[screens.main]
tiles_json = '{"x":1}'
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
"#;
        let l = LayoutConfig::from_toml_str(src).unwrap();
        assert_eq!(l.screens["main"].panels.len(), 1);
        // Not re-serialized.
        assert!(!l.to_toml_string().unwrap().contains("tiles_json"));
    }

    #[test]
    fn frame_round_trips_and_is_separate_from_config() {
        let src = r#"
[[screens.main.panels]]
type = "numeric"
frame = [0.0, 0.25, 0.5, 0.5]
channel = "demo.sine"
"#;
        let l = LayoutConfig::from_toml_str(src).unwrap();
        let p = &l.screens["main"].panels[0];
        assert_eq!(p.frame, Some([0.0, 0.25, 0.5, 0.5]));
        // `frame` must NOT leak into the panel's own config table.
        assert!(!p.config.contains_key("frame"));
        assert_eq!(p.config["channel"], toml::Value::String("demo.sine".into()));
        // Round-trips through serialization.
        let l2 = LayoutConfig::from_toml_str(&l.to_toml_string().unwrap()).unwrap();
        assert_eq!(l, l2);
    }

    #[test]
    fn panel_subtables_serialize_inline() {
        let src = r##"
[[screens.main.panels]]
type = "state_graph"
channel = "load/state0"
frame = [0.0, 0.35, 0.5, 0.13]

[screens.main.panels.aliases]
idle = "test"

[screens.main.panels.colors]
done = "#d8cece"

[[screens.main.panels]]
type = "gauge"
channel = "load/ch5"

[[screens.main.panels.thresholds]]
color = "#fe640b"
value = 0.0
"##;
        let l = LayoutConfig::from_toml_str(src).unwrap();
        let out = l.to_toml_string().unwrap();
        // Sub-tables stay nested inside the panel as inline tables — no
        // detached `[screens.main.panels.aliases]` headers.
        assert!(out.contains(r#"aliases = { idle = "test" }"#), "got:\n{out}");
        assert!(out.contains(r##"colors = { done = "#d8cece" }"##), "got:\n{out}");
        assert!(!out.contains("[screens.main.panels.aliases]"), "got:\n{out}");
        // Array-of-tables collapses to an array of inline tables.
        assert!(
            out.contains(r##"thresholds = [{ color = "#fe640b", value = 0.0 }]"##),
            "got:\n{out}"
        );
        // Scalar arrays sit on one line (values are f32, so don't match on
        // exact decimals — assert the whole array is on a single line).
        assert!(
            out.lines().any(|l| {
                let l = l.trim();
                l.starts_with("frame = [") && l.ends_with(']')
            }),
            "got:\n{out}"
        );
        // And it all still parses back to the same layout.
        let l2 = LayoutConfig::from_toml_str(&out).unwrap();
        assert_eq!(l, l2);
    }

    #[test]
    fn frame_absent_is_none() {
        let src = r#"
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
"#;
        let l = LayoutConfig::from_toml_str(src).unwrap();
        assert_eq!(l.screens["main"].panels[0].frame, None);
    }
}
