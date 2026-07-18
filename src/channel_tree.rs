use eframe::egui;

use crate::config::ChannelRegistry;

enum Node {
    Group { label: String, children: Vec<Node> },
    Leaf  { label: String, full_name: String },
}

/// Channel name tree, split on '/' separators.
///
/// "sensor/imu/accel_x" → Group("sensor") → Group("imu") → Leaf("accel_x")
/// "demo.sine"          → Leaf("demo.sine")  (no '/' → flat leaf)
pub struct ChannelTree {
    roots: Vec<Node>,
}

fn insert_path(nodes: &mut Vec<Node>, parts: &[&str], full_name: &str) {
    if parts.len() == 1 {
        nodes.push(Node::Leaf { label: parts[0].to_string(), full_name: full_name.to_string() });
        return;
    }
    let group_label = parts[0];
    if let Some(pos) = nodes
        .iter()
        .position(|n| matches!(n, Node::Group { label, .. } if label == group_label))
    {
        if let Node::Group { children, .. } = &mut nodes[pos] {
            insert_path(children, &parts[1..], full_name);
        }
    } else {
        let mut children = Vec::new();
        insert_path(&mut children, &parts[1..], full_name);
        nodes.push(Node::Group { label: group_label.to_string(), children });
    }
}

impl ChannelTree {
    pub fn build(registry: &ChannelRegistry) -> Self {
        let mut roots: Vec<Node> = Vec::new();
        for id in registry.iter_ids() {
            let name = &registry.meta(id).name;
            let parts: Vec<&str> = name.split('/').collect();
            insert_path(&mut roots, &parts, name);
        }
        Self { roots }
    }

    /// Render the tree with checkboxes. `selected` is the shared selection list.
    pub fn ui(&self, ui: &mut egui::Ui, selected: &mut Vec<String>) {
        for node in &self.roots {
            render_node(ui, node, selected);
        }
    }
}

fn render_node(ui: &mut egui::Ui, node: &Node, selected: &mut Vec<String>) {
    match node {
        Node::Group { label, children } => {
            egui::CollapsingHeader::new(label)
                .default_open(true)
                .show(ui, |ui| {
                    for child in children {
                        render_node(ui, child, selected);
                    }
                });
        }
        Node::Leaf { label, full_name } => {
            let mut checked = selected.contains(full_name);
            if ui.checkbox(&mut checked, label).changed() {
                if checked {
                    selected.push(full_name.clone());
                } else {
                    selected.retain(|n| n != full_name);
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

    #[test]
    fn flat_channels_become_root_leaves() {
        let r = reg(&[("alpha", "float"), ("beta", "int")]);
        let tree = ChannelTree::build(&r);
        assert_eq!(tree.roots.len(), 2);
        assert!(!is_group(&tree.roots[0]));
        assert!(!is_group(&tree.roots[1]));
        assert_eq!(node_label(&tree.roots[0]), "alpha");
        assert_eq!(node_label(&tree.roots[1]), "beta");
    }

    #[test]
    fn slash_groups_siblings_under_one_group() {
        let r = reg(&[("sensors/x", "float"), ("sensors/y", "float")]);
        let tree = ChannelTree::build(&r);
        assert_eq!(tree.roots.len(), 1);
        let group = &tree.roots[0];
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
        let tree = ChannelTree::build(&r);
        assert_eq!(tree.roots.len(), 1);
        let a = &tree.roots[0];
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
        let tree = ChannelTree::build(&r);
        assert_eq!(tree.roots.len(), 2);
        assert!(is_group(&tree.roots[0]));
        assert_eq!(node_label(&tree.roots[0]), "control");
        assert!(!is_group(&tree.roots[1]));
        assert_eq!(node_label(&tree.roots[1]), "temperature");
    }

    #[test]
    fn no_channels_gives_empty_tree() {
        let r = reg(&[]);
        let tree = ChannelTree::build(&r);
        assert!(tree.roots.is_empty());
    }
}
