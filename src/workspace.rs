use std::collections::BTreeMap;

use anyhow::anyhow;
use eframe::egui;
use egui_phosphor::regular as icon;
use egui_tiles::{Container, LinearDir, Tile, TileId, Tree};

use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry, ScreenConfig};
use crate::dynamic_channel::{resolve_or_register_drop, MqttTopicMap};
use crate::store::ChannelStore;
use crate::types::SampleType;
use crate::viz::{placeholder, PanelRegistry, VizPanel};

/// Drop-time context for registering discovered MQTT topics on the fly:
/// the shared routing table and the current discovered-topic snapshot.
pub type MqttDropCtx<'a> = (&'a MqttTopicMap, &'a BTreeMap<String, String>);

/// A panel plus the layout.toml type string needed to re-serialize it.
pub struct PanelSlot {
    pub type_name: String,
    pub panel: Box<dyn VizPanel>,
}

/// One screen: a tile tree whose panes are indices into `panels`.
pub struct ScreenState {
    pub tree: Tree<usize>,
    pub panels: Vec<PanelSlot>,
    /// Pane index whose settings window is currently open, if any.
    pub settings_open: Option<usize>,
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

/// Deserialize a persisted tile tree, tolerating egui_tiles' non-round-trippable
/// `width`/`height`. Those are `f32` defaulting to `INFINITY` ("fill available
/// space"); serde_json writes infinity as `null`, which then fails to parse back
/// into `f32` — so a naive `from_str` rejects every tree we ever saved and the
/// whole layout (panel sizes and splits) is silently lost. Restore the null
/// fields to a value above `f32::MAX` so the cast yields infinity again.
fn parse_tree(json: &str) -> Option<Tree<usize>> {
    let mut v: serde_json::Value = serde_json::from_str(json).ok()?;
    if let Some(obj) = v.as_object_mut() {
        for k in ["width", "height"] {
            if obj.get(k).is_some_and(serde_json::Value::is_null) {
                obj.insert(k.to_string(), serde_json::json!(1e40));
            }
        }
    }
    serde_json::from_value(v).ok()
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
        Self {
            tree: Tree::empty(egui::Id::new(("screen", name))),
            panels: Vec::new(),
            settings_open: None,
        }
    }

    fn remove_panel(&mut self, tile_id: TileId) {
        let pane_idx = match self.tree.tiles.get(tile_id) {
            Some(Tile::Pane(i)) => *i,
            _ => return,
        };
        self.tree.remove_recursively(tile_id);
        if pane_idx < self.panels.len() {
            self.panels.remove(pane_idx);
        }
        // Panel indices shift on removal; drop any open settings window rather
        // than let it point at the wrong (or a since-removed) panel.
        self.settings_open = None;
        // Shift down any pane indices that were above the removed one.
        for (_, tile) in self.tree.tiles.iter_mut() {
            if let Tile::Pane(i) = tile {
                if *i > pane_idx {
                    *i -= 1;
                }
            }
        }
    }

    /// Split `target` (a pane) into a new linear container laid out along `dir`,
    /// keeping the existing panel and adding a fresh *undefined* pane beside it.
    /// The undefined pane renders type-picker buttons until the user chooses a
    /// type (see [`Self::define_panel`]). The target's TileId is reused for the
    /// new container so its position in the parent (or the root pointer) is
    /// intact, which sidesteps parent-kind-specific child rewiring.
    fn split_panel(&mut self, target: TileId, dir: LinearDir) {
        let old_idx = match self.tree.tiles.get(target) {
            Some(Tile::Pane(i)) => *i,
            _ => return,
        };
        let new_idx = self.panels.len();
        self.panels.push(PanelSlot {
            type_name: placeholder::TYPE_NAME.to_string(),
            panel: Box::new(placeholder::PlaceholderPanel),
        });
        let moved = self.tree.tiles.insert_pane(old_idx);
        let added = self.tree.tiles.insert_pane(new_idx);
        if let Some(tile) = self.tree.tiles.get_mut(target) {
            *tile = Tile::Container(Container::new_linear(dir, vec![moved, added]));
        }
    }

