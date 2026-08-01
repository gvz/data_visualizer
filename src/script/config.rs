use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

/// The `[scripts]` section of the shared config.toml.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptsConfig {
    /// Directory holding `*.py` scripts, relative to config.toml.
    pub dir: String,
    /// Script stems (filename without `.py`) that are active.
    pub enabled: Vec<String>,
    /// Seconds of history handed to each script per tick.
    pub window_s: f64,
}

impl Default for ScriptsConfig {
    fn default() -> Self {
        Self { dir: "scripts".to_string(), enabled: Vec::new(), window_s: 10.0 }
    }
}

#[derive(Deserialize)]
struct DocWrapper {
    scripts: Option<RawScripts>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScripts {
    dir: Option<String>,
    #[serde(default)]
    enabled: Vec<String>,
    window_s: Option<f64>,
}

impl ScriptsConfig {
    /// Parse the `[scripts]` table out of a full config.toml. Absent section or
    /// absent keys fall back to the defaults.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let doc: DocWrapper = toml::from_str(s).context("parsing [scripts]")?;
        let def = ScriptsConfig::default();
        Ok(match doc.scripts {
            None => def,
            Some(raw) => ScriptsConfig {
                dir: raw.dir.unwrap_or(def.dir),
                enabled: raw.enabled,
                window_s: raw.window_s.unwrap_or(def.window_s),
            },
        })
    }

    /// Rewrite only the `[scripts]` keys in an existing config.toml, preserving
    /// every other section and its comments (same approach as
    /// `LayoutConfig::save`).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use toml_edit::{value, Array, DocumentMut};

        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc: DocumentMut =
            existing.parse().context("parsing existing config.toml")?;

        let mut arr = Array::new();
        for name in &self.enabled {
            arr.push(name.as_str());
        }
        doc["scripts"]["dir"] = value(self.dir.as_str());
        doc["scripts"]["enabled"] = value(arr);
        doc["scripts"]["window_s"] = value(self.window_s);

        std::fs::write(path, doc.to_string())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_yields_defaults() {
        let c = ScriptsConfig::from_toml_str("default_window_s = 5.0\n").unwrap();
        assert_eq!(c, ScriptsConfig::default());
    }

    #[test]
    fn parses_all_fields() {
        let c = ScriptsConfig::from_toml_str(
            "[scripts]\ndir = \"s\"\nenabled = [\"a\", \"b\"]\nwindow_s = 2.5\n",
        )
        .unwrap();
        assert_eq!(c.dir, "s");
        assert_eq!(c.enabled, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(c.window_s, 2.5);
    }

    #[test]
    fn missing_keys_fall_back() {
        let c = ScriptsConfig::from_toml_str("[scripts]\nenabled = [\"x\"]\n").unwrap();
        assert_eq!(c.dir, "scripts");
        assert_eq!(c.window_s, 10.0);
        assert_eq!(c.enabled, vec!["x".to_string()]);
    }

    #[test]
    fn save_rewrites_scripts_preserving_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# top comment\ndefault_window_s = 5.0\n\n[channels.\"a\"]\ntype = \"float\"\n",
        )
        .unwrap();

        let cfg = ScriptsConfig {
            dir: "scripts".into(),
            enabled: vec!["accel_mag".into()],
            window_s: 4.0,
        };
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        // Round-trips through the parser.
        let reparsed = ScriptsConfig::from_toml_str(&text).unwrap();
        assert_eq!(reparsed, cfg);
        // Other sections and comments survive.
        assert!(text.contains("# top comment"));
        assert!(text.contains("[channels.\"a\"]"));
        assert!(text.contains("default_window_s = 5.0"));
    }
}
