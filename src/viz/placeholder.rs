//! Iced-port placeholder panel — empty-pane prompt shown before a real panel
//! type is chosen.
//!
//! The pane renderer (Task 12) will overlay a type-picker UI on top of this;
//! for now `view` shows a centered hint text so the pane is visually
//! recognizable as unconfigured.

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::viz::{PanelMessage, VizPanel};

pub const TYPE_NAME: &str = "placeholder";

/// A freshly-split, not-yet-configured pane.
///
/// This is a real registered panel type so an unconfigured pane survives a
/// layout save/reload.  The workspace (Task 12) draws the interactive picker
/// on top; this panel is never used as a data visualization in its own right.
pub struct PlaceholderPanel;

impl PlaceholderPanel {
    /// Construct a placeholder — ignores config entirely.
    pub fn new(_cfg: &toml::Table, _ch: &ChannelRegistry) -> Self {
        PlaceholderPanel
    }
}

impl VizPanel for PlaceholderPanel {
    fn title(&self) -> String {
        "Empty".to_string()
    }

    fn sync(&mut self, _store: &dyn ChannelStore) {}

    fn view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, text};
        use iced::{Alignment, Length};
        container(text("Choose a panel type").size(16))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into()
    }

    fn update(&mut self, _msg: PanelMessage) {}

    fn config_view(&self) -> iced::Element<'_, PanelMessage> {
        use iced::widget::{container, text};
        container(text("No configuration — choose a panel type first.")).into()
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
    use crate::config::ChannelRegistry;

    fn empty_registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str("").unwrap()
    }

    #[test]
    fn placeholder_constructs_without_panic() {
        let ch = empty_registry();
        let _p = PlaceholderPanel::new(&toml::Table::new(), &ch);
    }

    #[test]
    fn placeholder_title_is_empty() {
        let ch = empty_registry();
        let p = PlaceholderPanel::new(&toml::Table::new(), &ch);
        assert_eq!(p.title(), "Empty");
    }

    #[test]
    fn placeholder_view_builds_without_panic() {
        let ch = empty_registry();
        let p = PlaceholderPanel::new(&toml::Table::new(), &ch);
        let _ = p.view(); // must not panic
    }

    #[test]
    fn placeholder_serialize_returns_empty_table() {
        let ch = empty_registry();
        let p = PlaceholderPanel::new(&toml::Table::new(), &ch);
        assert!(p.serialize().is_empty());
    }

    #[test]
    fn placeholder_bind_channel_returns_false() {
        let ch = empty_registry();
        let mut p = PlaceholderPanel::new(&toml::Table::new(), &ch);
        assert!(!p.bind_channel("some_channel"));
    }
}