    /// Replace the pane at `idx` (typically an undefined placeholder) with a
    /// fresh, unconfigured panel of `type_name`. Binding-less build failures
    /// fall back to an inline error panel, matching [`Self::from_screen_config`].
    fn define_panel(
        &mut self,
        idx: usize,
        type_name: &str,
        reg: &PanelRegistry,
        channels: &ChannelRegistry,
    ) {
        let Some(slot) = self.panels.get_mut(idx) else {
            return;
        };
        let entry = PanelEntry { panel_type: type_name.to_string(), config: toml::Table::new() };
        let panel = reg.build(&entry, channels).unwrap_or_else(|e| {
            Box::new(ErrorPanel {
                title: format!("{type_name} (unconfigured)"),
                msg: e.to_string(),
                orig: entry.config.clone(),
            })
        });
        slot.type_name = type_name.to_string();
        slot.panel = panel;
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
            .and_then(parse_tree)
            .filter(|t| tree_panes_valid(t, panels.len()))
            .unwrap_or_else(|| default_tree(name, panels.len()));
        Self { tree, panels, settings_open: None }
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

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        store: &dyn ChannelStore,
        channels: &ChannelRegistry,
        reg: &PanelRegistry,
        mqtt: Option<MqttDropCtx>,
    ) {
        let type_names = reg.pickable_type_names();
        let Some(st) = self.screens.get_mut(&self.active) else {
            return;
        };
        let mut behavior = TreeBehavior {
            store,
            panels: &mut st.panels,
            pending_remove: None,
            pending_split: None,
            pending_define: None,
            pending_settings: None,
            type_names: &type_names,
            channels,
            mqtt,
        };
        st.tree.ui(&mut behavior, ui);
        let pending_remove = behavior.pending_remove;
        let pending_split = behavior.pending_split;
        let pending_define = behavior.pending_define;
        let pending_settings = behavior.pending_settings;
        if let Some(pane) = pending_settings {
            st.settings_open = Some(pane);
        }
        if let Some(tile_id) = pending_remove {
            st.remove_panel(tile_id);
        }
        if let Some((tile_id, dir)) = pending_split {
            st.split_panel(tile_id, dir);
        }
        if let Some((idx, type_name)) = pending_define {
            st.define_panel(idx, &type_name, reg, channels);
        }

        // Floating settings window for the panel picked via the context menu.
        // Rendered at the screen level (not inside the pane) so it can move
        // freely and overlay other panels.
        if let Some(pane) = st.settings_open {
            if let Some(slot) = st.panels.get_mut(pane) {
                let mut open = true;
                egui::Window::new(format!("{} settings", slot.panel.title()))
                    .id(egui::Id::new(("panel-settings-window", pane)))
                    .collapsible(false)
                    .resizable(true)
                    .open(&mut open)
                    .show(ui.ctx(), |ui| slot.panel.config_ui(ui));
                if !open {
                    st.settings_open = None;
                }
            } else {
                st.settings_open = None;
            }
        }
    }

    /// Re-attempt to resolve every panel's unknown channels against
    /// newly-discovered MQTT topics. Cheap (hashmap lookups per unresolved
    /// binding); call when the discovered-topic snapshot changes. This is what
    /// lets a layout referencing a drop-created (dynamic) MQTT channel bind
    /// after restart, once the broker republishes that topic.
    pub fn refresh_bindings(
        &mut self,
        channels: &ChannelRegistry,
        store: &dyn ChannelStore,
        mqtt: Option<MqttDropCtx>,
    ) {
        let ctx = crate::viz::common::RebindCtx { channels, store, mqtt };
        for st in self.screens.values_mut() {
            for slot in &mut st.panels {
                slot.panel.refresh_bindings(&ctx);
            }
        }
    }

    /// Clear the interactive time-zoom on every panel across all screens, so
    /// one toolbar click returns the whole workspace to its live/default view.
    pub fn reset_zoom(&mut self) {
        for st in self.screens.values_mut() {
            for slot in &mut st.panels {
                slot.panel.reset_zoom();
            }
        }
    }

