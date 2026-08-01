use std::collections::{BTreeMap, HashSet};

use eframe::egui;

use crate::config::ChannelRegistry;

#[derive(Clone)]
enum Node {
    Group { label: String, children: Vec<Node> },
    Leaf  { label: String, full_name: String, value: Option<String> },
}

/// Channel name tree, split on '/' separators.
///
/// "sensor/imu/accel_x" → Group("sensor") → Group("imu") → Leaf("accel_x")
/// "demo.sine"          → Leaf("demo.sine")  (no '/' → flat leaf)
///
/// Holds the registry channel names; the node hierarchy (and each leaf's live
/// value) is assembled at render time, so ZMQ and MQTT channels share one
/// drag-only tree.
///
/// Leaves are multi-selectable: plain click selects one, Ctrl/Cmd+click toggles
/// membership, Shift+click selects a contiguous range in the flattened leaf
/// order. Dragging any selected leaf carries the whole selection as one payload;
/// dragging an unselected leaf carries just that one.
#[derive(Clone)]
pub struct ChannelTree {
    names: Vec<String>,
    /// Full names of currently selected leaves.
    selected: HashSet<String>,
    /// Anchor leaf for Shift-range selection (last plain/toggle click target).
    anchor: Option<String>,
}

/// Flatten assembled nodes into the depth-first leaf order the tree renders in.
/// Range selection and drag-payload ordering both key off this sequence.
fn flat_leaf_order(nodes: &[Node]) -> Vec<String> {
    fn walk(nodes: &[Node], out: &mut Vec<String>) {
        for n in nodes {
            match n {
                Node::Group { children, .. } => walk(children, out),
                Node::Leaf { full_name, .. } => out.push(full_name.clone()),
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, &mut out);
    out
}

/// Inclusive slice of `order` between leaves `a` and `b` (either click order).
/// Empty when either endpoint is absent.
fn range_between(order: &[String], a: &str, b: &str) -> Vec<String> {
    match (
        order.iter().position(|n| n == a),
        order.iter().position(|n| n == b),
    ) {
        (Some(i), Some(j)) => {
            let (lo, hi) = if i <= j { (i, j) } else { (j, i) };
            order[lo..=hi].to_vec()
        }
        _ => Vec::new(),
    }
}

fn insert_path(nodes: &mut Vec<Node>, parts: &[&str], full_name: &str, value: Option<String>) {
    if parts.len() == 1 {
        nodes.push(Node::Leaf {
            label: parts[0].to_string(),
            full_name: full_name.to_string(),
            value,
        });
        return;
    }
    let group_label = parts[0];
    if let Some(pos) = nodes
        .iter()
        .position(|n| matches!(n, Node::Group { label, .. } if label == group_label))
    {
        if let Node::Group { children, .. } = &mut nodes[pos] {
            insert_path(children, &parts[1..], full_name, value);
        }
    } else {
        let mut children = Vec::new();
        insert_path(&mut children, &parts[1..], full_name, value);
        nodes.push(Node::Group { label: group_label.to_string(), children });
    }
}

impl ChannelTree {
    pub fn build(registry: &ChannelRegistry) -> Self {
        let names = registry
            .iter_ids()
            .map(|id| registry.meta(id).name.clone())
            .collect();
        Self { names, selected: HashSet::new(), anchor: None }
    }

    /// Refresh the channel name list from the registry, keeping the current
    /// selection and anchor. The registry only grows (dynamic channels are
    /// appended), so a length change is a reliable "something new" trigger —
    /// this is how script outputs and dropped MQTT topics appear in the tree
    /// after `build` snapshotted the original names.
    pub fn sync(&mut self, registry: &ChannelRegistry) {
        if registry.len() == self.names.len() {
            return;
        }
        self.names = registry
            .iter_ids()
            .map(|id| registry.meta(id).name.clone())
            .collect();
    }

    /// Update the selection for a click on leaf `name`. `toggle` is the
    /// Ctrl/Cmd modifier, `range` is Shift. `order` is the current flattened
    /// leaf order (from [`flat_leaf_order`]).
    ///
    /// - Shift with a valid anchor selects the inclusive range; combined with
    ///   Ctrl it unions the range into the existing selection.
    /// - Ctrl toggles the single leaf and moves the anchor to it.
    /// - Plain click selects only that leaf and sets it as the anchor.
    pub(crate) fn apply_click(&mut self, name: &str, toggle: bool, range: bool, order: &[String]) {
        if range {
            if let Some(anchor) = self.anchor.clone() {
                let r = range_between(order, &anchor, name);
                if !r.is_empty() {
                    if !toggle {
                        self.selected.clear();
                    }
                    self.selected.extend(r);
                    // Keep the anchor so successive Shift-clicks grow from it.
                    return;
                }
            }
            // No usable anchor → fall through to plain/toggle handling.
        }
        if toggle {
            if !self.selected.remove(name) {
                self.selected.insert(name.to_string());
            }
        } else {
            self.selected.clear();
            self.selected.insert(name.to_string());
        }
        self.anchor = Some(name.to_string());
    }

    /// The drag payload for grabbing leaf `name`: every selected leaf (in tree
    /// order) when `name` is part of the selection, otherwise just `name`.
    pub(crate) fn drag_payload(&self, name: &str, order: &[String]) -> Vec<String> {
        if self.selected.contains(name) {
            let mut out: Vec<String> = order
                .iter()
                .filter(|n| self.selected.contains(*n))
                .cloned()
                .collect();
            if out.is_empty() {
                out.push(name.to_string());
            }
            out
        } else {
            vec![name.to_string()]
        }
    }

    /// Assemble the node hierarchy for rendering: every registry channel (value
    /// looked up via `value_of`) plus any `extra` topics not already in the
    /// registry (e.g. discovered-but-undropped MQTT topics, value from `extra`).
    fn assemble(
        &self,
        extra: &BTreeMap<String, String>,
        value_of: &impl Fn(&str) -> Option<String>,
    ) -> Vec<Node> {
        let mut roots: Vec<Node> = Vec::new();
        for name in &self.names {
            let parts: Vec<&str> = name.split('/').collect();
            insert_path(&mut roots, &parts, name, value_of(name));
        }
        for (topic, value) in extra {
            if self.names.iter().any(|n| n == topic) {
                continue;
            }
            let parts: Vec<&str> = topic.split('/').collect();
            insert_path(&mut roots, &parts, topic, Some(value.clone()));
        }
        roots
    }

    /// Render one drag-only "/" tree. Every leaf is a selectable drag source and
    /// shows its live value dimmed on the right. `extra` are discovered MQTT
    /// topics not yet in the registry; the workspace resolves which drops can
    /// actually bind.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        extra: &BTreeMap<String, String>,
        value_of: impl Fn(&str) -> Option<String>,
    ) {
        let roots = self.assemble(extra, &value_of);
        let order = flat_leaf_order(&roots);
        // Drop selections that no longer exist (channels removed / renamed).
        self.selected.retain(|n| order.iter().any(|o| o == n));
        // Collect the click so selection is mutated after the immutable render
        // walk finishes: (leaf name, toggle modifier, range modifier).
        let mut click: Option<(String, bool, bool)> = None;
        for node in &roots {
            self.render_node(ui, node, &order, &mut click);
        }
        if let Some((name, toggle, range)) = click {
            self.apply_click(&name, toggle, range, &order);
        }
    }

    fn render_node(
        &self,
        ui: &mut egui::Ui,
        node: &Node,
        order: &[String],
        click: &mut Option<(String, bool, bool)>,
    ) {
        match node {
            Node::Group { label, children } => {
                egui::CollapsingHeader::new(label)
                    .default_open(true)
                    .show(ui, |ui| {
                        for child in children {
                            self.render_node(ui, child, order, click);
                        }
                    });
            }
            Node::Leaf { label, full_name, value } => {
                let is_sel = self.selected.contains(full_name);
                let payload = self.drag_payload(full_name, order);
                let id = egui::Id::new(("ch_leaf_drag", full_name.as_str()));
                let resp = ui
                    .dnd_drag_source(id, payload, |ui| {
                        let mut frame = egui::Frame::none()
                            .inner_margin(egui::Margin::symmetric(2.0, 0.0));
                        if is_sel {
                            frame = frame.fill(egui::Color32::from_rgb(45, 70, 120));
                        }
                        frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(label);
                                if let Some(v) = value {
                                    let display = if v.chars().count() > 10 {
                                        let s: String = v.chars().take(10).collect();
                                        format!("{}…", s)
                                    } else {
                                        v.clone()
                                    };
                                    ui.label(egui::RichText::new(display).small().weak());
                                }
                            });
                        });
                    })
                    .response
                    .interact(egui::Sense::click());
                if resp.clicked() {
                    let mods = ui.input(|i| i.modifiers);
                    *click = Some((full_name.clone(), mods.command, mods.shift));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;

    fn reg(channels: &[(&str, &str)]) -> ChannelRegistry {
        if channels.is_empty() {
            return ChannelRegistry::from_toml_str("[channels]\n").unwrap();
        }
        let mut toml = String::new();
        for (name, ty) in channels {
            toml.push_str(&format!(
                "[channels.\"{}\"]\ntopic = \"t\"\nproto_path = \"M.v\"\nts_path = \"M.t\"\ntype = \"{}\"\n\n",
                name, ty
            ));
        }
        ChannelRegistry::from_toml_str(&toml).unwrap()
    }

    fn node_label(node: &Node) -> &str {
        match node {
            Node::Group { label, .. } | Node::Leaf { label, .. } => label,
        }
    }

    fn is_group(node: &Node) -> bool {
        matches!(node, Node::Group { .. })
    }

    fn children(node: &Node) -> &[Node] {
        match node {
            Node::Group { children, .. } => children,
            Node::Leaf { .. } => &[],
        }
    }

    fn nodes(tree: &ChannelTree) -> Vec<Node> {
        tree.assemble(&BTreeMap::new(), &|_| None)
    }

    #[test]
    fn flat_channels_become_root_leaves() {
        let r = reg(&[("alpha", "float"), ("beta", "int")]);
        let roots = nodes(&ChannelTree::build(&r));
        assert_eq!(roots.len(), 2);
        assert!(!is_group(&roots[0]));
        assert!(!is_group(&roots[1]));
        assert_eq!(node_label(&roots[0]), "alpha");
        assert_eq!(node_label(&roots[1]), "beta");
    }

    #[test]
    fn slash_groups_siblings_under_one_group() {
        let r = reg(&[("sensors/x", "float"), ("sensors/y", "float")]);
        let roots = nodes(&ChannelTree::build(&r));
        assert_eq!(roots.len(), 1);
        let group = &roots[0];
        assert!(is_group(group));
        assert_eq!(node_label(group), "sensors");
        let kids = children(group);
        assert_eq!(kids.len(), 2);
        assert_eq!(node_label(&kids[0]), "x");
        assert_eq!(node_label(&kids[1]), "y");
    }

    #[test]
    fn deep_nesting() {
        let r = reg(&[("a/b/c", "float")]);
        let roots = nodes(&ChannelTree::build(&r));
        assert_eq!(roots.len(), 1);
        let a = &roots[0];
        assert!(is_group(a));
        assert_eq!(node_label(a), "a");
        let b = &children(a)[0];
        assert!(is_group(b));
        assert_eq!(node_label(b), "b");
        let c = &children(b)[0];
        assert!(!is_group(c));
        assert_eq!(node_label(c), "c");
    }

    #[test]
    fn mixed_grouped_and_flat() {
        // BTreeMap order: "control/mode" < "temperature"
        let r = reg(&[("control/mode", "int"), ("temperature", "float")]);
        let roots = nodes(&ChannelTree::build(&r));
        assert_eq!(roots.len(), 2);
        assert!(is_group(&roots[0]));
        assert_eq!(node_label(&roots[0]), "control");
        assert!(!is_group(&roots[1]));
        assert_eq!(node_label(&roots[1]), "temperature");
    }

    #[test]
    fn no_channels_gives_empty_tree() {
        let r = reg(&[]);
        assert!(nodes(&ChannelTree::build(&r)).is_empty());
    }

    #[test]
    fn renders_headless_without_panic() {
        let mut t = tree4();
        // Pre-select some leaves so the highlighted-frame path is exercised.
        let o = order(&t);
        t.apply_click("a", false, false, &o);
        t.apply_click("c", true, false, &o);
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                t.ui(ui, &BTreeMap::new(), |_| Some("1".to_string()));
            });
        });
    }

    #[test]
    fn ui_prunes_stale_selection() {
        let mut t = tree4();
        let o = order(&t);
        t.apply_click("a", false, false, &o);
        t.selected.insert("ghost".to_string()); // no longer in the tree
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                t.ui(ui, &BTreeMap::new(), |_| None);
            });
        });
        assert!(!t.selected.contains("ghost"));
        assert!(t.selected.contains("a"));
    }

    #[test]
    fn extra_topics_merge_and_dedup_registry() {
        // "sensors/x" also in registry → not duplicated; "mqtt/temp" is new.
        let r = reg(&[("sensors/x", "float")]);
        let tree = ChannelTree::build(&r);
        let mut extra = BTreeMap::new();
        extra.insert("sensors/x".to_string(), "9".to_string());
        extra.insert("mqtt/temp".to_string(), "21.5".to_string());
        let roots = tree.assemble(&extra, &|_| None);
        // "sensors" group (with single x leaf) + "mqtt" group. x not duplicated.
        assert_eq!(roots.len(), 2);
        let sensors = roots.iter().find(|n| node_label(n) == "sensors").unwrap();
        assert_eq!(children(sensors).len(), 1);
    }

    fn order(tree: &ChannelTree) -> Vec<String> {
        flat_leaf_order(&tree.assemble(&BTreeMap::new(), &|_| None))
    }

    fn tree4() -> ChannelTree {
        // Flat order: a, b, c, d
        ChannelTree::build(&reg(&[
            ("a", "float"),
            ("b", "float"),
            ("c", "float"),
            ("d", "float"),
        ]))
    }

    #[test]
    fn sync_picks_up_dynamic_channels_and_keeps_selection() {
        let r = reg(&[("alpha", "float")]);
        let mut t = ChannelTree::build(&r);
        let o = order(&t);
        t.apply_click("alpha", false, false, &o);

        // A script output / dropped MQTT topic registers after build.
        let r2 = reg(&[("alpha", "float"), ("scripts/out", "float")]);
        t.sync(&r2);

        let roots = t.assemble(&BTreeMap::new(), &|_| None);
        let labels: Vec<&str> = roots.iter().map(node_label).collect();
        assert!(labels.contains(&"scripts")); // new channel now in the tree
        assert!(t.selected.contains("alpha")); // selection survived
    }

    #[test]
    fn plain_click_selects_single_and_sets_anchor() {
        let mut t = tree4();
        let o = order(&t);
        t.apply_click("b", false, false, &o);
        assert_eq!(t.selected, HashSet::from(["b".to_string()]));
        assert_eq!(t.anchor.as_deref(), Some("b"));
        // A second plain click replaces, not accumulates.
        t.apply_click("c", false, false, &o);
        assert_eq!(t.selected, HashSet::from(["c".to_string()]));
    }

    #[test]
    fn ctrl_click_toggles_membership() {
        let mut t = tree4();
        let o = order(&t);
        t.apply_click("a", false, false, &o);
        t.apply_click("c", true, false, &o); // add
        assert_eq!(t.selected, HashSet::from(["a".to_string(), "c".to_string()]));
        t.apply_click("a", true, false, &o); // remove
        assert_eq!(t.selected, HashSet::from(["c".to_string()]));
    }

    #[test]
    fn shift_click_selects_inclusive_range_from_anchor() {
        let mut t = tree4();
        let o = order(&t);
        t.apply_click("b", false, false, &o); // anchor = b
        t.apply_click("d", false, true, &o); // range b..=d
        assert_eq!(
            t.selected,
            HashSet::from(["b".to_string(), "c".to_string(), "d".to_string()])
        );
        // Range is order-agnostic: shift back up to a covers a..=b.
        t.apply_click("a", false, true, &o);
        assert_eq!(t.selected, HashSet::from(["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn ctrl_shift_unions_range_into_selection() {
        let mut t = tree4();
        let o = order(&t);
        t.apply_click("a", false, false, &o); // {a}, anchor a
        t.apply_click("a", true, false, &o); // toggle off but anchor stays a; {}
        t.apply_click("d", true, false, &o); // {d}, anchor d
        t.apply_click("b", true, true, &o); // union range b..=d
        assert_eq!(
            t.selected,
            HashSet::from(["b".to_string(), "c".to_string(), "d".to_string()])
        );
    }

    #[test]
    fn shift_without_anchor_falls_back_to_plain() {
        let mut t = tree4();
        let o = order(&t);
        t.apply_click("c", false, true, &o); // no anchor yet
        assert_eq!(t.selected, HashSet::from(["c".to_string()]));
        assert_eq!(t.anchor.as_deref(), Some("c"));
    }

    #[test]
    fn drag_payload_carries_selection_in_tree_order() {
        let mut t = tree4();
        let o = order(&t);
        t.apply_click("d", false, false, &o);
        t.apply_click("a", true, false, &o); // {a, d}
        // Grabbing a selected leaf carries the whole set, tree-ordered.
        assert_eq!(t.drag_payload("a", &o), vec!["a".to_string(), "d".to_string()]);
        // Grabbing an unselected leaf carries just that one.
        assert_eq!(t.drag_payload("c", &o), vec!["c".to_string()]);
    }

    #[test]
    fn leaf_shows_value_from_value_of() {
        let r = reg(&[("alpha", "float")]);
        let roots = ChannelTree::build(&r).assemble(&BTreeMap::new(), &|n| {
            (n == "alpha").then(|| "42".to_string())
        });
        match &roots[0] {
            Node::Leaf { value, .. } => assert_eq!(value.as_deref(), Some("42")),
            _ => panic!("expected leaf"),
        }
    }
}
