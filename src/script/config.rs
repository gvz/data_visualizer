use anyhow::Context;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputBinding {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptInstance {
    pub id: String,
    pub script: String,
    #[serde(default)]
    pub inputs: Option<Vec<String>>,
    #[serde(default)]
    pub outputs: Option<Vec<OutputBinding>>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// The `[scripts]` section of the shared config.toml.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptsConfig {
    /// Directory holding `*.py` scripts, relative to config.toml.
    pub dir: String,
    /// Script stems (filename without `.py`) that are active.
    pub enabled: Vec<String>,
    /// Seconds of history handed to each script per tick.
    pub window_s: f64,
    /// Script instances.
    pub instances: Vec<ScriptInstance>,
}

impl Default for ScriptsConfig {
    fn default() -> Self {
        Self { dir: "scripts".to_string(), enabled: Vec::new(), window_s: 10.0, instances: Vec::new() }
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
    #[serde(default)]
    instances: Vec<ScriptInstance>,
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
                instances: raw.instances,
            },
        })
    }

    /// Rewrite only the `[scripts]` keys in an existing config.toml, preserving
    /// every other section and its comments (same approach as
    /// `LayoutConfig::save`).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Table, Item};

        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc: DocumentMut =
            existing.parse().context("parsing existing config.toml")?;

        // Ensure [scripts] is a proper table, not an inline table
        if !doc.contains_key("scripts") {
            doc["scripts"] = Item::Table(Table::new());
        } else if doc["scripts"].is_inline_table() {
            // Convert inline table to proper table if needed
            if let Some(inline) = doc["scripts"].as_inline_table() {
                let mut new_table = Table::new();
                for (k, v) in inline.iter() {
                    new_table[k] = Item::Value(v.clone());
                }
                doc["scripts"] = Item::Table(new_table);
            }
        }

        doc["scripts"]["dir"] = value(self.dir.as_str());
        doc["scripts"]["window_s"] = value(self.window_s);

        let mut tables = ArrayOfTables::new();
        for inst in &self.instances {
            let mut t = Table::new();
            t["id"] = value(inst.id.as_str());
            t["script"] = value(inst.script.as_str());
            if let Some(inputs) = &inst.inputs {
                let mut arr = Array::new();
                for name in inputs {
                    arr.push(name.as_str());
                }
                t["inputs"] = value(arr);
            }
            if let Some(outputs) = &inst.outputs {
                let mut outs = Array::new();
                for o in outputs {
                    let mut it = toml_edit::InlineTable::new();
                    it.insert("name", o.name.as_str().into());
                    it.insert("type", o.ty.as_str().into());
                    it.insert("unit", o.unit.as_str().into());
                    outs.push(toml_edit::Value::InlineTable(it));
                }
                t["outputs"] = value(outs);
            }
            t["enabled"] = value(inst.enabled);
            tables.push(t);
        }
        doc["scripts"]["instances"] = Item::ArrayOfTables(tables);

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
    fn parses_instances_with_defaults() {
        let c = ScriptsConfig::from_toml_str(
            "[scripts]\ndir = \"s\"\nwindow_s = 4.0\n\n\
             [[scripts.instances]]\nid = \"a\"\nscript = \"sine_rms\"\n\n\
             [[scripts.instances]]\nid = \"b\"\nscript = \"sine_rms\"\n\
             inputs = [\"load/ch1\"]\nenabled = false\n\
             outputs = [{ name = \"scripts.b\", type = \"float\", unit = \"g\" }]\n",
        )
        .unwrap();
        assert_eq!(c.dir, "s");
        assert_eq!(c.window_s, 4.0);
        assert_eq!(c.instances.len(), 2);

        let a = &c.instances[0];
        assert_eq!(a.id, "a");
        assert_eq!(a.script, "sine_rms");
        assert_eq!(a.inputs, None);
        assert_eq!(a.outputs, None);
        assert!(a.enabled); // default true

        let b = &c.instances[1];
        assert_eq!(b.inputs, Some(vec!["load/ch1".to_string()]));
        assert!(!b.enabled);
        let ob = b.outputs.as_ref().unwrap();
        assert_eq!(ob[0].name, "scripts.b");
        assert_eq!(ob[0].ty, "float");
        assert_eq!(ob[0].unit, "g");
    }

    #[test]
    fn save_round_trips_instances_preserving_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# top\ndefault_window_s = 5.0\n\n[channels.\"x\"]\ntype = \"float\"\n")
            .unwrap();

        let cfg = ScriptsConfig {
            dir: "scripts".into(),
            window_s: 7.0,
            instances: vec![
                ScriptInstance {
                    id: "ch0_rms".into(),
                    script: "sine_rms".into(),
                    inputs: Some(vec!["load/ch0".into()]),
                    outputs: None,
                    enabled: true,
                },
                ScriptInstance {
                    id: "ch1_rms".into(),
                    script: "sine_rms".into(),
                    inputs: Some(vec!["load/ch1".into()]),
                    outputs: Some(vec![OutputBinding {
                        name: "scripts.ch1_rms".into(),
                        ty: "float".into(),
                        unit: String::new(),
                    }]),
                    enabled: false,
                },
            ],
            enabled: Vec::new(),
        };
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let reparsed = ScriptsConfig::from_toml_str(&text).unwrap();
        assert_eq!(reparsed, cfg);
        assert!(text.contains("# top"));
        assert!(text.contains("[channels.\"x\"]"));
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
            instances: Vec::new(),
        };
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        // Round-trips through the parser (note: enabled list is not persisted).
        let reparsed = ScriptsConfig::from_toml_str(&text).unwrap();
        let expected = ScriptsConfig {
            dir: "scripts".into(),
            enabled: Vec::new(), // enabled list is not saved/loaded anymore
            window_s: 4.0,
            instances: Vec::new(),
        };
        assert_eq!(reparsed, expected);
        // Other sections and comments survive.
        assert!(text.contains("# top comment"));
        assert!(text.contains("[channels.\"a\"]"));
        assert!(text.contains("default_window_s = 5.0"));
    }
}
