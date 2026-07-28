use std::collections::HashMap;

use anyhow::anyhow;
use eframe::egui;

use crate::config::{ChannelRegistry, PanelEntry};
use crate::store::ChannelStore;
use crate::types::SampleType;

pub mod common;
pub mod decimate;
pub mod gauge;
pub mod log;
pub mod measure;
pub mod numeric;
pub mod placeholder;
pub mod spectrum;
pub mod state_graph;
pub mod status;
pub mod waveform;
pub mod xy_scatter;

/// A single visualization panel: one cell in the workspace grid that reads
/// channel samples and draws them.
///
/// Panels observe data exclusively through the [`ChannelStore`] trait, so a
/// panel cannot tell whether it is showing a live feed or a replayed
/// recording — the same `render` call serves both. This is the extension
/// point for new visualizations: implement this trait, expose a `TYPE_NAME`
/// and a [`PanelCtor`], and register both in [`PanelRegistry::with_builtins`].
///
/// # Lifecycle
///
/// A panel is built from a [`PanelEntry`] by its [`PanelCtor`], lives for as
/// long as the pane exists, and is persisted back to `layout.toml` via
/// [`serialize`](VizPanel::serialize). Each frame the workspace calls
/// [`render`](VizPanel::render); [`config_ui`](VizPanel::config_ui) is called
/// only while the panel's settings popup is open.
///
/// # Implementing
///
/// The five methods above the defaults are required. The default methods are
/// opt-in hooks for interactive behaviour (drag-and-drop rebinding, dynamic
/// channel discovery, linked zoom) — override only the ones a panel needs.
///
/// A binding problem (unknown channel, wrong sample type) must never abort
/// construction: the [`PanelCtor`] returns a panel that draws an inline error,
/// and `render` keeps working. See [`PanelCtor`] for the `Err` contract.
pub trait VizPanel {
    /// Human-readable panel title, shown in the pane header. Usually the bound
    /// channel name, or a fixed label for channel-less panels.
    fn title(&self) -> &str;

    /// Sample types this panel can display. Used to gate drag-and-drop and the
    /// channel picker so incompatible channels cannot be bound.
    fn accepted_types(&self) -> &[SampleType];

    /// Draw the panel's configuration controls into `ui`.
    ///
    /// Shown in the panel's settings popup/side area, not in the main view.
    /// Mutations take effect on the next [`render`](VizPanel::render).
    fn config_ui(&mut self, ui: &mut egui::Ui);

    /// Draw the panel for the current frame, reading samples from `store`.
    ///
    /// Called once per frame while the pane is visible. `store` abstracts over
    /// live and replay sources, so the same code path serves both. Must render
    /// an inline message rather than panic when its channel is unresolved.
    fn render(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore);

    /// The panel's own config keys, to be written under its entry in
    /// `layout.toml`.
    ///
    /// Must round-trip: feeding this table back through the panel's
    /// [`PanelCtor`] reconstructs an equivalent panel. Must **not** include the
    /// `"type"` key — [`PanelEntry`] carries the type separately.
    fn serialize(&self) -> toml::Table;

    /// Bind a channel dropped onto this panel via drag-and-drop.
    ///
    /// Single-channel panels replace their current channel; multi-channel
    /// panels append. `name` is the dropped channel; `reg` resolves it to an
    /// id and metadata. Default no-op for panels that take no channels.
    fn drop_channel(&mut self, _name: &str, _reg: &crate::config::ChannelRegistry) {}

    /// Re-attempt to resolve channels that were unknown at construction time.
    ///
    /// Called when new (dynamic) MQTT topics are discovered, letting a panel
    /// restored from a layout bind once its topic reappears. Default no-op for
    /// panels whose channels are always known up front.
    fn refresh_bindings(&mut self, _ctx: &common::RebindCtx) {}

    /// Clear any interactive zoom and resume the default/live view.
    ///
    /// Driven by the global "reset zoom" toolbar action, which fans out to
    /// every panel. Default no-op; panels with a zoom state (e.g. waveform)
    /// override.
    fn reset_zoom(&mut self) {}

    /// Freeze a shared linked time-window into this panel's own zoom state so
    /// it stays put after the link is released.
    ///
    /// Called only for panels participating in an active linked zoom, with
    /// `range` as the `(start, end)` timestamps in nanoseconds. Default no-op;
    /// waveform and state_graph override.
    fn freeze_time_zoom(&mut self, _range: (i64, i64)) {}
}

/// Builds a [`VizPanel`] from its serialized form.
///
/// The [`toml::Table`] is the panel's own config (the same shape
/// [`VizPanel::serialize`] produces); the [`ChannelRegistry`] resolves channel
/// names to ids and metadata.
///
/// # Errors
///
/// Return `Err` **only** for malformed config — e.g. a missing required key or
/// a value of the wrong TOML type. A *binding* problem (unknown channel name,
/// channel whose sample type the panel does not accept) is not an error: build
/// a panel that renders an inline message, so the pane survives and can rebind
/// later via [`VizPanel::drop_channel`] or [`VizPanel::refresh_bindings`].
pub type PanelCtor =
    fn(&toml::Table, &ChannelRegistry) -> anyhow::Result<Box<dyn VizPanel>>;

/// Registry mapping `layout.toml` `type` strings to their [`PanelCtor`].
///
/// Populated once at startup by [`with_builtins`](PanelRegistry::with_builtins)
/// and consulted by [`build`](PanelRegistry::build) to instantiate panels from
/// a saved layout. Call [`register`](PanelRegistry::register) to add a new
/// panel type.
pub struct PanelRegistry {
    ctors: HashMap<&'static str, PanelCtor>,
}

impl PanelRegistry {
    /// A registry pre-loaded with every panel type the app ships.
    pub fn with_builtins() -> Self {
        let mut reg = Self { ctors: HashMap::new() };
        reg.register(gauge::TYPE_NAME, gauge::ctor);
        reg.register(log::TYPE_NAME, log::ctor);
        reg.register(numeric::TYPE_NAME, numeric::ctor);
        reg.register(placeholder::TYPE_NAME, placeholder::ctor);
        reg.register(spectrum::TYPE_NAME, spectrum::ctor);
        reg.register(waveform::TYPE_NAME, waveform::ctor);
        reg.register(state_graph::TYPE_NAME, state_graph::ctor);
        reg.register(status::TYPE_NAME, status::ctor);
        reg.register(xy_scatter::TYPE_NAME, xy_scatter::ctor);
        reg
    }

    /// Register `ctor` under the `layout.toml` `type` string `name`,
    /// overwriting any existing entry with the same name.
    pub fn register(&mut self, name: &'static str, ctor: PanelCtor) {
        self.ctors.insert(name, ctor);
    }

    /// All registered panel type strings, sorted for stable UI listings.
    pub fn type_names(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self.ctors.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Type names a user can choose for a panel, excluding the internal
    /// `placeholder` type used for freshly-split, not-yet-defined panes.
    pub fn pickable_type_names(&self) -> Vec<&'static str> {
        self.type_names()
            .into_iter()
            .filter(|t| *t != placeholder::TYPE_NAME)
            .collect()
    }

    /// Build a panel from a saved [`PanelEntry`], dispatching on its
    /// `panel_type`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `panel_type` is not registered, or if the matched
    /// [`PanelCtor`] rejects the config (see [`PanelCtor`] for its error
    /// contract).
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
