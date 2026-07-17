use std::collections::BTreeMap;

use anyhow::anyhow;
use eframe::egui;
use egui_tiles::{Tile, TileId, Tree};

use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry, ScreenConfig};
use crate::store::ChannelStore;
use crate::types::SampleType;
use crate::viz::{PanelRegistry, VizPanel};

/// A panel plus the layout.toml type string needed to re-serialize it.
pub struct PanelSlot {
    pub type_name: String,
    pub panel: Box<dyn VizPanel>,
}

/// One screen: a tile tree whose panes are indices into `panels`.
pub struct ScreenState {
    pub tree: Tree<usize>,
    pub panels: Vec<PanelSlot>,
}

/// All screens + which one is showing. The dockable-layout engine.
pub struct Workspace {
    pub screens: BTreeMap<String, ScreenState>,
    pub active: String,
}

/// Stand-in for a panel whose constructor failed (e.g. unknown type when the
/// layout file came from a newer build). Renders the error, and re-serializes
/// the ORIGINAL config so saving the layout never destroys user data.
struct ErrorPanel {
    title: String,
    msg: String,
    orig: toml::Table,
}

impl VizPanel for ErrorPanel {
    fn title(&self) -> &str {
        &self.title
    }
    fn accepted_types(&self) -> &[SampleType] {
        &[]
    }
    fn config_ui(&mut self, _ui: &mut egui::Ui) {}
    fn render(&mut self, ui: &mut egui::Ui, _store: &dyn ChannelStore) {
        ui.colored_label(egui::Color32::RED, &self.msg);
    }
    fn serialize(&self) -> toml::Table {
        self.orig.clone()
    }
}

fn default_tree(name: &str, n: usize) -> Tree<usize> {
    Tree::new_grid(egui::Id::new(("screen", name)), (0..n).collect())
}

/// A persisted tree is usable only if its panes are exactly {0..n} once each.
fn tree_panes_valid(t: &Tree<usize>, n: usize) -> bool {
    let mut seen = vec![false; n];
    let mut count = 0;
    for (_, tile) in t.tiles.iter() {
        if let Tile::Pane(i) = tile {
            if *i >= n || seen[*i] {
                return false;
            }
            seen[*i] = true;
            count += 1;
        }
    }
    count == n
}

impl ScreenState {
    fn empty(name: &str) -> Self {
        Self { tree: Tree::empty(egui::Id::new(("screen", name))), panels: Vec::new() }
    }

    fn from_screen_config(
        name: &str,
        sc: &ScreenConfig,
        reg: &PanelRegistry,
        channels: &ChannelRegistry,
    ) -> Self {
        let panels: Vec<PanelSlot> = sc
            .panels
            .iter()
            .map(|entry| {
                let panel = reg.build(entry, channels).unwrap_or_else(|e| {
                    Box::new(ErrorPanel {
                        title: format!("{} (error)", entry.panel_type),
                        msg: e.to_string(),
                        orig: entry.config.clone(),
                    })
                });
                PanelSlot { type_name: entry.panel_type.clone(), panel }
            })
            .collect();
        let tree = sc
            .tiles_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<Tree<usize>>(j).ok())
            .filter(|t| tree_panes_valid(t, panels.len()))
            .unwrap_or_else(|| default_tree(name, panels.len()));
        Self { tree, panels }
    }
}

impl Workspace {
    pub fn from_config(
        cfg: &LayoutConfig,
        reg: &PanelRegistry,
        channels: &ChannelRegistry,
    ) -> Self {
        let mut screens = BTreeMap::new();
        for (name, sc) in &cfg.screens {
            screens.insert(
                name.clone(),
                ScreenState::from_screen_config(name, sc, reg, channels),
            );
        }
        if screens.is_empty() {
            screens.insert("main".to_string(), ScreenState::empty("main"));
        }
        let active = screens.keys().next().unwrap().clone();
        Self { screens, active }
    }

