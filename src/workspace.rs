use std::collections::BTreeMap;

use anyhow::anyhow;
use eframe::egui;

use crate::canvas::{self, auto_arrange, CanvasEvent, NormRect, PanelPlacement};
use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry, ScreenConfig};
use crate::dynamic_channel::MqttTopicMap;
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

/// One screen: panels plus their canvas placements (index-aligned).
pub struct ScreenState {
    pub panels: Vec<PanelSlot>,
    pub layout: Vec<PanelPlacement>,
    /// Panel index whose settings window is currently open, if any.
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

impl ScreenState {
    fn empty(_name: &str) -> Self {
        Self { panels: Vec::new(), layout: Vec::new(), settings_open: None }
    }

    /// Seed a single undefined pane on an empty screen so the type-picker shows
    /// instead of a blank canvas.
    fn ensure_undefined_pane(&mut self) {
        if !self.panels.is_empty() {
            return;
        }
        self.panels.push(PanelSlot {
            type_name: placeholder::TYPE_NAME.to_string(),
            panel: Box::new(placeholder::PlaceholderPanel),
        });
        self.layout.push(PanelPlacement {
            rect: NormRect { fx: 0.15, fy: 0.15, fw: 0.7, fh: 0.7 },
        });
    }

    fn remove_panel(&mut self, idx: usize) {
        if idx >= self.panels.len() {
            return;
        }
        self.panels.remove(idx);
        self.layout.remove(idx);
        self.settings_open = None;
    }

    /// Add an undefined panel at `rect` (from a canvas right-click).
    /// If `rect` overlaps an existing placement the requested position is
    /// discarded and we fall back to the same free-slot / carve logic used by
    /// `add_panel`, preserving the "panels never overlap" invariant.
    fn add_undefined_at(&mut self, rect: NormRect) {
        let rect = if self.layout.iter().any(|p| rects_overlap(rect, p.rect)) {
            free_slot(&self.layout).unwrap_or_else(|| carve_slot(&mut self.layout))
        } else {
            rect
        };
        self.panels.push(PanelSlot {
            type_name: placeholder::TYPE_NAME.to_string(),
            panel: Box::new(placeholder::PlaceholderPanel),
        });
        self.layout.push(PanelPlacement { rect });
    }

