//! Reusable modal overlay helper using iced 0.14's `stack!` widget.
//!
//! Layers a centred `card` element on top of a semi-transparent `base` element.
//! Clicking the backdrop outside the card emits `on_dismiss`.

use iced::widget::{container, mouse_area, opaque, stack};
use iced::{Color, Element, Length};

/// Wrap `base` with a centred card overlay and a dismissible backdrop.
///
/// * `base`       — the main application UI rendered underneath.
/// * `card`       — the modal content (centred automatically).
/// * `on_dismiss` — message emitted when the user clicks outside the card.
pub fn modal<'a, Message: Clone + 'a>(
    base: Element<'a, Message>,
    card: Element<'a, Message>,
    on_dismiss: Message,
) -> Element<'a, Message> {
    // Semi-transparent dark backdrop that catches click-to-dismiss events.
    let backdrop = mouse_area(
        container(iced::widget::Space::new())
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme: &iced::Theme| container::Style {
                background: Some(
                    Color { r: 0.0, g: 0.0, b: 0.0, a: 0.45 }.into(),
                ),
                ..container::Style::default()
            }),
    )
    .on_press(on_dismiss);

    // Centre the card within the overlay layer.
    let overlay = container(opaque(card))
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill);

    stack![
        base,
        opaque(backdrop),
        overlay,
    ]
    .into()
}
