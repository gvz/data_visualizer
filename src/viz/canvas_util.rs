//! Utilities for panels that draw on an `iced::widget::Canvas`.
//!
//! This module contains shared helpers for iced `Canvas`-based panels —
//! coordinate transforms, colour maps, grid drawing primitives, etc.

use iced::widget::canvas::{Frame, Text};
use iced::{Alignment, Color, Point};

/// Draw text centred on `position` inside a `Frame`.
///
/// Used by Gauge, Spectrum, and StateGraph panels so they don't each
/// duplicate the alignment boilerplate.
pub fn centered_text(
    frame: &mut Frame,
    position: Point,
    content: impl Into<String>,
    size: f32,
    color: Color,
) {
    use iced::alignment;
    frame.fill_text(Text {
        content: content.into(),
        position,
        color,
        size: size.into(),
        align_x: Alignment::Center.into(),
        align_y: alignment::Vertical::Center,
        ..Text::default()
    });
}
