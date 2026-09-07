//! Generic animation infrastructure.
//!
//! The core idea (see `Animation`) is to keep "how far along is this
//! animation" (a single `f64` between 0.0 and 1.0) completely separate
//! from "what does that number actually move on screen". `Animation`
//! only knows about wall-clock time; it has no idea it's being used for
//! a tag switch. `TagSlideAnimation`, further down, is what actually
//! interprets progress as an on-screen window offset for *this*
//! animation specifically.
//!
//! Splitting it this way means a future animation (a window fade, a
//! resize tween, ...) can reuse `Animation` completely unchanged and
//! only needs to write its own small "progress -> on-screen effect"
//! struct, the same way `TagSlideAnimation` does here.

use std::time::{Duration, Instant};

use smithay::{
    desktop::Window,
    utils::{Logical, Point},
};

/// A single scalar animation progress value, driven by wall-clock time.
///
/// Construct one with `Animation::new(duration)` at the moment the
/// animation should start, then call `progress`/`eased_progress` with
/// the current time on every frame you render while it's running.
/// Everything is derived from `start` and `duration` — there's no
/// mutable "current position" field to keep in sync, which is what
/// lets this be driven equally well by a render loop ticking every
/// frame or, for something like a gesture, by input events instead.
#[derive(Debug, Clone, Copy)]
pub struct Animation {
    start: Instant,
    duration: Duration,
}

impl Animation {
    pub fn new(duration: Duration) -> Self {
        Self {
            start: Instant::now(),
            duration,
        }
    }

    /// Linear progress from `0.0` (just started) to `1.0` (finished).
    /// Always clamped to that range, so callers never have to guard
    /// against overshoot themselves.
    pub fn progress(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        (elapsed / self.duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    /// `progress`, passed through an ease-out curve: fast to start,
    /// gently settling into place, rather than moving at a constant
    /// speed and stopping abruptly. This is what should actually drive
    /// on-screen motion in almost every case — `progress` itself is
    /// mostly useful for checking completion.
    pub fn eased_progress(&self, now: Instant) -> f64 {
        ease_out_cubic(self.progress(now))
    }

    pub fn is_finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.duration
    }
}

/// Starts fast, decelerates into the landing. `t` and the result are
/// both in `0.0..=1.0`. This particular curve (`1 - (1-t)^3`) is a
/// common, cheap default for "something settling into its new place" —
/// swap it for a different curve here if you want a different feel;
/// nothing else needs to change since every caller only ever sees the
/// output of `eased_progress`, never the curve itself.
fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

/// An in-flight tag-switch slide: the outgoing tag's windows sliding
/// off one edge of the output while the incoming tag's windows slide
/// in from the other, together, as one continuous motion.
///
/// One of these is stored per output (see `Alice::tag_animations`) for
/// as long as the switch is animating. Both window lists are snapshots
/// taken once, when the animation starts — `outgoing` at the position
/// each window already occupied, `incoming` at the *final* tiled
/// position layout just computed for it. The animation only ever
/// interpolates between those two fixed snapshots; it never re-queries
/// layout mid-flight.
pub struct TagSlideAnimation {
    /// `1` if this is a "next tag" switch (the incoming tag slides in
    /// from the right, outgoing windows exit to the left), `-1` for
    /// "previous tag" (mirrored).
    pub direction: i32,
    /// How far, in logical pixels, a window travels from fully off-screen
    /// to its resting position — the output's own width, so a window is
    /// always fully off-screen before it's considered "arrived",
    /// regardless of panels/bars eating into the usable tiling area.
    pub distance: i32,
    pub animation: Animation,
    /// The tag being switched away from, at the on-screen position each
    /// window already had (unchanged — these don't move until the
    /// animation starts advancing them).
    pub outgoing: Vec<(Window, Point<i32, Logical>)>,
    /// The tag being switched to, at the final position layout gave it.
    /// The animation starts these off-screen and slides them to this
    /// exact point.
    pub incoming: Vec<(Window, Point<i32, Logical>)>,
}

impl TagSlideAnimation {
    /// The one quantity this whole animation boils down to: think of it
    /// as the position of the "seam" between the outgoing content and
    /// the incoming content, expressed as an offset from where that seam
    /// ends up at rest (screen edge, offset 0).
    ///
    /// At progress 0.0 this is `direction * distance` — a full output
    /// width away, which is exactly the offset `slide_tag` used to place
    /// the incoming windows off-screen in the first place (see the
    /// `start = final_pos + direction * distance` line there). At
    /// progress 1.0 it's `0`.
    ///
    /// Incoming windows are simply `final_position + offset`. Outgoing
    /// windows are `base_position + offset - direction * distance` — the
    /// same offset, just measured from the *other* end of the slide, so
    /// they end up exactly one output-width further along in the same
    /// direction of travel. Working out both window sets from this one
    /// number is what keeps them moving as a single continuous strip
    /// instead of two independently-timed animations that could drift
    /// out of sync with each other.
    pub fn offset_at(&self, now: Instant) -> i32 {
        let eased = self.animation.eased_progress(now);
        (self.direction as f64 * self.distance as f64 * (1.0 - eased)).round() as i32
    }

    pub fn is_finished(&self, now: Instant) -> bool {
        self.animation.is_finished(now)
    }
}
