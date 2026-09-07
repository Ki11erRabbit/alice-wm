//! Touchpad swipe gestures.
//!
//! A gesture is bound in the user's config to the exact same [`Action`]
//! a keybinding can trigger (see `config.rs`'s `gesture`/`Direction`).
//! For most actions that's all there is to it: the action fires once,
//! on release, same as a key press.
//!
//! The four actions this was actually built for —
//! [`Action::MoveUpStack`]/[`Action::MoveDownStack`] (the stack-reorder
//! reflow) and [`Action::FocusNextTag`]/[`Action::FocusPreviousTag`]
//! (the tag-switch slide) — already animate when triggered from the
//! keyboard. Bound to a gesture, they additionally get driven 1:1 by the
//! finger instead of running on a fixed timer: the swipe *is* the
//! animation's progress, live, until the finger lifts, at which point it
//! either finishes settling into place or eases back to where it
//! started. See `Alice::gesture_update`/`gesture_end` in `state.rs` for
//! the state machine that actually does this — this module just holds
//! the data it operates on.

use smithay::reexports::wayland_server::backend::ObjectId;

use crate::{config::Action, output::OutputId};

/// One of the four cardinal directions a bound swipe can resolve to.
/// Diagonal swipes resolve to whichever axis has moved further once the
/// dead zone (see [`DEAD_ZONE`]) clears — see `Alice::gesture_update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GestureDirection {
    Up,
    Down,
    Left,
    Right,
}

/// How far (in logical pixels, summed across both axes independently) a
/// swipe has to travel before it's resolved into a direction and, if
/// bound to something, starts actually driving that thing. Below this,
/// it's ambiguous — a click-and-drag starting to move, a gesture that
/// might still turn into a diagonal, or on some touchpads just noise —
/// so nothing happens yet.
pub const DEAD_ZONE: f64 = 12.0;

/// How far (in logical pixels) a resolved swipe has to travel before its
/// bound animation is considered "complete" if released right there.
/// Tuned for a trackpad rather than the on-screen distance the animation
/// itself covers (an output's full width, for a tag switch) — dragging a
/// finger the width of an output across a trackpad would make the
/// gesture nearly unusable.
pub const GESTURE_DISTANCE: f64 = 220.0;

/// Progress (0.0..=1.0) a resolved swipe must have reached when released
/// for its bound animation to be committed rather than cancelled.
pub const COMMIT_THRESHOLD: f64 = 0.5;

/// How long the hand-off animation (`Animation::release_to`) takes once
/// a gesture ends, whichever way it resolves — commit or cancel.
pub const RELEASE_DURATION_MS: u64 = 200;

/// State for one swipe currently in progress, from `GestureSwipeBegin`
/// until its matching `GestureSwipeEnd`.
pub struct ActiveGesture {
    pub fingers: u32,
    /// Accumulated raw delta since the gesture began, in logical pixels
    /// — `(x, y)`. Kept even after resolving (rather than switching to
    /// per-update deltas) so progress can always be recomputed from
    /// scratch as "total distance travelled", with no risk of drifting
    /// from rounding a running sum some other way.
    pub total: (f64, f64),
    /// `None` until the swipe clears the dead zone; see
    /// `Alice::gesture_update`.
    pub resolved: Option<Resolved>,
}

impl ActiveGesture {
    pub fn new(fingers: u32) -> Self {
        Self {
            fingers,
            total: (0.0, 0.0),
            resolved: None,
        }
    }
}

/// A swipe that has cleared the dead zone and committed to a direction.
pub struct Resolved {
    pub direction: GestureDirection,
    pub kind: ResolvedKind,
    /// Progress last computed in `gesture_update`, `0.0..=1.0` — reused
    /// by `gesture_end` to decide whether to commit or cancel.
    pub progress: f64,
}

/// What a resolved swipe is actually doing, frame to frame.
pub enum ResolvedKind {
    /// Driving an in-flight `TagSlideAnimation` on `output` directly, 1:1
    /// with the finger — started by firing `Action::FocusNextTag`/
    /// `FocusPreviousTag` the moment the swipe resolved, then hijacking
    /// the `Timed` animation that started straight into a `Manual` one.
    Tag { output: OutputId },
    /// Driving a batch of in-flight `WindowMorph`s (identified by their
    /// surface's `ObjectId`, the same key `Alice::window_morphs` uses) —
    /// started the same way, by firing `Action::MoveUpStack`/
    /// `MoveDownStack` immediately and taking over the morphs it created.
    /// `undo` is the opposite of whichever action started this: cancelling
    /// re-fires it, which both reverts the stack reorder itself and — via
    /// `start_window_morph_impl`'s "already mid-animation" case — eases
    /// smoothly back from wherever the drag currently sits rather than
    /// snapping first.
    Stack { keys: Vec<ObjectId>, undo: Action },
    /// This direction wasn't bound to anything animatable — either
    /// nothing at all, or an ordinary action with no live progress to
    /// drive. Nothing to do frame to frame; `action` (if any) fires once
    /// on release if the swipe travelled far enough.
    Discrete { action: Option<Action> },
}
