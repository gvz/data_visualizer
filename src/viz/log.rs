use eframe::egui;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{ChannelSnapshot, SampleType, TimeWindow};
use crate::viz::common::{
    bind, binding_error, format_time_of_day, label_config_row, opt_i64, opt_label, opt_str_array,
    refresh_binding, serialize_label, Binding, RebindCtx,
};
use crate::viz::VizPanel;

pub const TYPE_NAME: &str = "log";

const ACCEPTED: &[SampleType] = &[SampleType::Text];

/// Scrolling, filterable, timestamped log merged from one or more text channels.
pub struct LogPanel {
    title: String,
    label: Option<String>,
    bound: Vec<Binding>,
    max_lines: usize,
    filter: String,
}

pub fn ctor(
    cfg: &toml::Table,
    reg: &ChannelRegistry,
) -> anyhow::Result<Box<dyn VizPanel>> {
    let names = opt_str_array(cfg, "channels");
    let bound = names.iter().map(|n| bind(n, reg, ACCEPTED)).collect();
    Ok(Box::new(LogPanel {
        title: names.join(", "),
        label: opt_label(cfg),
        bound,
        max_lines: opt_i64(cfg, "max_lines", 500).max(1) as usize,
        filter: String::new(),
    }))
}

/// Merge line sets, sort by timestamp, apply case-insensitive substring
/// filter, keep only the newest `max` lines.
pub(crate) fn merge_filter(
    mut sets: Vec<Vec<(i64, String)>>,
    filter: &str,
    max: usize,
) -> Vec<(i64, String)> {
    let mut all: Vec<(i64, String)> = sets.drain(..).flatten().collect();
    all.sort_by_key(|(t, _)| *t);
    let f = filter.to_lowercase();
    let mut out: Vec<(i64, String)> = all
        .into_iter()
        .filter(|(_, l)| f.is_empty() || l.to_lowercase().contains(&f))
        .collect();
    if out.len() > max {
        out.drain(..out.len() - max);
    }
    out
}

impl VizPanel for LogPanel {
    fn title(&self) -> &str {
        self.label
            .as_deref()
            .unwrap_or_else(|| self.bound.first().map(|b| b.name.as_str()).unwrap_or(""))
    }

    fn accepted_types(&self) -> &[SampleType] {
        ACCEPTED
    }

    fn config_ui(&mut self, ui: &mut egui::Ui) {
        let default = self.bound.first().map(|b| b.name.clone()).unwrap_or_default();
        label_config_row(ui, &mut self.label, &default);
        ui.horizontal(|ui| {
            ui.label("filter:");
            ui.text_edit_singleline(&mut self.filter);
            let mut max = self.max_lines as i64;
            ui.label("max lines:");
            ui.add(egui::DragValue::new(&mut max).range(1..=100_000));
            self.max_lines = max.max(1) as usize;
        });
    }

    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        if self.bound.is_empty() {
            ui.label(egui::RichText::new("Drop channels here").weak());
            return;
        }
        // In sync mode only show lines within the shared zoom window.
        let (start_ns, end_ns) = crate::viz::common::linked_window(ui.ctx())
            .map(|(s, e)| (s, e + 1))
            .unwrap_or((i64::MIN, i64::MAX));
        let mut sets = Vec::new();
        for b in &self.bound {
            if binding_error(ui, b, TYPE_NAME) {
                continue;
            }
            let id = b.id.expect("checked by binding_error");
            let snap = store.snapshot(id, TimeWindow { start_ns, end_ns });
            if let ChannelSnapshot::Text { lines } = snap {
                sets.push(lines);
            }
        }
        let lines = merge_filter(sets, &self.filter, self.max_lines);
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (ts, line) in &lines {
                    ui.monospace(format!("{}  {}", format_time_of_day(*ts), line));
                }
            });
    }

    fn serialize(&self) -> toml::Table {
        let mut t = toml::Table::new();
        t.insert(
            "channels".to_string(),
            toml::Value::Array(
                self.bound
                    .iter()
                    .map(|b| toml::Value::String(b.name.clone()))
                    .collect(),
            ),
        );
        t.insert("max_lines".to_string(), toml::Value::Integer(self.max_lines as i64));
        serialize_label(&mut t, &self.label);
        t
    }

    fn drop_channel(&mut self, name: &str, reg: &crate::config::ChannelRegistry) {
        if self.bound.iter().any(|b| b.name == name) {
            return;
        }
        self.bound.push(bind(name, reg, ACCEPTED));
        self.title = self.bound.iter().map(|b| b.name.as_str()).collect::<Vec<_>>().join(", ");
    }

    fn refresh_bindings(&mut self, ctx: &RebindCtx) {
        for b in &mut self.bound {
            refresh_binding(b, ACCEPTED, ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::viz::PanelRegistry;
    use eframe::egui;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."system.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"

[channels."app.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap()
    }

    #[test]
    fn merge_sorts_filters_and_caps() {
        let sets = vec![
            vec![(3i64, "warn: c".to_string()), (1, "info: a".to_string())],
            vec![(2i64, "WARN: b".to_string())],
        ];
        // case-insensitive filter, merged and sorted by ts
        let out = merge_filter(sets.clone(), "warn", 10);
        assert_eq!(out, vec![(2, "WARN: b".to_string()), (3, "warn: c".to_string())]);
        // cap keeps the newest
        let out = merge_filter(sets, "", 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 2);
        assert_eq!(out[1].0, 3);
    }

    #[test]
    fn builds_serializes_round_trip() {
        let channels = registry();
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "log"
channels = ["system.log"]
max_lines = 200"#,
        )
        .unwrap();
        let p = reg.build(&e, &channels).unwrap();
        assert_eq!(p.serialize(), e.config);
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let a = channels.id("system.log").unwrap();
        let b = channels.id("app.log").unwrap();
        store.write_text(a, 1, "boot ok".into());
        store.write_text(b, 2, "app started".into());
        let reg = PanelRegistry::with_builtins();
        let e: PanelEntry = toml::from_str(
            r#"type = "log"
channels = ["system.log", "app.log", "does.not.exist"]"#,
        )
        .unwrap();
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
