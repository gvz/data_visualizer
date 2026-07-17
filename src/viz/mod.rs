use std::collections::HashMap;

use anyhow::anyhow;
use eframe::egui;

use crate::config::{ChannelRegistry, PanelEntry};
use crate::store::ChannelStore;
use crate::types::SampleType;

pub mod common;
pub mod decimate;
pub mod gauge;
pub mod measure;
pub mod numeric;
pub mod spectrum;
pub mod state_graph;
pub mod waveform;
pub mod xy_scatter;

/// A visualization panel. Panels only see the ChannelStore trait — live vs
/// replay is transparent here.
pub trait VizPanel {
    fn title(&self) -> &str;
    fn accepted_types(&self) -> &[SampleType];
    /// Panel settings UI (shown in a config popup/side area).
    fn config_ui(&mut self, ui: &mut egui::Ui);
    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore);
    /// Panel-specific config keys for layout.toml. Must NOT include "type" —
    /// PanelEntry carries that.
    fn serialize(&self) -> toml::Table;
}

/// Constructor: panel-specific toml table + channel registry (for resolving
/// channel names to ids) → boxed panel. Binding problems (unknown channel,
/// wrong type) must produce a panel that renders an inline error, not Err —
/// Err is for malformed config only (e.g. missing required key).
pub type PanelCtor =
    fn(&toml::Table, &ChannelRegistry) -> anyhow::Result<Box<dyn VizPanel>>;

/// Maps layout.toml `type` strings to constructors. Later plans call
/// `register` for each new panel type.
pub struct PanelRegistry {
    ctors: HashMap<&'static str, PanelCtor>,
}

impl PanelRegistry {
    pub fn with_builtins() -> Self {
        let mut reg = Self { ctors: HashMap::new() };
        reg.register(gauge::TYPE_NAME, gauge::ctor);
        reg.register(numeric::TYPE_NAME, numeric::ctor);
        reg.register(spectrum::TYPE_NAME, spectrum::ctor);
        reg.register(waveform::TYPE_NAME, waveform::ctor);
        reg.register(state_graph::TYPE_NAME, state_graph::ctor);
        reg.register(xy_scatter::TYPE_NAME, xy_scatter::ctor);
        reg
    }

    pub fn register(&mut self, name: &'static str, ctor: PanelCtor) {
        self.ctors.insert(name, ctor);
    }

    pub fn build(
        &self,
        entry: &PanelEntry,
        channels: &ChannelRegistry,
    ) -> anyhow::Result<Box<dyn VizPanel>> {
        let ctor = self
            .ctors
            .get(entry.panel_type.as_str())
            .ok_or_else(|| anyhow!("unknown panel type `{}`", entry.panel_type))?;
        ctor(&entry.config, channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use eframe::egui;

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
unit = "V"
max_rate = 100
history_s = 1.0

[channels."demo.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
"#,
        )
        .unwrap()
    }

    fn entry(toml_src: &str) -> PanelEntry {
        toml::from_str(toml_src).unwrap()
    }

    #[test]
    fn builds_numeric_panel_from_entry() {
        let channels = registry();
        let panels = PanelRegistry::with_builtins();
        let e = entry(r#"type = "numeric"
channel = "demo.sine""#);
        let p = panels.build(&e, &channels).unwrap();
        assert_eq!(p.title(), "demo.sine");
    }

    #[test]
    fn unknown_panel_type_is_error() {
        let channels = registry();
        let panels = PanelRegistry::with_builtins();
        let e = entry(r#"type = "hologram"
channel = "demo.sine""#);
        assert!(panels.build(&e, &channels).is_err());
    }

    #[test]
    fn serialize_round_trips_through_registry() {
        let channels = registry();
        let panels = PanelRegistry::with_builtins();
        let e = entry(r#"type = "numeric"
channel = "demo.sine""#);
        let p = panels.build(&e, &channels).unwrap();
        let cfg = p.serialize();
        assert_eq!(cfg, e.config); // same panel-specific keys back out
        let e2 = PanelEntry { panel_type: "numeric".into(), config: cfg };
        assert!(panels.build(&e2, &channels).is_ok());
    }

    #[test]
    fn renders_headless_without_panic() {
        let channels = registry();
        let store = LiveStore::from_registry(&channels);
        let id = channels.id("demo.sine").unwrap();
        store.write_numeric(id, 1, NumericVal::Float(3.25));

        let panels = PanelRegistry::with_builtins();
        // valid binding, missing channel, and wrong-type binding must all
        // render an inline result/error — never panic.
        let sources = [
            r#"type = "numeric"
channel = "demo.sine""#,
            r#"type = "numeric"
channel = "does.not.exist""#,
            r#"type = "numeric"
channel = "demo.log""#,
        ];
        for src in sources {
            let mut p = panels.build(&entry(src), &channels).unwrap();
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
