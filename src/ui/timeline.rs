//! Timeline scrub bar — a canvas-based playhead drawn over the recording's
//! time range. Dragging emits [`Message::Seek`].

use iced::widget::canvas::{self, Frame, Program, Stroke};
use iced::widget::canvas::path::Builder;
use iced::{Color, Element, Length, Point};
use iced::mouse;

use super::message::Message;

// ---------------------------------------------------------------------------
// Pure geometry helper
// ---------------------------------------------------------------------------

/// Map a timestamp `ts` within `range` to a pixel column in `[0, width)`.
///
/// Returns 0.0 if the range has zero duration.
pub fn playhead_x(range: (i64, i64), ts: i64, width: f32) -> f32 {
    let (start, end) = range;
    let duration = end - start;
    if duration == 0 {
        return 0.0;
    }
    let frac = (ts - start) as f64 / duration as f64;
    (frac as f32 * width).clamp(0.0, width)
}

// ---------------------------------------------------------------------------
// Canvas Program
// ---------------------------------------------------------------------------

/// Interactive state for the timeline canvas (tracks drag).
#[derive(Default)]
pub struct TimelineState {
    /// Whether the left mouse button is currently held down.
    dragging: bool,
}

/// The timeline canvas Program.
///
/// Draws a coloured background plus a vertical playhead line.
/// Mouse clicks/drags emit [`Message::Seek`].
pub struct TimelineProgram {
    /// Time range of the loaded recording: (start_ns, start_ns + duration_ns).
    pub range: (i64, i64),
    /// Current playhead position in nanoseconds.
    pub position_ns: i64,
}

impl Program<Message> for TimelineProgram {
    type State = TimelineState;

    fn update(
        &self,
        state: &mut TimelineState,
        event: &canvas::Event,
        bounds: iced::Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let seek_at_cursor = |cursor: mouse::Cursor| -> Option<canvas::Action<Message>> {
            let pos = cursor.position_in(bounds)?;
            let (start, end) = self.range;
            let duration = end - start;
            if duration == 0 {
                return None;
            }
            let frac = (pos.x / bounds.width).clamp(0.0, 1.0) as f64;
            let ts = start + (frac * duration as f64) as i64;
            Some(canvas::Action::publish(Message::Seek(ts)).and_capture())
        };

        match event {
            canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                state.dragging = true;
                seek_at_cursor(cursor)
            }
            canvas::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.dragging = false;
                None
            }
            canvas::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                seek_at_cursor(cursor)
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &TimelineState,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry<iced::Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());

        // Background.
        frame.fill_rectangle(
            Point::ORIGIN,
            bounds.size(),
            Color::from_rgb(0.15, 0.15, 0.18),
        );

        // Playhead vertical line.
        let x = playhead_x(self.range, self.position_ns, bounds.width);
        let mut builder = Builder::new();
        builder.move_to(Point::new(x, 0.0));
        builder.line_to(Point::new(x, bounds.height));
        let path = builder.build();
        frame.stroke(
            &path,
            Stroke::default()
                .with_color(Color::from_rgb(0.9, 0.4, 0.1))
                .with_width(2.0),
        );

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &TimelineState,
        _bounds: iced::Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.dragging {
            mouse::Interaction::ResizingHorizontally
        } else {
            mouse::Interaction::Pointer
        }
    }
}

/// Build the timeline widget for use in the main layout.
pub fn view(range: (i64, i64), position_ns: i64) -> Element<'static, Message> {
    use iced::widget::canvas;
    canvas(TimelineProgram { range, position_ns })
        .width(Length::Fill)
        .height(Length::Fixed(80.0))
        .into()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playhead_x_midpoint() {
        // (0, 100) at ts=50, width=200 → 100.0
        let x = playhead_x((0, 100), 50, 200.0);
        assert!((x - 100.0).abs() < 0.001, "expected 100.0 got {x}");
    }

    #[test]
    fn playhead_x_start() {
        let x = playhead_x((1_000_000_000, 3_000_000_000), 1_000_000_000, 400.0);
        assert!((x - 0.0).abs() < 0.001, "expected 0.0 got {x}");
    }

    #[test]
    fn playhead_x_end() {
        let x = playhead_x((0, 1_000), 1_000, 500.0);
        assert!((x - 500.0).abs() < 0.001, "expected 500.0 got {x}");
    }

    #[test]
    fn playhead_x_clamped_below() {
        let x = playhead_x((100, 200), 50, 300.0);
        assert!((x - 0.0).abs() < 0.001, "expected 0.0 got {x}");
    }

    #[test]
    fn playhead_x_clamped_above() {
        let x = playhead_x((100, 200), 300, 300.0);
        assert!((x - 300.0).abs() < 0.001, "expected 300.0 got {x}");
    }

    #[test]
    fn playhead_x_zero_range_returns_zero() {
        let x = playhead_x((500, 500), 500, 200.0);
        assert!((x - 0.0).abs() < 0.001, "expected 0.0 got {x}");
    }
}
