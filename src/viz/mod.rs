//! Visualization panel trait and registry for the iced UI port.
//!
//! # Extension points
//!
//! Implement [`VizPanel`] for a new panel type, expose a `TYPE_NAME: &str`
//! constant and a constructor matching [`PanelCtor`], then register both in
//! [`PanelRegistry::with_builtins`].
//!
//! The `sync`/`view` split exists because [`iced_plot::PlotWidget`] is
//! stateful: `sync` calls `update_series`; `view` calls `widget.view()`.

pub mod canvas_util;
pub(crate) mod decimate;
pub mod gauge;
pub mod log;
pub mod numeric;
pub mod plot;
pub mod spectrum;
pub mod status;
pub mod waveform;
pub mod xy_scatter;

use std::collections::HashMap;

/// A panel-local configuration message, emitted by controls inside a panel.
///
/// Grows one variant per concrete panel type as panels are implemented in
/// later tasks.
#[derive(Debug, Clone)]
pub enum ConfigMsg {
    /// Placeholder — no concrete config messages yet.
    #[allow(dead_code)]
    Noop,
}

/// Messages a panel can emit.
///
/// `Plot` carries pan/zoom/pick events from `iced_plot`; `Config` carries
/// panel-specific configuration changes.
#[derive(Debug, Clone)]
pub enum PanelMessage {
    /// An event from an embedded `iced_plot` widget.
    Plot(iced_plot::PlotUiMessage),
    /// A panel-local configuration change.
    Config(ConfigMsg),
}

/// A single visualization panel: one cell in the workspace grid.
///
/// Panels read from [`crate::store::ChannelStore`] via [`sync`](VizPanel::sync)
/// (called each tick) and render via [`view`](VizPanel::view) (called each
/// frame). Keeping these two phases separate lets `iced_plot::PlotWidget`
/// update its internal GPU buffers in `sync` and render in `view` without
/// needing store access during rendering.
///
/// # Error contract
///
/// A binding failure (unknown channel, wrong type, bad config) must **not**
/// panic. [`PanelRegistry::build`] wraps failures in an [`ErrorPanel`] that
/// displays the message inline.
pub trait VizPanel {
    /// Human-readable title shown in the pane header.
    fn title(&self) -> String;

    /// Pull new samples from the store into internal widget state.
    ///
    /// Called each tick. May mutate cached series data or `PlotWidget` state.
    fn sync(&mut self, store: &dyn crate::store::ChannelStore);

    /// Render current state. Pure; no store access (data already synced).
    fn view(&self) -> iced::Element<'_, PanelMessage>;

    /// Handle a panel-local message (pan/zoom/config).
    fn update(&mut self, msg: PanelMessage);

    /// Render the panel's configuration controls.
    fn config_view(&self) -> iced::Element<'_, PanelMessage>;

    /// Attempt to bind a dragged channel; return `true` if accepted.
    fn bind_channel(&mut self, name: &str) -> bool;

    /// Serialize the panel's state to a TOML table (for layout persistence).
    ///
    /// Must round-trip: feeding this table back through the panel's
    /// [`PanelCtor`] reconstructs an equivalent panel.
    fn serialize(&self) -> toml::Table;
}

/// A constructor for a panel type. Receives the panel's config table and the
/// channel registry, and returns the panel.
///
/// Binding failures (unknown channel, wrong sample type) must NOT abort
/// construction — represent them as an inline error by returning
/// [`ErrorPanel::new(msg)`](ErrorPanel::new) as the panel. There is no `Err`.
pub type PanelCtor = Box<
    dyn Fn(
        &toml::Table,
        &crate::config::ChannelRegistry,
    ) -> Box<dyn VizPanel>,
>;

/// Registry mapping `layout.toml` `type` strings to their [`PanelCtor`].
///
/// Populated once at startup by [`PanelRegistry::with_builtins`] and
/// consulted by [`PanelRegistry::build`] to instantiate panels from a saved
/// layout.
pub struct PanelRegistry {
    ctors: HashMap<String, PanelCtor>,
}

