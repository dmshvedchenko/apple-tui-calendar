//! Provider-neutral input types consumed by application state.
//!
//! Terminal adapters translate their native input into these values. Pointer
//! events describe intent only; drag recognition and event mutation stay out
//! of this module.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerPosition {
    pub x: u16,
    pub y: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerAction {
    Move,
    Press,
    Release,
    Cancel,
    /// Retained for existing wheel navigation. Positive `delta_y` means down.
    Scroll {
        delta_x: i16,
        delta_y: i16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerEvent {
    /// Focus-loss cancellation may not have a terminal coordinate.
    pub position: Option<PointerPosition>,
    pub button: Option<PointerButton>,
    pub action: PointerAction,
}

impl PointerEvent {
    pub const fn cancel() -> Self {
        Self {
            position: None,
            button: None,
            action: PointerAction::Cancel,
        }
    }
}