    pub fn to_config(&self) -> LayoutConfig {
        let mut cfg = LayoutConfig::default();
        for (name, st) in &self.screens {
            // Panes in TileId order → deterministic panel order in the file.
            let mut pane_tiles: Vec<(TileId, usize)> = st
                .tree
                .tiles
                .iter()
                .filter_map(|(id, tile)| match tile {
                    Tile::Pane(i) => Some((*id, *i)),
                    _ => None,
                })
                .collect();
            // TileId does not implement Ord; sort by the underlying u64.
            pane_tiles.sort_by_key(|(id, _)| id.0);

            let mut remap = vec![usize::MAX; st.panels.len()];
            let mut entries = Vec::new();
            for (new_idx, (_, old_idx)) in pane_tiles.iter().enumerate() {
                remap[*old_idx] = new_idx;
                let slot = &st.panels[*old_idx];
                entries.push(PanelEntry {
                    panel_type: slot.type_name.clone(),
                    config: slot.panel.serialize(),
                });
            }
            // Clone the tree with panes renumbered to the new order.
            let mut tree = st.tree.clone();
            let ids: Vec<TileId> = tree.tiles.iter().map(|(id, _)| *id).collect();
            for id in ids {
                if let Some(Tile::Pane(i)) = tree.tiles.get_mut(id) {
                    *i = remap[*i];
                }
            }
            cfg.screens.insert(
                name.clone(),
                ScreenConfig {
                    panels: entries,
                    tiles_json: serde_json::to_string(&tree).ok(),
                },
            );
        }
        cfg
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, store: &dyn ChannelStore) {
        let Some(st) = self.screens.get_mut(&self.active) else {
            return;
        };
        let mut behavior = TreeBehavior { store, panels: &mut st.panels };
        st.tree.ui(&mut behavior, ui);
    }

    pub fn add_screen(&mut self, name: &str) {
        if !self.screens.contains_key(name) {
            self.screens.insert(name.to_string(), ScreenState::empty(name));
        }
        self.active = name.to_string();
    }

    pub fn add_panel(
        &mut self,
        entry: &PanelEntry,
        reg: &PanelRegistry,
        channels: &ChannelRegistry,
    ) -> anyhow::Result<()> {
        let st = self
            .screens
            .get_mut(&self.active)
            .ok_or_else(|| anyhow!("no active screen"))?;
        let panel = reg.build(entry, channels)?;
        let idx = st.panels.len();
        st.panels.push(PanelSlot { type_name: entry.panel_type.clone(), panel });
        let pane = st.tree.tiles.insert_pane(idx);
        match st.tree.root {
            None => st.tree.root = Some(pane),
            Some(root) => match st.tree.tiles.get_mut(root) {
                Some(Tile::Container(c)) => c.add_child(pane),
                _ => {
                    let new_root = st.tree.tiles.insert_tab_tile(vec![root, pane]);
                    st.tree.root = Some(new_root);
                }
            },
        }
        Ok(())
    }
}

/// egui_tiles glue: renders a pane by delegating to its panel; tab drag &
/// drop and splitting come free from egui_tiles.
struct TreeBehavior<'a> {
    store: &'a dyn ChannelStore,
    panels: &'a mut Vec<PanelSlot>,
}

impl egui_tiles::Behavior<usize> for TreeBehavior<'_> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        _tile_id: TileId,
        pane: &mut usize,
    ) -> egui_tiles::UiResponse {
        if let Some(slot) = self.panels.get_mut(*pane) {
            egui::CollapsingHeader::new("settings")
                .id_source((*pane, "panel-settings"))
                .show(ui, |ui| slot.panel.config_ui(ui));
            slot.panel.render(ui, self.store);
        }
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &usize) -> egui::WidgetText {
        self.panels
            .get(*pane)
            .map(|s| s.panel.title().to_string())
            .unwrap_or_default()
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::PanelRegistry;
    use eframe::egui;

    fn channels() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."demo.sine"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"

[channels."demo.counter"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"
"#,
        )
        .unwrap()
    }

    const LAYOUT: &str = r#"
