//! Serde layout schema for the iced workspace, superseding the legacy
//! JSON blob stored in [`super::ScreenConfig::tiles_json`].
//!
//! The canonical layout is a recursive [`LayoutNode`] tree.  At save time the
//! workspace serialises its canonical tree into the `[screens.<name>]` section
//! of `config.toml`; at load time the workspace deserialises it and builds a
//! `pane_grid::State` from it via `pane_grid::Configuration`.
//!
//! ## TOML shape (per screen)
//!
//! ```toml
//! [screens.main]
//! [screens.main.layout]
//! axis = "v"
//! ratio = 0.5
//! [screens.main.layout.a]
//! type = "numeric"
//! # … panel config fields …
//! [screens.main.layout.b]
//! type = "gauge"
//! ```

use serde::{Deserialize, Serialize};

use super::PanelEntry;

/// The split axis in a [`LayoutNode::Split`].
///
/// `H` (horizontal) means the split is a horizontal line — the two children
/// are stacked top/bottom.  `V` (vertical) means a vertical line — side by
/// side.  This maps 1-to-1 onto `iced::widget::pane_grid::Axis`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Horizontal split — top/bottom.
    H,
    /// Vertical split — left/right.
    V,
}

impl From<Axis> for iced::widget::pane_grid::Axis {
    fn from(a: Axis) -> Self {
        match a {
            Axis::H => iced::widget::pane_grid::Axis::Horizontal,
            Axis::V => iced::widget::pane_grid::Axis::Vertical,
        }
    }
}

impl From<iced::widget::pane_grid::Axis> for Axis {
    fn from(a: iced::widget::pane_grid::Axis) -> Self {
        match a {
            iced::widget::pane_grid::Axis::Horizontal => Axis::H,
            iced::widget::pane_grid::Axis::Vertical => Axis::V,
        }
    }
}

/// A node in the canonical workspace layout tree.
///
/// Leaves hold a [`PanelEntry`]; internal nodes split the available area.
/// This is the source of truth that [`crate::ui::workspace::Workspace`] owns;
/// the `pane_grid::State` is _derived_ from it and is rebuilt whenever the
/// tree changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LayoutNode {
    /// An internal split node.
    Split {
        /// The axis along which the space is divided.
        axis: Axis,
        /// Fraction [0, 1] allocated to child `a` (top or left).
        ratio: f32,
        /// Top/left child.
        a: Box<LayoutNode>,
        /// Bottom/right child.
        b: Box<LayoutNode>,
    },
    /// A leaf containing one panel.
    Leaf(PanelEntry),
}

impl LayoutNode {
    /// Collect all [`PanelEntry`] leaves in left-to-right, top-to-bottom order.
    pub fn leaves(&self) -> Vec<&PanelEntry> {
        match self {
            LayoutNode::Leaf(entry) => vec![entry],
            LayoutNode::Split { a, b, .. } => {
                let mut v = a.leaves();
                v.extend(b.leaves());
                v
            }
        }
    }

    /// Build a default single-pane layout from a list of panels.
    ///
    /// With 0 or 1 panels the tree is a single leaf (ErrorPanel placeholder if
    /// empty).  With 2+ panels they are stacked vertically in equal halves.
    pub fn from_panels(panels: &[PanelEntry]) -> Self {
        match panels {
            [] => LayoutNode::Leaf(PanelEntry {
                panel_type: "error".into(),
                config: toml::Table::new(),
            }),
            [single] => LayoutNode::Leaf(single.clone()),
            [a, b] => LayoutNode::Split {
                axis: Axis::V,
                ratio: 0.5,
                a: Box::new(LayoutNode::Leaf(a.clone())),
                b: Box::new(LayoutNode::Leaf(b.clone())),
            },
            _ => {
                // Split first panel off the top; recurse for the rest.
                let (first, rest) = panels.split_at(1);
                LayoutNode::Split {
                    axis: Axis::V,
                    ratio: 1.0 / panels.len() as f32,
                    a: Box::new(LayoutNode::Leaf(first[0].clone())),
                    b: Box::new(LayoutNode::from_panels(rest)),
                }
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

    #[test]
    fn layout_node_roundtrips() {
        let node = LayoutNode::Split {
            axis: Axis::H,
            ratio: 0.5,
            a: Box::new(LayoutNode::Leaf(PanelEntry {
                panel_type: "numeric".into(),
                config: Default::default(),
            })),
            b: Box::new(LayoutNode::Leaf(PanelEntry {
                panel_type: "gauge".into(),
                config: Default::default(),
            })),
        };
        let s = toml::to_string(&node).unwrap();
        let back: LayoutNode = toml::from_str(&s).unwrap();
        assert_eq!(node, back);
    }

    #[test]
    fn single_leaf_roundtrips() {
        let node = LayoutNode::Leaf(PanelEntry {
            panel_type: "waveform".into(),
            config: {
                let mut t = toml::Table::new();
                t.insert("channel".into(), toml::Value::String("demo.sine".into()));
                t
            },
        });
        let s = toml::to_string(&node).unwrap();
        let back: LayoutNode = toml::from_str(&s).unwrap();
        assert_eq!(node, back);
    }

    #[test]
    fn from_panels_empty_gives_error_placeholder() {
        let node = LayoutNode::from_panels(&[]);
        assert!(matches!(node, LayoutNode::Leaf(ref e) if e.panel_type == "error"));
    }

    #[test]
    fn from_panels_two_panels_splits() {
        let panels = vec![
            PanelEntry { panel_type: "a".into(), config: Default::default() },
            PanelEntry { panel_type: "b".into(), config: Default::default() },
        ];
        let node = LayoutNode::from_panels(&panels);
        assert!(matches!(node, LayoutNode::Split { .. }));
    }
}
