//! Crossterm-specific translation boundary.
//!
//! Application state receives only `input::PointerEvent`, never Crossterm
//! mouse types or terminal protocol details.

use crate::input::{PointerAction, PointerButton, PointerEvent, PointerPosition};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

/// Converts one Crossterm mouse event into a provider-neutral pointer event.
pub fn pointer_event_from_crossterm(event: MouseEvent) -> PointerEvent {
    let position = Some(PointerPosition {
        x: event.column,
        y: event.row,
    });
    let (button, action) = match event.kind {
        MouseEventKind::Down(button) => (Some(pointer_button(button)), PointerAction::Press),
        MouseEventKind::Up(button) => (Some(pointer_button(button)), PointerAction::Release),
        MouseEventKind::Drag(button) => (Some(pointer_button(button)), PointerAction::Move),
        MouseEventKind::Moved => (None, PointerAction::Move),
        MouseEventKind::ScrollDown => (
            None,
            PointerAction::Scroll {
                delta_x: 0,
                delta_y: 1,
            },
        ),
        MouseEventKind::ScrollUp => (
            None,
            PointerAction::Scroll {
                delta_x: 0,
                delta_y: -1,
            },
        ),
        MouseEventKind::ScrollLeft => (
            None,
            PointerAction::Scroll {
                delta_x: -1,
                delta_y: 0,
            },
        ),
        MouseEventKind::ScrollRight => (
            None,
            PointerAction::Scroll {
                delta_x: 1,
                delta_y: 0,
            },
        ),
    };
    PointerEvent {
        position,
        button,
        action,
    }
}

/// Terminal focus loss becomes a provider-neutral pointer cancellation.
pub const fn pointer_cancel_from_focus_loss() -> PointerEvent {
    PointerEvent::cancel()
}

fn pointer_button(button: MouseButton) -> PointerButton {
    match button {
        MouseButton::Left => PointerButton::Primary,
        MouseButton::Right => PointerButton::Secondary,
        MouseButton::Middle => PointerButton::Middle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseEvent};

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 41,
            row: 17,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn converts_pointer_motion_and_preserves_coordinates() {
        assert_eq!(
            pointer_event_from_crossterm(mouse(MouseEventKind::Moved)),
            PointerEvent {
                position: Some(PointerPosition { x: 41, y: 17 }),
                button: None,
                action: PointerAction::Move,
            }
        );
    }

    #[test]
    fn converts_button_press() {
        assert_eq!(
            pointer_event_from_crossterm(mouse(MouseEventKind::Down(MouseButton::Left))),
            PointerEvent {
                position: Some(PointerPosition { x: 41, y: 17 }),
                button: Some(PointerButton::Primary),
                action: PointerAction::Press,
            }
        );
    }

    #[test]
    fn converts_button_release() {
        assert_eq!(
            pointer_event_from_crossterm(mouse(MouseEventKind::Up(MouseButton::Right))),
            PointerEvent {
                position: Some(PointerPosition { x: 41, y: 17 }),
                button: Some(PointerButton::Secondary),
                action: PointerAction::Release,
            }
        );
    }

    #[test]
    fn converts_focus_loss_to_pointer_cancellation() {
        assert_eq!(
            pointer_cancel_from_focus_loss(),
            PointerEvent {
                position: None,
                button: None,
                action: PointerAction::Cancel,
            }
        );
    }
}
