use eframe::egui::Color32;

use crate::types::Sample;
use crate::viz::common::{color_to_hex, hex_to_color};

/// The string match key for a sample: `Text` as-is, `Int`/`Bool` stringified.
/// `Float` has no discrete key (the type is rejected before render).
pub(crate) fn sample_to_key(s: &Sample) -> Option<String> {
    match s {
        Sample::Text(t) => Some(t.clone()),
        Sample::Int(i) => Some(i.to_string()),
        Sample::Bool(b) => Some(b.to_string()),
        Sample::Float(_) => None,
    }
}

/// One configured state: a raw-value key, its badge color, and an optional
/// display label (falls back to the key).
pub(crate) struct StateEntry {
    pub match_key: String,
    pub label: Option<String>,
    pub color: Color32,
}

impl StateEntry {
    /// Text shown on the badge for this entry.
    pub(crate) fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.match_key)
    }
}

/// User-configured value->(label,color) map for the status badge.
#[derive(Default)]
pub(crate) struct StateMap {
    pub entries: Vec<StateEntry>,
}

impl StateMap {
    /// Parse the `states` array of `{ match, label?, color }`. Entries missing
    /// `match` or a parseable `color` are skipped.
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

    /// Write the `states` array; omitted entirely when empty.
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Entry with a label displays the label.
        let fault = m.lookup("2").unwrap();
        assert_eq!(fault.display(), "FAULT");
        assert_eq!(fault.color, Color32::from_rgb(0xd6, 0x27, 0x28));
        // Entry without a label displays the raw key.
        assert_eq!(m.lookup("1").unwrap().display(), "1");
        // Unmapped key.
        assert!(m.lookup("0").is_none());
    }

    #[test]
    fn malformed_entry_is_skipped() {
        // Missing `color` -> skipped; missing `match` -> skipped; good one kept.
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
    fn config_round_trips() {
        let src: toml::Table = toml::from_str(
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
        let m = StateMap::from_config(&src);
        let mut out = toml::Table::new();
        m.write_config(&mut out);
        // Reparsing the written config yields an equal map.
        let m2 = StateMap::from_config(&out);
        assert_eq!(m2.entries.len(), 2);
        assert_eq!(m2.entries[0].match_key, "2");
        assert_eq!(m2.entries[0].label.as_deref(), Some("FAULT"));
        assert_eq!(m2.entries[0].color, Color32::from_rgb(0xd6, 0x27, 0x28));
        assert_eq!(m2.entries[1].label, None);
    }

    #[test]
    fn empty_map_writes_nothing() {
        let m = StateMap::default();
        let mut out = toml::Table::new();
        m.write_config(&mut out);
        assert!(out.get("states").is_none());
    }
}
