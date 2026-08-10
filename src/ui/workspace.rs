//! Workspace — the central pane grid area of the application.
//!
//! A [`Workspace`] owns one or more named *screens*.  Each screen is an
//! independent [`iced::widget::pane_grid::State`] tiling of [`VizPanel`]s.
//! Only the *active* screen is rendered at any one time.
//!
//! ## Layout persistence
//!
//! The canonical layout for each screen is kept as a [`LayoutNode`] tree (see
//! [`crate::config::workspace_layout`]).  The `pane_grid::State` is derived
//! from it.  On [`to_config`](Workspace::to_config) the canonical trees are
//! serialised into [`LayoutConfig::screens`]; on
//! [`from_config`](Workspace::from_config) they are deserialised and pane
//! states are rebuilt.

use std::collections::{BTreeMap, HashMap};

use iced::widget::pane_grid::{self, Configuration};
use iced::{Element, Length};

use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry, ScreenConfig};
use crate::config::workspace_layout::LayoutNode;
use crate::store::ChannelStore;
use crate::viz::{PanelRegistry, VizPanel};

use super::message::{Message, PaneId};

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert our canonical [`LayoutNode`] into the iced `Configuration<u32>`
/// needed by `pane_grid::State::with_configuration`.
///
/// `next_id` is a monotonic counter threaded through the recursion; each leaf
/// gets its own u32 id.  `panel_types` receives the `panel_type` string for
/// each allocated id so that round-tripping via `pane_node_to_layout` can
/// reconstruct the correct `PanelEntry.panel_type`.
fn node_to_configuration(
    node: &LayoutNode,
    panels: &mut HashMap<u32, Box<dyn VizPanel>>,
    panel_types: &mut HashMap<u32, String>,
    reg: &PanelRegistry,
    ch: &ChannelRegistry,
    next_id: &mut u32,
) -> Configuration<u32> {
    match node {
        LayoutNode::Split { axis, ratio, a, b } => {
            let ca = node_to_configuration(a, panels, panel_types, reg, ch, next_id);
            let cb = node_to_configuration(b, panels, panel_types, reg, ch, next_id);
            Configuration::Split {
                axis: (*axis).into(),
                ratio: *ratio,
                a: Box::new(ca),
                b: Box::new(cb),
            }
        }
        LayoutNode::Leaf(entry) => {
            let id = *next_id;
            *next_id += 1;
            let panel = reg.build(entry, ch);
            panels.insert(id, panel);
            panel_types.insert(id, entry.panel_type.clone());
            Configuration::Pane(id)
        }
    }
}

