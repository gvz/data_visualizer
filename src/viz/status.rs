use eframe::egui::{self, Color32, FontId};

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{Sample, SampleType};
use crate::viz::common::{
    bind, binding_error, color_to_hex, hex_to_color, is_light, label_config_row, linked_window,
    opt_label, opt_str, outlined_text, refresh_binding, serialize_label, Binding, RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "status";

const ACCEPTED: &[SampleType] = &[SampleType::Text, SampleType::Int, SampleType::Bool];

/// Fallback badge fill for a value with no configured state entry.
const UNMAPPED_COLOR: Color32 = Color32::from_gray(70);

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

    /// Editable rows: `[match][label][color][remove]`, plus an add button.
    pub(crate) fn config_ui(&mut self, ui: &mut egui::Ui) {
        ui.label("states (value \u{2192} color):");
        let mut remove = None;
        for (i, e) in self.entries.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut e.match_key)
                        .desired_width(80.0)
                        .hint_text("value"),
                );
                let mut label = e.label.clone().unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut label)
                            .desired_width(100.0)
                            .hint_text("label"),
                    )
                    .changed()
                {
                    e.label = if label.trim().is_empty() { None } else { Some(label) };
                }
                ui.color_edit_button_srgba(&mut e.color);
                if ui.button("\u{2715}").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.entries.remove(i);
        }
        if ui.button("+ state").clicked() {
            // A fresh editable swatch; Color32::GRAY (from_gray 128) is a visible
            // starting color, deliberately distinct from the runtime UNMAPPED_COLOR.
            self.entries.push(StateEntry {
                match_key: String::new(),
                label: None,
                color: Color32::GRAY,
            });
        }
    }
}

/// Single-value badge showing a channel's current discrete state, recolored per
/// the configured value→color map.
pub struct StatusPanel {
    bound: Binding,
    label: Option<String>,
    states: StateMap,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let name = opt_str(cfg, "channel");
    Ok(Box::new(StatusPanel {
        bound: bind(&name, reg, ACCEPTED),
        label: opt_label(cfg),
        states: StateMap::from_config(cfg),
    }))
}

impl VizPanel for StatusPanel {
    fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.bound.name)
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        label_config_row(ui, &mut self.label, &self.bound.name);
        ui.separator();
        self.states.config_ui(ui);
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
        // In sync mode read the value at the shared zoom window's end, so the
        // badge matches the zoomed waveform's right edge; else the latest value.
        let at = linked_window(ui.ctx())
            .map(|(_, end)| end)
            .unwrap_or_else(|| store.now_ns());
        let sample = store.latest_at(id, at).map(|(_, s)| s);
        let key = sample.as_ref().and_then(sample_to_key);
        // Matched entry → its label+color; unmapped value → raw text on gray;
        // no sample → dash on gray. `key` is None only when there is no sample:
        // Float is rejected at bind time (ACCEPTED), so it never reaches
        // sample_to_key here.
        let (text, color) = match &key {
            Some(k) => match self.states.lookup(k) {
                Some(e) => (e.display().to_string(), e.color),
                None => (k.clone(), UNMAPPED_COLOR),
            },
            None => ("\u{2014}".to_string(), UNMAPPED_COLOR),
        };

        let desired = egui::vec2(ui.available_width().max(80.0), 48.0);
        let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, color);
        // Contrast text against any fill: black on light, white on dark, outlined
        // with the opposite so it stays legible.
        let (fg, outline) = if is_light(color) {
            (Color32::BLACK, Color32::WHITE)
        } else {
            (Color32::WHITE, Color32::BLACK)
        };
        outlined_text(painter, rect.center(), &text, FontId::proportional(20.0), fg, outline);
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert("channel".to_string(), toml::Value::String(self.bound.name.clone()));
        serialize_label(&mut t, &self.label);
        self.states.write_config(&mut t);
        t
    }

    fn drop_channel(&mut self, name: &str, reg: &ChannelRegistry) {
        self.bound = bind(name, reg, ACCEPTED);
    }

    fn refresh_bindings(&mut self, ctx: &RebindCtx) {
        refresh_binding(&mut self.bound, ACCEPTED, ctx);
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

[channels."motor.mode"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"

[channels."valve.state"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"

[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
"#,
        )
        .unwrap()
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r##"type = "status"
channel = "motor.state"

[[states]]
match = "2"
label = "FAULT"
color = "#d62728"

[[states]]
match = "1"
color = "#2ca02c""##,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
        assert_eq!(p.title(), "motor.state");
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let motor = channels.id("motor.state").unwrap();
        let mode = channels.id("motor.mode").unwrap();
        store.write_numeric(motor, 1, NumericVal::Int(2));
        store.write_text(mode, 1, "RUNNING".to_string());
        let reg = PanelRegistry::with_builtins();
        // int with states, text channel, unknown channel, float (rejected),
        // and `valve.state` (accepted type, no sample written → no-data badge)
        // must all render without panic.
        for src in [
            r##"type = "status"
channel = "motor.state"

[[states]]
match = "2"
label = "FAULT"
color = "#d62728""##,
            r#"type = "status"
channel = "motor.mode""#,
            r#"type = "status"
channel = "does.not.exist""#,
            r#"type = "status"
channel = "demo.sine""#,
            r#"type = "status"
channel = "valve.state""#,
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
}
