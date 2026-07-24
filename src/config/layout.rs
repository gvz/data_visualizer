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
    /// egui_tiles tree layout (JSON-encoded), written by the workspace module.
    /// Absent on hand-written configs — a default grid is built instead.
    /// MUST be declared before `panels`: TOML requires scalar values to
    /// serialize before arrays-of-tables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiles_json: Option<String>,
    #[serde(default)]
    pub panels: Vec<PanelEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelEntry {
    #[serde(rename = "type")]
    pub panel_type: String,
    #[serde(flatten)]
    pub config: toml::Table,
}

impl LayoutConfig {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        toml::from_str(s).context("parsing config.toml layout")
    }

    pub fn to_toml_string(&self) -> anyhow::Result<String> {
        toml::to_string_pretty(self).context("serializing layout")
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
    fn tiles_json_round_trips() {
        let src = r#"
[screens.main]
tiles_json = '{"fake":"tree"}'

[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
"#;
        let l = LayoutConfig::from_toml_str(src).unwrap();
        assert_eq!(
            l.screens["main"].tiles_json.as_deref(),
            Some(r#"{"fake":"tree"}"#)
        );
        let l2 = LayoutConfig::from_toml_str(&l.to_toml_string().unwrap()).unwrap();
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
    fn tiles_json_absent_is_none_and_not_serialized() {
        let l = LayoutConfig::from_toml_str("[screens.empty]\n").unwrap();
        assert_eq!(l.screens["empty"].tiles_json, None);
        let out = l.to_toml_string().unwrap();
        assert!(!out.contains("tiles_json"));
    }
}