    /// Copy a linked time-window into every panel's own zoom state across all
    /// screens — used when the toolbar link is released so panels stay frozen
    /// where they were.
    pub fn freeze_time_zoom(&mut self, range: (i64, i64)) {
        for st in self.screens.values_mut() {
            for slot in &mut st.panels {
                slot.panel.freeze_time_zoom(range);
            }
        }
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
        let panel = reg.build(entry, channels).unwrap_or_else(|e| {
            Box::new(ErrorPanel {
                title: format!("{} (unconfigured)", entry.panel_type),
                msg: e.to_string(),
                orig: entry.config.clone(),
            })
        });
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

/// A phosphor glyph that visually hints at each panel type, shown on the
/// undefined-pane type picker. Unknown types fall back to a plain square.
fn type_pictogram(type_name: &str) -> &'static str {
    match type_name {
        "gauge" => icon::GAUGE,
        "waveform" => icon::WAVE_SINE,
        "numeric" => icon::HASH,
        "spectrum" => icon::CHART_BAR,
        "state_graph" => icon::WAVE_SQUARE,
        "status" => icon::TAG,
        "log" => icon::SCROLL,
        "xy_scatter" => icon::CHART_SCATTER,
        _ => icon::SQUARE,
    }
}

/// egui_tiles glue: renders a pane by delegating to its panel; tab drag &
/// drop and splitting come free from egui_tiles.
struct TreeBehavior<'a> {
    store: &'a dyn ChannelStore,
    channels: &'a ChannelRegistry,
    panels: &'a mut Vec<PanelSlot>,
    pending_remove: Option<TileId>,
    /// Requested split: (target pane, layout direction). The new pane starts
    /// undefined and shows type-picker buttons.
    pending_split: Option<(TileId, LinearDir)>,
    /// Requested definition of an undefined pane: (pane index, chosen type).
    pending_define: Option<(usize, String)>,
    /// Pane index whose settings window was just requested via context menu.
    pending_settings: Option<usize>,
    /// Panel type names offered by the undefined-pane picker.
    type_names: &'a [&'static str],
    mqtt: Option<MqttDropCtx<'a>>,
}