impl PanelRegistry {
    /// A registry pre-loaded with every panel type the app ships.
    ///
    /// Currently empty — concrete panels are registered in their own tasks.
    pub fn with_builtins() -> Self {
        let mut reg = Self { ctors: HashMap::new() };
        reg.register(
            waveform::TYPE_NAME,
            Box::new(|cfg, ch| Box::new(waveform::Waveform::new(cfg, ch))),
        );
        reg.register(
            numeric::TYPE_NAME,
            Box::new(|cfg, ch| Box::new(numeric::Numeric::new(cfg, ch))),
        );
        reg.register(
            gauge::TYPE_NAME,
            Box::new(|cfg, ch| Box::new(gauge::Gauge::new(cfg, ch))),
        );
        reg.register(
            status::TYPE_NAME,
            Box::new(|cfg, ch| Box::new(status::Status::new(cfg, ch))),
        );
        reg.register(
            log::TYPE_NAME,
            Box::new(|cfg, ch| Box::new(log::Log::new(cfg, ch))),
        );
        reg.register(
            spectrum::TYPE_NAME,
            Box::new(|cfg, ch| Box::new(spectrum::Spectrum::new(cfg, ch))),
        );
        reg.register(
            xy_scatter::TYPE_NAME,
            Box::new(|cfg, ch| Box::new(xy_scatter::XyScatter::new(cfg, ch))),
        );
        reg
    }

    /// Register `ctor` under the `layout.toml` `type` string `name`,
    /// overwriting any existing entry with the same name.
    pub fn register(&mut self, name: impl Into<String>, ctor: PanelCtor) {
        self.ctors.insert(name.into(), ctor);
    }

    /// All registered panel type strings, sorted for stable UI listings.
    pub fn type_names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.ctors.keys().map(|s| s.as_str()).collect();
        v.sort_unstable();
        v
    }

    /// Build a panel from a layout entry.
    ///
    /// If the `panel_type` is not registered, or if the constructor returns an
    /// error, returns an [`ErrorPanel`] describing the failure — never panics.
    pub fn build(
        &self,
        entry: &crate::config::PanelEntry,
        ch: &crate::config::ChannelRegistry,
    ) -> Box<dyn VizPanel> {
        match self.ctors.get(entry.panel_type.as_str()) {
            Some(ctor) => ctor(&entry.config, ch),
            None => Box::new(ErrorPanel {
                message: format!("unknown panel type `{}`", entry.panel_type),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// ErrorPanel
// ---------------------------------------------------------------------------

/// A fallback panel displayed when a [`PanelCtor`] fails or when a panel type
/// is not registered. Shows the error message inline instead of panicking.
pub struct ErrorPanel {
    message: String,
}

impl ErrorPanel {
    /// Create a new error panel with the given message.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl VizPanel for ErrorPanel {
    fn title(&self) -> String {
        format!("Error: {}", self.message)
    }

    fn sync(&mut self, _store: &dyn crate::store::ChannelStore) {}

    fn view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, text};
        container(text(self.message.clone())).into()
    }

    fn update(&mut self, _msg: PanelMessage) {}

    fn config_view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, text};
        container(text("No configuration available for error panels.")).into()
    }

    fn bind_channel(&mut self, _name: &str) -> bool {
        false
    }

    fn serialize(&self) -> toml::Table {
        toml::Table::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_type_yields_error_panel_not_panic() {
        let reg = PanelRegistry::with_builtins();
        let ch = crate::config::ChannelRegistry::from_toml_str("").unwrap();
        let entry = crate::config::PanelEntry {
            panel_type: "does_not_exist".into(),
            config: toml::Table::new(),
        };
        let p = reg.build(&entry, &ch);
        assert!(
            p.title().to_lowercase().contains("error")
                || p.title().to_lowercase().contains("unknown")
        );
    }

    #[test]
    fn type_names_returns_sorted_list() {
        let reg = PanelRegistry::with_builtins();
        let names = reg.type_names();
        // with_builtins now registers the waveform panel
        assert!(names.contains(&"waveform"), "waveform must be registered");
        // the list must be sorted
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "type_names must be sorted");
    }

    #[test]
    fn registered_ctor_is_called() {
        let mut reg = PanelRegistry::with_builtins();
        reg.register(
            "test_panel",
            Box::new(|_config, _ch| {
                Box::new(ErrorPanel::new("test panel constructed"))
            }),
        );
        let ch = crate::config::ChannelRegistry::from_toml_str("").unwrap();
        let entry = crate::config::PanelEntry {
            panel_type: "test_panel".into(),
            config: toml::Table::new(),
        };
        let p = reg.build(&entry, &ch);
        assert!(p.title().contains("test panel constructed"));
    }
}
