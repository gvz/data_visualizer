pub mod channels;
pub mod layout;

pub use channels::{ChannelConfig, ChannelRegistry};
pub use layout::{LayoutConfig, PanelEntry, ScreenConfig};

/// Built-in config used when no `config.toml` exists in the working directory.
/// An empty starter: no channels or panels, so the app opens to the panel
/// type picker. Written verbatim to disk when the user opts to save it.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses_to_empty_starter() {
        let channels = ChannelRegistry::from_toml_str(DEFAULT_CONFIG_TOML)
            .expect("default config channels parse");
        assert_eq!(channels.iter_ids().count(), 0, "default has no channels");

        let layout =
            LayoutConfig::from_toml_str(DEFAULT_CONFIG_TOML).expect("default config layout parse");
        assert!(layout.screens.is_empty(), "default has no screens");
    }
}
