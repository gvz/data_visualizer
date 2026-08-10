//! Left sidebar: a scrollable channel-tree view with drag-start support.
//!
//! The sidebar is a pure view over [`ChannelTree`] — it does not mutate the
//! model directly. Pressing a leaf emits [`Message::DragChannel`] with the
//! channel's full name. Drop-onto-pane wiring is Task 12.

use iced::widget::{button, column, container, scrollable, text};
use iced::{Element, Length, Padding};

use crate::channel_tree::{ChannelTree, RenderNode};
use crate::ui::message::Message;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Depth-first list of every leaf label in the channel tree.
///
/// Used by tests to assert the sidebar enumerates all channels.
pub fn leaf_labels(tree: &ChannelTree) -> Vec<String> {
    fn walk(nodes: &[RenderNode], out: &mut Vec<String>) {
        for node in nodes {
            match node {
                RenderNode::Group { children, .. } => walk(children, out),
                RenderNode::Leaf { full_name, .. } => out.push(full_name.clone()),
            }
        }
    }
    let nodes = tree.render_nodes();
    let mut out = Vec::new();
    walk(&nodes, &mut out);
    out
}

/// Render the channel-tree sidebar as a scrollable iced [`Element`].
///
/// Groups are shown as non-interactive indented labels; leaf rows are
/// pressable buttons that emit [`Message::DragChannel`] on click.
/// Selected leaves are visually distinct (bold text).
pub fn view(tree: &ChannelTree) -> Element<'static, Message> {
    let nodes = tree.render_nodes();
    let mut rows: Vec<Element<'static, Message>> = Vec::new();
    render_nodes(&nodes, 0, &mut rows);

    let inner = column(rows)
        .spacing(1)
        .padding(4);

    container(scrollable(inner))
        .width(Length::Fixed(220.0))
        .height(Length::Fill)
        .style(|theme: &iced::Theme| {
            let palette = theme.extended_palette();
            container::Style {
                background: Some(palette.background.weak.color.into()),
                ..container::Style::default()
            }
        })
        .into()
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn render_nodes(
    nodes: &[RenderNode],
    depth: u16,
    rows: &mut Vec<Element<'static, Message>>,
) {
    for node in nodes {
        match node {
            RenderNode::Group { label, children } => {
                let left_pad: u16 = 4 + depth * 12;
                let label_widget = text(label.clone())
                    .size(13);
                let row = container(label_widget)
                    .padding(Padding {
                        top: 2.0,
                        right: 4.0,
                        bottom: 2.0,
                        left: left_pad as f32,
                    })
                    .into();
                rows.push(row);
                render_nodes(children, depth + 1, rows);
            }
            RenderNode::Leaf { label, full_name, selected } => {
                let left_pad: u16 = 4 + depth * 12;
                let label_str = if *selected {
                    format!("• {label}")
                } else {
                    label.clone()
                };
                let leaf_text = text(label_str).size(13);
                let full_name_owned = full_name.clone();
                let btn = button(
                    container(leaf_text)
                        .padding(Padding {
                            top: 2.0,
                            right: 4.0,
                            bottom: 2.0,
                            left: left_pad as f32,
                        })
                )
                .on_press(Message::DragChannel(full_name_owned))
                .style(button::text)
                .width(Length::Fill);
                rows.push(btn.into());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TWO_CHANNELS: &str = r#"
[channels."sensors/temp"]
topic      = "t"
proto_path = "M.v"
ts_path    = "M.t"
type       = "float"

[channels."sensors/humidity"]
topic      = "t"
proto_path = "M.v"
ts_path    = "M.t"
type       = "float"
"#;

    #[test]
    fn sidebar_lists_every_channel() {
        let ch = crate::config::ChannelRegistry::from_toml_str(SAMPLE_TWO_CHANNELS).unwrap();
        let tree = crate::channel_tree::ChannelTree::build(&ch);
        assert_eq!(leaf_labels(&tree).len(), 2);
    }
}