impl egui_tiles::Behavior<usize> for TreeBehavior<'_> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        tile_id: TileId,
        pane: &mut usize,
    ) -> egui_tiles::UiResponse {
        // Allocate the pane-wide interaction FIRST so the panel's own widgets
        // (settings foldout, drag values, sliders) are registered afterwards and
        // therefore sit on top in z-order. Interacting over `max_rect` after the
        // content would instead cover it and swallow every click — leaving the
        // "settings" header and all config widgets unresponsive.
        let pane_rect = ui.max_rect();
        let resp =
            ui.interact(pane_rect, ui.id().with("pane_ctx"), egui::Sense::hover());

        let is_undefined = self
            .panels
            .get(*pane)
            .map(|s| s.type_name == placeholder::TYPE_NAME)
            .unwrap_or(false);
        if is_undefined {
            // Freshly-split pane: show a grid of pictogram tiles, one per panel
            // type. Choosing one replaces this slot with that panel type.
            let type_names = self.type_names;
            let mut chosen: Option<String> = None;
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Choose a panel type").weak());
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for t in type_names {
                        ui.allocate_ui(egui::vec2(64.0, 74.0), |ui| {
                            ui.vertical_centered(|ui| {
                                let glyph = type_pictogram(t);
                                let btn = egui::Button::new(egui::RichText::new(glyph).size(30.0))
                                    .min_size(egui::vec2(52.0, 52.0));
                                if ui.add(btn).on_hover_text(*t).clicked() {
                                    chosen = Some(t.to_string());
                                }
                                ui.label(egui::RichText::new(*t).small());
                            });
                        });
                    }
                });
            });
            if let Some(t) = chosen {
                self.pending_define = Some((*pane, t));
            }
        } else if let Some(slot) = self.panels.get_mut(*pane) {
            // Panel label, always shown above the content (the tab bar is not
            // visible for single panes or grid layouts).
            let title = slot.panel.title().to_string();
            if !title.is_empty() {
                ui.strong(title);
            }
            slot.panel.render(ui, self.store);
        }

        // Highlight when one or more channels are dragged over this panel.
        if resp.dnd_hover_payload::<Vec<String>>().is_some() {
            ui.painter().rect_stroke(
                pane_rect,
                2.0_f32,
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(80, 140, 255)),
            );
        }

        // Accept dropped channel names or MQTT topics. The payload carries every
        // selected leaf (one for a plain drag). Configured channels resolve
        // directly; an unconfigured but discovered MQTT topic is registered on
        // the fly (new store slot + routing). Names that resolve to nothing are
        // silently skipped; multi-channel panels accumulate them in order while
        // single-value panels keep the last.
        if let Some(dropped) = resp.dnd_release_payload::<Vec<String>>() {
            for raw in dropped.iter() {
                if let Some(name) =
                    resolve_or_register_drop(raw, self.channels, self.store, self.mqtt)
                {
                    if let Some(slot) = self.panels.get_mut(*pane) {
                        slot.panel.drop_channel(&name, self.channels);
                    }
                }
            }
        }

        resp.context_menu(|ui| {
            if ui.button(format!("{} Settings", icon::GEAR)).clicked() {
                self.pending_settings = Some(*pane);
                ui.close_menu();
            }
            if ui.button(format!("{} Reset zoom", icon::MAGNIFYING_GLASS_MINUS)).clicked() {
                if let Some(slot) = self.panels.get_mut(*pane) {
                    slot.panel.reset_zoom();
                }
                ui.close_menu();
            }
            ui.separator();
            if ui.button(format!("{} Split horizontal", icon::COLUMNS)).clicked() {
                self.pending_split = Some((tile_id, LinearDir::Horizontal));
                ui.close_menu();
            }
            if ui.button(format!("{} Split vertical", icon::ROWS)).clicked() {
                self.pending_split = Some((tile_id, LinearDir::Vertical));
                ui.close_menu();
            }
            ui.separator();
            if ui.button(format!("{} Delete panel", icon::TRASH)).clicked() {
                self.pending_remove = Some(tile_id);
                ui.close_menu();
            }
        });
        egui_tiles::UiResponse::None
    }

    fn tab_title_for_pane(&mut self, pane: &usize) -> egui::WidgetText {
        self.panels
            .get(*pane)
            .map(|s| s.panel.title().to_string())
            .unwrap_or_default()
            .into()
    }

    /// Pick grid columns from the child count alone, ignoring the available
    /// rect. The default heuristic recomputes columns from the window's
    /// aspect ratio every frame, so resizing the window reflows the grid and
    /// panels jump to new cells. A rect-independent count keeps the
    /// arrangement fixed, so panels simply resize to fill the new width.
    fn grid_auto_column_count(
        &self,
        num_visible_children: usize,
        _rect: egui::Rect,
        _gap: f32,
    ) -> usize {
        (num_visible_children as f32).sqrt().ceil().max(1.0) as usize
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
    fn panel_sizes_survive_round_trip() {
        use egui_tiles::{Container, Tile};
        let (ch, reg, mut ws) = build();
        // Simulate a user resize by editing the grid's row/col shares.
        let st = ws.screens.get_mut("main").unwrap();
        let ids: Vec<_> = st.tree.tiles.iter().map(|(id, _)| *id).collect();
        let mut set = false;
        for id in ids {
            if let Some(Tile::Container(Container::Grid(g))) = st.tree.tiles.get_mut(id) {
                g.col_shares = vec![2.5];
                g.row_shares = vec![4.0, 1.0];
                set = true;
            }
        }
        assert!(set, "main screen should have a grid container");

        // Save → reload and confirm the shares (sizes) came back, i.e. the tree
        // was NOT discarded and rebuilt as a default grid.
        let cfg = ws.to_config();
        let ws2 = Workspace::from_config(&cfg, &reg, &ch);
        let mut found = false;
        for (_, t) in ws2.screens["main"].tree.tiles.iter() {
            if let Tile::Container(Container::Grid(g)) = t {
                assert_eq!(g.col_shares, vec![2.5]);
                assert_eq!(g.row_shares, vec![4.0, 1.0]);
                found = true;
            }
        }
        assert!(found, "grid with restored shares must exist after reload");
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
        // Unknown type creates an ErrorPanel so the user sees it inline.
        let bad = PanelEntry { panel_type: "hologram".into(), config: toml::Table::new() };
        ws.add_panel(&bad, &reg, &ch).unwrap();
        assert_eq!(pane_count(&ws.screens["fresh"]), 3);
        assert_eq!(ws.screens["fresh"].panels.len(), 3);
    }

    #[test]
    fn ui_renders_headless_without_panic() {
        let (ch, reg, mut ws) = build();
        let store = LiveStore::from_registry(&ch);
        store.write_numeric(ch.id("demo.sine").unwrap(), 1, NumericVal::Float(1.0));
        for screen in ["aux", "main"] {
            ws.active = screen.to_string();
            let ctx = egui::Context::default();
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ws.ui(ui, &store, &ch, &reg, None);
                });
            });
        }
    }

    #[test]
    fn undefined_pane_picker_renders_headless_without_panic() {
        let (ch, reg, mut ws) = build();
        // Split a pane so the active screen holds an undefined placeholder pane
        // that renders the pictogram type picker.
        let st = ws.screens.get_mut("main").unwrap();
        let target = st
            .tree
            .tiles
            .iter()
            .find_map(|(id, t)| matches!(t, Tile::Pane(0)).then_some(*id))
            .unwrap();
        st.split_panel(target, LinearDir::Vertical);
        ws.active = "main".to_string();
        let store = LiveStore::from_registry(&ch);
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ws.ui(ui, &store, &ch, &reg, None);
            });
        });
    }

    #[test]
    fn every_pickable_type_has_a_pictogram() {
        // A non-fallback glyph for every user-choosable panel type keeps the
        // picker meaningful as new panel types are added.
        let reg = PanelRegistry::with_builtins();
        for t in reg.pickable_type_names() {
            assert_ne!(type_pictogram(t), icon::SQUARE, "no pictogram for `{t}`");
        }
    }

    #[test]
    fn split_panel_adds_undefined_pane_and_survives_round_trip() {
        let (ch, reg, mut ws) = build();
        let st = ws.screens.get_mut("main").unwrap();
        assert_eq!(pane_count(st), 2);
        // Split the tile holding pane index 0 into a vertical pair.
        let target = st
            .tree
            .tiles
            .iter()
            .find_map(|(id, t)| matches!(t, Tile::Pane(0)).then_some(*id))
            .unwrap();
        st.split_panel(target, LinearDir::Vertical);
        assert_eq!(pane_count(st), 3);
        assert_eq!(st.panels.len(), 3);
        // New pane starts undefined until the user picks a type.
        assert_eq!(st.panels[2].type_name, placeholder::TYPE_NAME);
        // Tree stays valid (panes are exactly {0,1,2}) so it round-trips.
        assert!(tree_panes_valid(&st.tree, st.panels.len()));
        let cfg = ws.to_config();
        let ws2 = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(pane_count(&ws2.screens["main"]), 3);
        // The undefined pane survives save/reload as a placeholder.
        assert_eq!(ws2.screens["main"].panels[2].type_name, placeholder::TYPE_NAME);
    }

    #[test]
    fn define_panel_replaces_undefined_pane_with_chosen_type() {
        let (ch, reg, mut ws) = build();
        let st = ws.screens.get_mut("main").unwrap();
        let target = st
            .tree
            .tiles
            .iter()
            .find_map(|(id, t)| matches!(t, Tile::Pane(0)).then_some(*id))
            .unwrap();
        st.split_panel(target, LinearDir::Vertical);
        assert_eq!(st.panels[2].type_name, placeholder::TYPE_NAME);
        st.define_panel(2, "gauge", &reg, &ch);
        assert_eq!(st.panels[2].type_name, "gauge");
    }

    #[test]
    fn refresh_bindings_resolves_dynamic_mqtt_channel() {
        use std::collections::HashMap;
        use std::sync::RwLock;

        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let store = LiveStore::from_registry(&ch);
        // Layout references an MQTT topic absent from channels.toml — a
        // drop-created dynamic channel that vanished on restart.
        let cfg = LayoutConfig::from_toml_str(
            r#"
[[screens.main.panels]]
type = "numeric"
channel = "home/sensors/temp"
"#,
        )
        .unwrap();
        let mut ws = Workspace::from_config(&cfg, &reg, &ch);
        assert!(ch.id("home/sensors/temp").is_none(), "unknown at load time");

        // No snapshot: still unresolved.
        ws.refresh_bindings(&ch, &store, None);
        assert!(ch.id("home/sensors/temp").is_none());

        // Broker republishes the topic → discovery re-registers it and the
        // panel binds.
        let topic_map: MqttTopicMap = RwLock::new(HashMap::new());
        let mut snap = BTreeMap::new();
        snap.insert("home/sensors/temp".to_string(), "21.5".to_string());
        ws.refresh_bindings(&ch, &store, Some((&topic_map, &snap)));

        assert!(ch.id("home/sensors/temp").is_some(), "resolved after discovery");
        assert!(topic_map.read().unwrap().contains_key("home/sensors/temp"));
    }

    #[test]
    fn freeze_time_zoom_reaches_panels() {
        let (_ch, _reg, mut ws) = build();
        // Iterates every panel across both screens; must not panic.
        ws.freeze_time_zoom((1_000, 2_000));
        // Re-freezing with a new range is also fine.
        ws.freeze_time_zoom((3_000, 4_000));
    }
}