    /// Replace the pane at `idx` with a fresh, unconfigured panel of `type_name`.
    fn define_panel(
        &mut self,
        idx: usize,
        type_name: &str,
        reg: &PanelRegistry,
        channels: &ChannelRegistry,
    ) {
        let Some(slot) = self.panels.get_mut(idx) else { return };
        let entry = PanelEntry {
            panel_type: type_name.to_string(),
            frame: None,
            config: toml::Table::new(),
        };
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
        _name: &str,
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

        // Use saved frames only if every panel has one; otherwise auto-arrange.
        let layout = if !sc.panels.is_empty() && sc.panels.iter().all(|e| e.frame.is_some()) {
            sc.panels
                .iter()
                .map(|e| PanelPlacement { rect: NormRect::from_array(e.frame.unwrap()) })
                .collect()
        } else {
            auto_arrange(sc.panels.len())
                .into_iter()
                .map(|rect| PanelPlacement { rect })
                .collect()
        };

        Self { panels, layout, settings_open: None }
    }
}

fn rects_overlap(a: NormRect, b: NormRect) -> bool {
    a.fx < b.fx + b.fw - 1e-4
        && b.fx < a.fx + a.fw - 1e-4
        && a.fy < b.fy + b.fh - 1e-4
        && b.fy < a.fy + a.fh - 1e-4
}

/// Find a default-sized rect that does not overlap any existing placement, by
/// scanning a coarse grid of candidate positions. Returns `None` when the
/// canvas is fully tiled and no gap exists (the caller then carves space).
fn free_slot(existing: &[PanelPlacement]) -> Option<NormRect> {
    const W: f32 = 0.3;
    const H: f32 = 0.3;
    let overlaps = |c: NormRect| existing.iter().any(|p| rects_overlap(c, p.rect));
    let steps = [0.0, 0.15, 0.3, 0.45, 0.6, 0.7];
    for fy in steps {
        for fx in steps {
            let c = NormRect { fx, fy, fw: W, fh: H };
            if !overlaps(c) {
                return Some(c);
            }
        }
    }
    None
}

/// Open a non-overlapping slot for one more panel. Prefers an empty gap; if the
/// canvas is fully tiled, halves the largest existing placement along its longer
/// axis and returns the freed half (the shrunk original stays put), guaranteeing
/// the new placement never overlaps.
fn carve_slot(existing: &mut [PanelPlacement]) -> NormRect {
    if let Some(r) = free_slot(existing) {
        return r;
    }
    // Halve the largest-area placement.
    let victim = existing
        .iter_mut()
        .max_by(|a, b| {
            let aa = a.rect.fw * a.rect.fh;
            let ba = b.rect.fw * b.rect.fh;
            aa.partial_cmp(&ba).unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("carve_slot requires at least one existing placement");
    let r = victim.rect;
    if r.fw >= r.fh {
        let half = r.fw * 0.5;
        victim.rect = NormRect { fw: r.fw - half, ..r };
        NormRect { fx: r.fx + (r.fw - half), fy: r.fy, fw: half, fh: r.fh }
    } else {
        let half = r.fh * 0.5;
        victim.rect = NormRect { fh: r.fh - half, ..r };
        NormRect { fx: r.fx, fy: r.fy + (r.fh - half), fw: r.fw, fh: half }
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
            let entries: Vec<PanelEntry> = st
                .panels
                .iter()
                .enumerate()
                .map(|(i, slot)| PanelEntry {
                    panel_type: slot.type_name.clone(),
                    frame: Some(st.layout[i].rect.to_array()),
                    config: slot.panel.serialize(),
                })
                .collect();
            cfg.screens.insert(
                name.clone(),
                ScreenConfig { panels: entries },
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
        st.ensure_undefined_pane();

        let events = canvas::show(
            ui,
            &mut st.panels,
            &mut st.layout,
            store,
            channels,
            mqtt,
            &type_names,
        );
        for ev in events {
            match ev {
                CanvasEvent::Define(idx, t) => st.define_panel(idx, &t, reg, channels),
                CanvasEvent::Settings(idx) => st.settings_open = Some(idx),
                CanvasEvent::ResetZoom(idx) => {
                    if let Some(slot) = st.panels.get_mut(idx) {
                        slot.panel.reset_zoom();
                    }
                }
                CanvasEvent::Delete(idx) => st.remove_panel(idx),
                CanvasEvent::AddAt(rect) => st.add_undefined_at(rect),
            }
        }

        // Floating settings window (unchanged behavior).
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

    /// Remove a screen. Refuses to drop the last screen; if the active screen
    /// is removed, activates the first remaining one.
    pub fn remove_screen(&mut self, name: &str) {
        if self.screens.len() <= 1 {
            return;
        }
        self.screens.remove(name);
        if self.active == name {
            self.active = self.screens.keys().next().cloned().unwrap_or_default();
        }
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
        st.panels.push(PanelSlot { type_name: entry.panel_type.clone(), panel });
        let rect = match entry.frame {
            // Explicit frame is trusted: comes from config-load or programmatic
            // callers (same assumption as `from_screen_config`).
            Some(f) => NormRect::from_array(f),
            None => carve_slot(&mut st.layout),
        };
        st.layout.push(PanelPlacement { rect });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::NormRect;
    use crate::config::{ChannelRegistry, LayoutConfig, PanelEntry};
    use crate::store::{ChannelStore, LiveStore};
    use crate::types::NumericVal;
    use crate::viz::{placeholder, PanelRegistry};
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
frame = [0.0, 0.0, 0.5, 1.0]
channel = "demo.sine"

[[screens.main.panels]]
type = "numeric"
frame = [0.5, 0.0, 0.5, 1.0]
channel = "demo.counter"
"#;

    fn build() -> (ChannelRegistry, PanelRegistry, Workspace) {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let cfg = LayoutConfig::from_toml_str(LAYOUT).unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        (ch, reg, ws)
    }

    #[test]
    fn from_config_reads_frames() {
        let (_, _, ws) = build();
        let st = &ws.screens["main"];
        assert_eq!(st.panels.len(), 2);
        assert_eq!(st.layout.len(), 2);
        assert_eq!(st.layout[0].rect, NormRect { fx: 0.0, fy: 0.0, fw: 0.5, fh: 1.0 });
        assert_eq!(st.layout[1].rect.fx, 0.5);
    }

    #[test]
    fn round_trip_preserves_panels_and_frames() {
        let (ch, reg, ws) = build();
        let cfg = ws.to_config();
        let main = &cfg.screens["main"];
        assert_eq!(main.panels.len(), 2);
        assert_eq!(main.panels[0].panel_type, "numeric");
        assert_eq!(main.panels[0].frame, Some([0.0, 0.0, 0.5, 1.0]));
        // Reload keeps geometry.
        let ws2 = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(ws2.screens["main"].layout[0].rect.fw, 0.5);
    }

    #[test]
    fn missing_frames_auto_arrange() {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let cfg = LayoutConfig::from_toml_str(
            r#"
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
[[screens.main.panels]]
type = "numeric"
channel = "demo.counter"
"#,
        )
        .unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        let st = &ws.screens["main"];
        assert_eq!(st.layout.len(), 2);
        // Two panels auto-arrange into a 2-column row (each width 0.5).
        assert!((st.layout[0].rect.fw - 0.5).abs() < 1e-4);
        assert!(st.layout[0].rect.fx != st.layout[1].rect.fx);
    }

    #[test]
    fn stale_tiles_json_loads_without_error() {
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        // Old config with tiles_json and no frames must not error.
        let cfg = LayoutConfig::from_toml_str(
            r#"
[screens.main]
tiles_json = '{"anything":1}'
[[screens.main.panels]]
type = "numeric"
channel = "demo.sine"
"#,
        )
        .unwrap();
        let ws = Workspace::from_config(&cfg, &reg, &ch);
        assert_eq!(ws.screens["main"].panels.len(), 1);
    }

    #[test]
    fn add_panel_appends_non_overlapping_placement() {
        let (ch, reg, mut ws) = build();
        let entry = PanelEntry {
            panel_type: "numeric".into(),
            frame: None,
            config: toml::from_str(r#"channel = "demo.sine""#).unwrap(),
        };
        ws.add_panel(&entry, &reg, &ch).unwrap();
        let st = &ws.screens["main"];
        assert_eq!(st.panels.len(), 3);
        assert_eq!(st.layout.len(), 3);
        // New placement does not overlap any existing one.
        let new = st.layout[2].rect;
        for i in 0..2 {
            let o = st.layout[i].rect;
            let overlap = new.fx < o.fx + o.fw - 1e-4
                && o.fx < new.fx + new.fw - 1e-4
                && new.fy < o.fy + o.fh - 1e-4
                && o.fy < new.fy + new.fh - 1e-4;
            assert!(!overlap);
        }
    }

    #[test]
    fn remove_panel_drops_from_both_vecs() {
        let (_ch, _reg, mut ws) = build();
        let st = ws.screens.get_mut("main").unwrap();
        st.remove_panel(0);
        assert_eq!(st.panels.len(), 1);
        assert_eq!(st.layout.len(), 1);
        assert!(st.settings_open.is_none());
    }

    #[test]
    fn empty_screen_seeds_undefined_panel() {
        let mut st = ScreenState::empty("blank");
        st.ensure_undefined_pane();
        assert_eq!(st.panels.len(), 1);
        assert_eq!(st.layout.len(), 1);
        assert_eq!(st.panels[0].type_name, placeholder::TYPE_NAME);
    }

    #[test]
    fn define_panel_replaces_type() {
        let (ch, reg, _ws) = build();
        let mut st = ScreenState::empty("blank");
        st.ensure_undefined_pane();
        st.define_panel(0, "gauge", &reg, &ch);
        assert_eq!(st.panels[0].type_name, "gauge");
    }

    /// A panel's title-bar gear button must open its settings window. Regression
    /// guard for a full-panel drop-interaction rect swallowing the title-bar
    /// buttons' clicks (they never fired). Injects a real pointer click at the
    /// gear and asserts `settings_open` is set.
    #[test]
    fn title_bar_gear_click_opens_settings() {
        use egui::{pos2, vec2, Event, PointerButton, Rect};
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        // One numeric panel filling the whole canvas.
        let cfg = LayoutConfig::from_toml_str(
            "[[screens.main.panels]]\ntype = \"numeric\"\nframe = [0.0,0.0,1.0,1.0]\nchannel = \"demo.sine\"\n",
        )
        .unwrap();
        let store = LiveStore::from_registry(&ch);
        let mut ws = Workspace::from_config(&cfg, &reg, &ch);
        ws.active = "main".to_string();

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let run = |ws: &mut Workspace, events: Vec<Event>| {
            let input = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ws.ui(ui, &store, &ch, &reg, None);
                });
            });
        };
        // Warm-up frame establishes widget geometry for hit-testing.
        run(&mut ws, vec![]);

        // The gear sits at the right end of the 22px title bar. Scan that strip
        // for a click that opens settings (exact glyph metrics vary by build).
        let mut opened = false;
        'scan: for gx in (600..796).step_by(2) {
            for gy in (4..20).step_by(2) {
                let p = pos2(gx as f32, gy as f32);
                run(&mut ws, vec![
                    Event::PointerMoved(p),
                    Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers: Default::default() },
                ]);
                run(&mut ws, vec![
                    Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: false, modifiers: Default::default() },
                ]);
                if ws.screens["main"].settings_open == Some(0) {
                    opened = true;
                    break 'scan;
                }
            }
        }
        assert!(opened, "clicking the title-bar gear never opened panel settings");
    }