/// Reconstruct a [`LayoutNode`] from the live pane_grid [`pane_grid::Node`]
/// plus the panel map (to re-serialise panel configs).
/// `pane_map`: maps pane_grid::Pane -> our u32 panel id.
fn pane_node_to_layout(
    node: &pane_grid::Node,
    panels: &HashMap<u32, Box<dyn VizPanel>>,
    panel_types: &HashMap<u32, String>,
    pane_map: &HashMap<pane_grid::Pane, u32>,
) -> LayoutNode {
    match node {
        pane_grid::Node::Split { axis, ratio, a, b, .. } => {
            LayoutNode::Split {
                axis: (*axis).into(),
                ratio: *ratio,
                a: Box::new(pane_node_to_layout(a, panels, panel_types, pane_map)),
                b: Box::new(pane_node_to_layout(b, panels, panel_types, pane_map)),
            }
        }
        pane_grid::Node::Pane(pane) => {
            if let Some(id) = pane_map.get(pane) {
                if let Some(panel) = panels.get(id) {
                    let panel_type = panel_types
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| "error".to_string());
                    let config = panel.serialize();
                    return LayoutNode::Leaf(PanelEntry { panel_type, config });
                }
            }
            // Fallback — should not happen in a consistent state.
            LayoutNode::Leaf(PanelEntry {
                panel_type: "error".into(),
                config: toml::Table::new(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

/// One named screen in the workspace: an independent tiling of panels.
struct Screen {
    /// The canonical layout tree. This is the source of truth.
    layout: LayoutNode,
    /// The live pane_grid state, derived from `layout`.
    grid: pane_grid::State<u32>,
    /// Map from our `u32` panel id to the panel box.
    panels: HashMap<u32, Box<dyn VizPanel>>,
    /// Panel type names keyed by panel id, for accurate `PanelEntry` reconstruction.
    panel_types: HashMap<u32, String>,
    /// Mapping from pane_grid::Pane to our u32 id, kept in sync with `grid`.
    pane_map: HashMap<pane_grid::Pane, u32>,
    /// Monotonic counter for allocating new panel ids.
    next_id: u32,
}

impl Screen {
    /// Build a screen from a [`LayoutNode`].
    fn from_layout(node: LayoutNode, reg: &PanelRegistry, ch: &ChannelRegistry) -> Self {
        let mut panels: HashMap<u32, Box<dyn VizPanel>> = HashMap::new();
        let mut panel_types: HashMap<u32, String> = HashMap::new();
        let mut next_id: u32 = 0;
        let config =
            node_to_configuration(&node, &mut panels, &mut panel_types, reg, ch, &mut next_id);
        let grid = pane_grid::State::with_configuration(config);
        // Build the reverse map: pane_grid::Pane -> u32.
        let pane_map: HashMap<pane_grid::Pane, u32> = grid
            .panes
            .iter()
            .map(|(pane, &id)| (*pane, id))
            .collect();
        Screen { layout: node, grid, panels, panel_types, pane_map, next_id }
    }

    /// Rebuild the grid and pane_map from the current canonical layout.
    fn rebuild_grid(&mut self, reg: &PanelRegistry, ch: &ChannelRegistry) {
        let mut panels: HashMap<u32, Box<dyn VizPanel>> = HashMap::new();
        let mut panel_types: HashMap<u32, String> = HashMap::new();
        let mut next_id: u32 = 0;
        let config = node_to_configuration(
            &self.layout,
            &mut panels,
            &mut panel_types,
            reg,
            ch,
            &mut next_id,
        );
        self.grid = pane_grid::State::with_configuration(config);
        self.panels = panels;
        self.panel_types = panel_types;
        self.pane_map = self.grid.panes.iter().map(|(p, &id)| (*p, id)).collect();
        self.next_id = next_id;
    }

    /// Re-serialise the canonical layout from the live pane_grid state.
    /// Call after split/close events to keep the canonical tree in sync.
    fn sync_layout_from_grid(&mut self) {
        let pane_map = &self.pane_map;
        let panels = &self.panels;
        let panel_types = &self.panel_types;
        self.layout = pane_node_to_layout(self.grid.layout(), panels, panel_types, pane_map);
    }

    /// Add a new panel to this screen, splitting the first available pane.
    fn add_panel(
        &mut self,
        entry: PanelEntry,
        reg: &PanelRegistry,
        ch: &ChannelRegistry,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let panel = reg.build(&entry, ch);
        self.panel_types.insert(id, entry.panel_type.clone());
        self.panels.insert(id, panel);

        // Find the first pane to split (or rebuild with a new leaf in layout).
        if let Some((&first_pane, _)) = self.grid.panes.iter().next() {
            if let Some((new_pane, _)) =
                self.grid.split(pane_grid::Axis::Vertical, first_pane, id)
            {
                self.pane_map.insert(new_pane, id);
            }
        } else {
            // Empty grid — start fresh with this panel as the only leaf.
            self.layout = LayoutNode::Leaf(entry);
            self.rebuild_grid(reg, ch);
            return;
        }
        // Rebuild canonical layout from current grid state.
        self.sync_layout_from_grid();
    }

    /// Close a pane identified by our `u32` panel id.
    fn close_pane_by_id(&mut self, target_id: u32) {
        // Find the pane_grid::Pane for this id.
        let pane = self
            .pane_map
            .iter()
            .find_map(|(&pane, &id)| if id == target_id { Some(pane) } else { None });

        if let Some(pane) = pane {
            if let Some((_, _)) = self.grid.close(pane) {
                self.panels.remove(&target_id);
                self.panel_types.remove(&target_id);
                self.pane_map.remove(&pane);
                self.sync_layout_from_grid();
            }
        }
    }

    /// Sync all panels from the store.
    fn sync_all(&mut self, store: &dyn ChannelStore) {
        for panel in self.panels.values_mut() {
            panel.sync(store);
        }
    }

    /// Render the pane_grid for this screen.
    fn view(&self) -> Element<'_, Message> {
        use iced::widget::{button, pane_grid, text};

        let grid_widget = pane_grid::PaneGrid::new(&self.grid, |_pane, &id, _is_maximized| {
            let title: String = self
                .panels
                .get(&id)
                .map(|p| p.title())
                .unwrap_or_else(|| "Panel".to_string());

            let close_btn: Element<'_, Message> = button(text("x").size(11))
                .on_press(Message::ClosePane(PaneId(id)))
                .padding(2)
                .into();

            let title_bar = pane_grid::TitleBar::new(text(title))
                .controls(close_btn)
                .padding(4);

            let body: Element<'_, Message> = if let Some(panel) = self.panels.get(&id) {
                panel
                    .view()
                    .map(move |m| Message::Panel(PaneId(id), m))
            } else {
                text("(empty)").into()
            };

            pane_grid::Content::new(body).title_bar(title_bar)
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .spacing(4)
        .on_resize(8, Message::PaneResized);

        grid_widget.into()
    }
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

/// The central pane grid workspace.  Holds one or more named screens.
pub struct Workspace {
    /// Named screens; kept sorted via BTreeMap for stable ordering.
    screens: BTreeMap<String, Screen>,
    /// Name of the currently-visible screen.
    active: String,
}

impl Workspace {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Build a workspace from a persisted [`LayoutConfig`].
    ///
    /// For each screen in `cfg`:
    /// - If [`ScreenConfig::layout`] is present and parses as a [`LayoutNode`],
    ///   that tree is used.
    /// - If the layout field is absent or contains a legacy egui_tiles JSON
    ///   blob (or any other unparseable content), a default linear layout is
    ///   built from the screen's panel list.
    pub fn from_config(
        cfg: &LayoutConfig,
        reg: &PanelRegistry,
        ch: &ChannelRegistry,
    ) -> Self {
        let mut screens: BTreeMap<String, Screen> = BTreeMap::new();

        for (name, screen_cfg) in &cfg.screens {
            let node = Self::node_from_screen_config(screen_cfg);
            screens.insert(name.clone(), Screen::from_layout(node, reg, ch));
        }

        // If no screens were defined, create a default empty one.
        if screens.is_empty() {
            let default_node = LayoutNode::from_panels(&[]);
            screens.insert("main".to_string(), Screen::from_layout(default_node, reg, ch));
        }

        let active = screens.keys().next().unwrap().clone();
        Workspace { screens, active }
    }

    /// Extract a [`LayoutNode`] from a [`ScreenConfig`], falling back to a
    /// default grid on any failure.
    fn node_from_screen_config(screen_cfg: &ScreenConfig) -> LayoutNode {
        // Try the new `layout` field first.
        if let Some(layout_node) = &screen_cfg.layout {
            return layout_node.clone();
        }

        // tiles_json present → legacy egui_tiles blob; build default grid.
        if screen_cfg.tiles_json.is_some() {
            eprintln!(
                "workspace: ignoring legacy egui_tiles blob; \
                 building default grid from panel list"
            );
        }

        // Fall back to a flat layout derived from the panel list.
        LayoutNode::from_panels(&screen_cfg.panels)
    }

    // -----------------------------------------------------------------------
    // Serialisation
    // -----------------------------------------------------------------------

    /// Serialise the current workspace state into a [`LayoutConfig`].
    pub fn to_config(&self) -> LayoutConfig {
        let mut cfg = LayoutConfig::default();
        for (name, screen) in &self.screens {
            let panels: Vec<PanelEntry> = screen
                .layout
                .leaves()
                .into_iter()
                .cloned()
                .collect();
            cfg.screens.insert(name.clone(), ScreenConfig {
                tiles_json: None,
                panels,
                layout: Some(screen.layout.clone()),
            });
        }
        cfg
    }

    // -----------------------------------------------------------------------
    // Mutations
    // -----------------------------------------------------------------------

    /// Add a panel to the active screen.
    pub fn add_panel(
        &mut self,
        entry: PanelEntry,
        reg: &PanelRegistry,
        ch: &ChannelRegistry,
    ) {
        if let Some(screen) = self.screens.get_mut(&self.active) {
            screen.add_panel(entry, reg, ch);
        }
    }

    /// Split a pane in the active screen along the given axis.
    pub fn split(
        &mut self,
        axis: pane_grid::Axis,
        pane_id: PaneId,
        entry: PanelEntry,
        reg: &PanelRegistry,
        ch: &ChannelRegistry,
    ) {
        let Some(screen) = self.screens.get_mut(&self.active) else { return };

        // Find the pane_grid::Pane for this PaneId.
        let pane = screen
            .pane_map
            .iter()
            .find_map(|(&pane, &id)| if id == pane_id.0 { Some(pane) } else { None });

        let Some(pane) = pane else { return };

        let new_id = screen.next_id;
        screen.next_id += 1;
        let panel = reg.build(&entry, ch);
        screen.panel_types.insert(new_id, entry.panel_type.clone());
        screen.panels.insert(new_id, panel);

        if let Some((new_pane, _)) = screen.grid.split(axis, pane, new_id) {
            screen.pane_map.insert(new_pane, new_id);
            screen.sync_layout_from_grid();
        }
    }

    /// Close a pane by its [`PaneId`].
    pub fn close_pane(&mut self, id: PaneId) {
        if let Some(screen) = self.screens.get_mut(&self.active) {
            screen.close_pane_by_id(id.0);
        }
    }

    /// Add a new empty screen with the given name (or an auto-generated name).
    pub fn add_screen(&mut self, name: Option<String>, reg: &PanelRegistry, ch: &ChannelRegistry) {
        let name = name.unwrap_or_else(|| {
            let n = self.screens.len();
            format!("screen_{n}")
        });
        let node = LayoutNode::from_panels(&[]);
        self.screens.entry(name.clone()).or_insert_with(|| Screen::from_layout(node, reg, ch));
        self.active = name;
    }

    /// Switch to the screen with the given name.
    pub fn set_active(&mut self, name: &str) {
        if self.screens.contains_key(name) {
            self.active = name.to_string();
        }
    }

    /// Forward a resize event from the pane_grid widget.
    pub fn on_resize(&mut self, event: pane_grid::ResizeEvent) {
        if let Some(screen) = self.screens.get_mut(&self.active) {
            screen.grid.resize(event.split, event.ratio);
            // Note: we intentionally do NOT sync the canonical layout here
            // to avoid the overhead; ratios from live drag-resize are therefore
            // not persisted across save/load in this task (noted as a concern).
        }
    }

    // -----------------------------------------------------------------------
    // Data sync
    // -----------------------------------------------------------------------

    /// Sync all panels in all screens from the store.
    ///
    /// Called every tick.  We sync all screens (not just the active one) so
    /// that panels in background screens stay current when you switch to them.
    pub fn sync_all(&mut self, store: &dyn ChannelStore) {
        for screen in self.screens.values_mut() {
            screen.sync_all(store);
        }
    }

    // -----------------------------------------------------------------------
    // View
    // -----------------------------------------------------------------------

    /// Render the active screen's pane grid.
    ///
    /// `active_only` is reserved for a future optimisation (render only the
    /// visible screen) — currently always true by construction.
    pub fn view(&self, _active_only: bool) -> Element<'_, Message> {
        if let Some(screen) = self.screens.get(&self.active) {
            screen.view()
        } else {
            iced::widget::text("No screen").into()
        }
    }

    /// Returns the name of the currently active screen.
    pub fn active_screen(&self) -> &str {
        &self.active
    }

    /// Returns the names of all screens.
    pub fn screen_names(&self) -> impl Iterator<Item = &str> {
        self.screens.keys().map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::viz::PanelRegistry;

    fn test_reg_ch() -> (PanelRegistry, ChannelRegistry) {
        (
            PanelRegistry::with_builtins(),
            ChannelRegistry::from_toml_str("").unwrap(),
        )
    }

    #[test]
    fn legacy_or_garbage_workspace_builds_default_grid() {
        // Simulate an old egui_tiles config with a JSON blob in tiles_json.
        let toml_src = r#"
[screens.main]
tiles_json = '{"LinearDir":"Horizontal","children":[{"Leaf":{"id":1}},{"Leaf":{"id":2}}]}'

[[screens.main.panels]]
type = "numeric"

[[screens.main.panels]]
type = "gauge"
"#;
        let cfg = LayoutConfig::from_toml_str(toml_src).unwrap();
        let (reg, ch) = test_reg_ch();

        // Must not panic and must yield a non-empty workspace.
        let ws = Workspace::from_config(&cfg, &reg, &ch);

        // At least one screen.
        assert!(ws.screens.len() >= 1);
        // The active screen must have at least one pane.
        let active = ws.screens.get(&ws.active).unwrap();
        assert!(!active.panels.is_empty(), "workspace must have at least one panel");
    }

    #[test]
    fn default_workspace_has_one_screen() {
        let cfg = LayoutConfig::default();
        let (reg, ch) = test_reg_ch();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(ws.screens.len(), 1, "default workspace has one screen");
    }

    #[test]
    fn add_screen_creates_new_screen() {
        let cfg = LayoutConfig::default();
        let (reg, ch) = test_reg_ch();
        let mut ws = Workspace::from_config(&cfg, &reg, &ch);
        ws.add_screen(Some("second".into()), &reg, &ch);
        assert!(ws.screens.contains_key("second"));
        assert_eq!(ws.active, "second");
    }
}
