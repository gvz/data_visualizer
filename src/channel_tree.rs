use std::collections::BTreeMap;

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
#[derive(Clone)]
pub struct ChannelTree {
    names: Vec<String>,
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
        Self { names }
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

    /// Render one drag-only "/" tree. Every leaf is a drag source and shows its
    /// live value dimmed on the right. `extra` are discovered MQTT topics not yet
    /// in the registry; the workspace resolves which drops can actually bind.
    pub fn ui(
        &self,
        ui: &mut egui::Ui,
        extra: &BTreeMap<String, String>,
        value_of: impl Fn(&str) -> Option<String>,
    ) {
        for node in &self.assemble(extra, &value_of) {
            render_topic_node(ui, node);
        }
    }
}

fn render_topic_node(ui: &mut egui::Ui, node: &Node) {
    match node {
        Node::Group { label, children } => {
            egui::CollapsingHeader::new(label)
                .default_open(true)
                .show(ui, |ui| {
                    for child in children {
                        render_topic_node(ui, child);
                    }
                });
        }
        Node::Leaf { label, full_name, value } => {
            ui.push_id(full_name.as_str(), |ui| {
                ui.dnd_drag_source(ui.id().with("drag"), full_name.clone(), |ui| {
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
            });
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