    /// Holding Ctrl/⌘ and dragging anywhere in a panel (here: its body, away
    /// from the title bar) must move the panel.
    #[test]
    fn ctrl_drag_body_moves_panel() {
        use egui::{pos2, vec2, Event, Modifiers, PointerButton, Rect};
        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        // One small panel with room to move on the canvas.
        let cfg = LayoutConfig::from_toml_str(
            "[[screens.main.panels]]\ntype = \"numeric\"\nframe = [0.1,0.1,0.3,0.3]\nchannel = \"demo.sine\"\n",
        )
        .unwrap();
        let store = LiveStore::from_registry(&ch);
        let mut ws = Workspace::from_config(&cfg, &reg, &ch);
        ws.active = "main".to_string();

        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let cmd = Modifiers { command: true, ctrl: true, ..Default::default() };
        let run = |ws: &mut Workspace, events: Vec<Event>| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                modifiers: cmd,
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ws.ui(ui, &store, &ch, &reg, None);
                });
            });
        };
        run(&mut ws, vec![]); // warm-up geometry

        let start_fx = ws.screens["main"].layout[0].rect.fx;
        // Press in the body (well below the 22px title bar), then drag right.
        let p0 = pos2(200.0, 160.0);
        run(&mut ws, vec![
            Event::PointerMoved(p0),
            Event::PointerButton { pos: p0, button: PointerButton::Primary, pressed: true, modifiers: cmd },
        ]);
        let p1 = pos2(300.0, 160.0);
        run(&mut ws, vec![Event::PointerMoved(p1)]);
        run(&mut ws, vec![Event::PointerMoved(p1)]);
        let moved_fx = ws.screens["main"].layout[0].rect.fx;
        run(&mut ws, vec![
            Event::PointerButton { pos: p1, button: PointerButton::Primary, pressed: false, modifiers: cmd },
        ]);

        assert!(
            moved_fx > start_fx + 0.05,
            "ctrl+drag in body should move panel right: start={start_fx} moved={moved_fx}"
        );
    }

    #[test]
    fn ui_renders_headless_without_panic() {
        let (ch, reg, mut ws) = build();
        let store = LiveStore::from_registry(&ch);
        store.write_numeric(ch.id("demo.sine").unwrap(), 1, NumericVal::Float(1.0));
        ws.active = "main".to_string();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ws.ui(ui, &store, &ch, &reg, None);
            });
        });
    }

    #[test]
    fn add_undefined_at_over_existing_panel_does_not_overlap() {
        // Start from a screen with two side-by-side panels that together fill
        // the canvas (left half and right half).
        let mut st = ScreenState::empty("blank");
        st.panels.push(PanelSlot {
            type_name: placeholder::TYPE_NAME.to_string(),
            panel: Box::new(placeholder::PlaceholderPanel),
        });
        st.layout.push(PanelPlacement { rect: NormRect { fx: 0.0, fy: 0.0, fw: 0.5, fh: 1.0 } });
        st.panels.push(PanelSlot {
            type_name: placeholder::TYPE_NAME.to_string(),
            panel: Box::new(placeholder::PlaceholderPanel),
        });
        st.layout.push(PanelPlacement { rect: NormRect { fx: 0.5, fy: 0.0, fw: 0.5, fh: 1.0 } });

        // Call add_undefined_at with a rect that overlaps the first panel.
        let overlapping = NormRect { fx: 0.0, fy: 0.0, fw: 0.5, fh: 1.0 };
        st.add_undefined_at(overlapping);

        assert_eq!(st.panels.len(), 3);
        assert_eq!(st.layout.len(), 3);
        let new = st.layout[2].rect;
        for i in 0..2 {
            let o = st.layout[i].rect;
            assert!(
                !rects_overlap(new, o),
                "new panel at {:?} overlaps existing {:?}",
                new,
                o
            );
        }
    }

    #[test]
    fn refresh_bindings_resolves_dynamic_mqtt_channel() {
        use std::collections::{BTreeMap, HashMap};
        use std::sync::RwLock;

        let ch = channels();
        let reg = PanelRegistry::with_builtins();
        let store = LiveStore::from_registry(&ch);
        let cfg = LayoutConfig::from_toml_str(
            r#"
[[screens.main.panels]]
type = "numeric"
channel = "home/sensors/temp"
"#,
        )
        .unwrap();
        let mut ws = Workspace::from_config(&cfg, &reg, &ch);
        assert!(ch.id("home/sensors/temp").is_none());

        let topic_map: MqttTopicMap = RwLock::new(HashMap::new());
        let mut snap = BTreeMap::new();
        snap.insert("home/sensors/temp".to_string(), "21.5".to_string());
        ws.refresh_bindings(&ch, &store, Some((&topic_map, &snap)));
        assert!(ch.id("home/sensors/temp").is_some());
    }
}