[screens.main]
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"

[[screens.main.panels]]
type = "numeric"
channel = "demo.counter"

[screens.aux]
[[screens.aux.panels]]
type = "numeric"
channel = "demo.sine"
"#;

    fn build() -> (ChannelRegistry, PanelRegistry, Workspace) {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let cfg = LayoutConfig::from_toml_str(LAYOUT).unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        (ch, reg, ws)
    }

    fn pane_count(st: &ScreenState) -> usize {
        st.tree
            .tiles
            .iter()
            .filter(|(_, t)| matches!(t, egui_tiles::Tile::Pane(_)))
            .count()
    }

    #[test]
    fn from_config_builds_default_grid() {
        let (_, _, ws) = build();
        assert_eq!(ws.screens.len(), 2);
        assert_eq!(ws.active, "aux"); // BTreeMap order: first key
        assert_eq!(pane_count(&ws.screens["main"]), 2);
        assert_eq!(ws.screens["main"].panels.len(), 2);
    }

    #[test]
    fn round_trip_preserves_panels_and_restores_tree() {
        let (ch, reg, ws) = build();
        let cfg = ws.to_config();
        assert_eq!(cfg.screens.len(), 2);
        let main = &cfg.screens["main"];
        assert_eq!(main.panels.len(), 2);
        assert_eq!(main.panels[0].panel_type, "numeric");
        assert!(main.tiles_json.is_some());
        // Reload: tree restored from tiles_json, still consistent.
        let ws2 = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(pane_count(&ws2.screens["main"]), 2);
        let cfg2 = ws2.to_config();
        assert_eq!(cfg.screens["main"].panels, cfg2.screens["main"].panels);
    }

    #[test]
    fn invalid_tiles_json_falls_back_to_grid() {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let mut cfg = LayoutConfig::from_toml_str(LAYOUT).unwrap();
        cfg.screens.get_mut("main").unwrap().tiles_json = Some("not json".into());
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(pane_count(&ws.screens["main"]), 2);
    }

    #[test]
    fn unknown_panel_type_becomes_error_panel_and_keeps_config_on_save() {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let cfg = LayoutConfig::from_toml_str(
            r#"
[screens.main]
[[screens.main.panels]]
type = "hologram"
channel = "demo.sine"
setting = 42
"#,
        )
        .unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(ws.screens["main"].panels.len(), 1);
        let saved = ws.to_config();
        let entry = &saved.screens["main"].panels[0];
        assert_eq!(entry.panel_type, "hologram"); // original type preserved
        assert_eq!(entry.config["setting"], toml::Value::Integer(42));
    }

    #[test]
    fn add_panel_and_add_screen() {
        let (ch, reg, mut ws) = build();
        ws.add_screen("fresh");
        assert_eq!(ws.active, "fresh");
        assert_eq!(pane_count(&ws.screens["fresh"]), 0);
        let entry = PanelEntry {
            panel_type: "numeric".into(),
            config: toml::from_str(r#"channel = "demo.sine""#).unwrap(),
        };
        ws.add_panel(&entry, &reg, &ch).unwrap();
        ws.add_panel(&entry, &reg, &ch).unwrap();
        assert_eq!(pane_count(&ws.screens["fresh"]), 2);
        assert_eq!(ws.screens["fresh"].panels.len(), 2);
        // Unknown type propagates Err (interactive path — user sees it).
        let bad = PanelEntry { panel_type: "hologram".into(), config: toml::Table::new() };
        assert!(ws.add_panel(&bad, &reg, &ch).is_err());
    }

    #[test]
    fn ui_renders_headless_without_panic() {
        let (ch, _, mut ws) = build();
        let store = LiveStore::from_registry(&ch);
        store.write_numeric(ch.id("demo.sine").unwrap(), 1, NumericVal::Float(1.0));
        for screen in ["aux", "main"] {
            ws.active = screen.to_string();
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ws.ui(ui, &store);
                });
            });
        }
    }
}
