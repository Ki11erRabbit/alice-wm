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

use crate::{layout::Rect, output::TagId};

/// A single scalar animation progress value, either driven by wall-clock
/// time (`Timed`) or set directly by the caller (`Manual`).
///
/// Construct a `Timed` one with `Animation::new(duration)` at the moment
/// the animation should start, then call `progress`/`eased_progress` with
/// the current time on every frame you render while it's running.
/// Everything is derived from `start` and `duration` — there's no
/// mutable "current position" field to keep in sync, which is what lets
/// this be driven equally well by a render loop ticking every frame.
///
/// `Manual` exists for the other case: a touchpad gesture, where there's
/// no clock to derive progress from — the finger *is* the clock. Build
/// one with `Animation::manual(0.0)` and call `set_manual_progress` on
/// every gesture update; `eased_progress` then reports exactly that value
/// back, unmodified (raw 1:1 tracking, no easing curve, while the finger
/// is actually driving it — the ease-out curve only kicks back in once
/// `release_to` hands it off to a `Timed` animation for the finger-up
/// settle).
#[derive(Debug, Clone, Copy)]
enum AnimationKind {
    Timed {
        start: Instant,
        duration: Duration,
        /// The progress value this animation eases *from* — usually
        /// `0.0`, except when `release_to` hands off from a manual
        /// (gesture) progress value partway through.
        from: f64,
        /// The progress value this animation eases *to* — usually
        /// `1.0`, except when `release_to` is settling a cancelled
        /// gesture back to where it started.
        to: f64,
    },
    Manual {
        progress: f64,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Animation(AnimationKind);

impl Animation {
    pub fn new(duration: Duration) -> Self {
        Self(AnimationKind::Timed {
            start: Instant::now(),
            duration,
            from: 0.0,
            to: 1.0,
        })
    }

    /// A gesture-driven animation: progress is whatever `progress` is set
    /// to (see `set_manual_progress`), not something that advances on its
    /// own. Used for the touchpad-gesture case — see the type doc above.
    pub fn manual(progress: f64) -> Self {
        Self(AnimationKind::Manual {
            progress: progress.clamp(0.0, 1.0),
        })
    }

    /// Overwrites a `Manual` animation's progress — the thing a gesture
    /// handler calls on every `GestureSwipeUpdate`. A no-op on a `Timed`
    /// animation; there's no external value to overwrite there.
    pub fn set_manual_progress(&mut self, progress: f64) {
        if let AnimationKind::Manual { progress: p } = &mut self.0 {
            *p = progress.clamp(0.0, 1.0);
        }
    }

    /// Hands a `Manual` (gesture) animation off to an ordinary `Timed`
    /// one that eases from wherever it currently sits to `target` (`1.0`
    /// to complete, `0.0` to cancel back to the start) over `duration`.
    /// Called the instant a gesture ends: the finger stops driving
    /// progress, but the motion should still settle smoothly instead of
    /// snapping straight to `target`.
    ///
    /// Works just as well on an already-`Timed` animation — it re-eases
    /// from that animation's current eased progress instead, which is
    /// what lets a second gesture (or a keypress) interrupt one that's
    /// already mid-release without a visible jump.
    pub fn release_to(&self, now: Instant, target: f64, duration: Duration) -> Animation {
        Self(AnimationKind::Timed {
            start: now,
            duration,
            from: self.eased_progress(now),
            to: target,
        })
    }

    /// Linear progress from `0.0` (just started) to `1.0` (finished).
    /// Always clamped to that range, so callers never have to guard
    /// against overshoot themselves. For a `Manual` animation this is
    /// always `1.0` — there's no "still running" concept, only whatever
    /// `progress` currently is (see `eased_progress`).
    pub fn progress(&self, now: Instant) -> f64 {
        match self.0 {
            AnimationKind::Timed { start, duration, .. } => {
                if duration.is_zero() {
                    return 1.0;
                }
                let elapsed = now.saturating_duration_since(start).as_secs_f64();
                (elapsed / duration.as_secs_f64()).clamp(0.0, 1.0)
            }
            AnimationKind::Manual { .. } => 1.0,
        }
    }

    /// `progress`, passed through an ease-out curve: fast to start,
    /// gently settling into place, rather than moving at a constant
    /// speed and stopping abruptly. This is what should actually drive
    /// on-screen motion in almost every case.
    ///
    /// For a `Manual` animation this is the raw progress value, completely
    /// unmodified — deliberately: while a gesture is actually driving it,
    /// the on-screen motion should track the finger exactly, not run it
    /// through a curve meant for a fire-and-forget timed animation.
    pub fn eased_progress(&self, now: Instant) -> f64 {
        match self.0 {
            AnimationKind::Timed { from, to, .. } => {
                from + (to - from) * ease_out_cubic(self.progress(now))
            }
            AnimationKind::Manual { progress } => progress,
        }
    }

    /// A `Manual` animation never "finishes" on its own — only
    /// `release_to`, converting it to `Timed`, can end one. Always
    /// `false` for `Manual`.
    pub fn is_finished(&self, now: Instant) -> bool {
        match self.0 {
            AnimationKind::Timed { start, duration, .. } => {
                now.saturating_duration_since(start) >= duration
            }
            AnimationKind::Manual { .. } => false,
        }
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
    /// The tag this switch started on — where a gesture-driven switch
    /// reverts to if the swipe that started it is released before
    /// completing (see `completed`/`Alice::gesture_end`'s `Tag` arm).
    /// Ordinary, non-gesture switches never look at this.
    pub old_tag: TagId,
    /// The tag this switch is headed to.
    pub new_tag: TagId,
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

    /// Whether this switch's progress, at `now`, represents "arrived at
    /// `new_tag`" (`true`) rather than "back where it started, at
    /// `old_tag`" (`false`). For an ordinary switch — which always eases
    /// from `0.0` to `1.0` — this is always `true` once finished, so
    /// existing (non-gesture) callers are unaffected. It only ever comes
    /// out `false` for a gesture that got released back toward `0.0`
    /// (see `Animation::release_to`) — i.e. a swipe the user let go of
    /// before it crossed the commit threshold. `advance_tag_animations`
    /// uses this to decide, once the animation finishes, whether to
    /// finalize as "switched" or unwind back to `old_tag`.
    pub fn completed(&self, now: Instant) -> bool {
        self.animation.eased_progress(now) >= 0.5
    }
}

// ---------------------------------------------------------------------
// Window morphs: a single window's box (position AND size) animating
// independently of Space/layout — grow-in on open, shrink-out on close,
// and the reflow when a sibling appears/disappears/reorders.
// ---------------------------------------------------------------------

use smithay::{
    backend::renderer::{
        ImportAll, Renderer,
        element::{Element, Id, RenderElement, AsRenderElements, surface::WaylandSurfaceRenderElement},
        utils::{CommitCounter, OpaqueRegions},
    },
    utils::{Buffer, Physical, Rectangle, Scale, Size, Transform},
};

/// What to do with a `WindowMorph` once its animation finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MorphFinish {
    /// The ordinary case (grow-in, reflow after a sibling changes, stack
    /// reorder): put the window back under `Space`'s normal rendering, at
    /// its resting position.
    Remap,
    /// The close case: the window is actually gone once this finishes —
    /// unmap it for good instead of remapping it.
    Unmap,
}

/// A window whose on-screen box is animating outside of the normal
/// layout flow: open (grow from a small placeholder), close (shrink to
/// one, played in reverse), a stack reorder, or a sibling's reflow. All
/// of these are the exact same shape — `from` a `Rect`, `to` a `Rect` —
/// so one struct and one render path cover every case.
///
/// Unlike `TagSlideAnimation`, this changes apparent *size*, which
/// `Space::map_element` can't do — the window is unmapped from `Space`
/// for the duration (see `Alice::start_window_morph`) and rendered
/// instead via `morph_render_elements`, which stretches its existing,
/// already-committed buffer to fill whatever `current_rect` currently
/// is. The client is only ever sent one real `configure`, to `to`'s
/// size (see the big comment on `apply_rects`) — every frame in between
/// is a purely visual stretch of that one buffer, never a new resize
/// request.
pub struct WindowMorph {
    pub window: Window,
    pub output: crate::output::OutputId,
    pub animation: Animation,
    pub from: Rect,
    pub to: Rect,
    pub on_finish: MorphFinish,
}

impl WindowMorph {
    pub fn current_rect(&self, now: Instant) -> Rect {
        let t = self.animation.eased_progress(now);
        Rect {
            x: lerp(self.from.x, self.to.x, t),
            y: lerp(self.from.y, self.to.y, t),
            width: lerp(self.from.width, self.to.width, t),
            height: lerp(self.from.height, self.to.height, t),
        }
    }

    pub fn is_finished(&self, now: Instant) -> bool {
        self.animation.is_finished(now)
    }
}

fn lerp(a: i32, b: i32, t: f64) -> i32 {
    (a as f64 + (b - a) as f64 * t).round() as i32
}

/// The small, centered placeholder a window grows out of when it opens
/// (and shrinks back into, played in reverse, when it closes) — 35% of
/// its real size on each axis, centered within the same rect.
pub fn shrink_target(rect: Rect) -> Rect {
    let w = ((rect.width as f64 * 0.35).round() as i32).max(1);
    let h = ((rect.height as f64 * 0.35).round() as i32).max(1);
    Rect {
        x: rect.x + (rect.width - w) / 2,
        y: rect.y + (rect.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Wraps a render element and reports a caller-supplied destination
/// rectangle instead of the element's own natural one. Everything else
/// (which buffer, which region of it, orientation) is delegated straight
/// through to `inner` unchanged — this is deliberately *only* a
/// destination-rectangle override.
///
/// Reporting a `dst` that differs from the buffer's real size is exactly
/// what makes this "scale" the window at all: `draw` receives back
/// whatever `geometry()` reports here as its `dst`, and stretches the
/// unchanged source buffer to fill it — the same src-to-dst blit any
/// texture-based renderer already needs for fractional scaling. There's
/// no separate "resize" step; the client's buffer never changes.
pub struct ScaledElement<E> {
    inner: E,
    dst: Rectangle<i32, Physical>,
}

impl<E> ScaledElement<E> {
    pub fn new(inner: E, dst: Rectangle<i32, Physical>) -> Self {
        Self { inner, dst }
    }
}

impl<E: Element> Element for ScaledElement<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.dst
    }
    fn location(&self, _scale: Scale<f64>) -> Point<i32, Physical> {
        self.dst.loc
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // Conservative rather than wrong: reporting "no known opaque
        // region" only costs a (tiny, short-lived-animation-only) missed
        // blending optimization, never an incorrect picture.
        OpaqueRegions::from_slice(&[])
    }
}

impl<R: Renderer, E: RenderElement<R>> RenderElement<R> for ScaledElement<E> {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        self.inner.draw(frame, src, dst, damage, opaque_regions)
    }
}

/// Builds the render elements for one morphing window at `rect` (its
/// *current*, in-between-frame box — see `WindowMorph::current_rect`).
///
/// `output_origin` is that output's own position in `Space`'s global
/// logical coordinates (`Space::output_geometry(output).loc`) — `rect`
/// is in that same global space (it comes straight from the layout
/// engine, same as everything `apply_rects` hands to
/// `Space::map_element`), and needs converting to output-local physical
/// pixels before it means anything to a renderer, the same conversion
/// `Space`'s own rendering does internally.
///
/// Multi-part windows (subsurfaces) are scaled as one rigid group: each
/// of the window's own render elements keeps its position *relative to
/// the window's top-left*, just uniformly scaled by the same factor as
/// the window as a whole, rather than every subsurface being
/// independently stretched to fill the whole target box.
pub fn morph_render_elements<R>(
    renderer: &mut R,
    window: &Window,
    rect: Rect,
    output_origin: Point<i32, Logical>,
    scale: f64,
    alpha: f32,
) -> Vec<ScaledElement<WaylandSurfaceRenderElement<R>>>
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    // The window's real, currently-configured size — never changes
    // mid-animation (see the module doc on `WindowMorph`), so it's the
    // fixed reference every frame's scale factor is computed against.
    let base = window.geometry().size;
    if base.w <= 0 || base.h <= 0 {
        return Vec::new();
    }
    let kx = rect.width as f64 / base.w as f64;
    let ky = rect.height as f64 / base.h as f64;

    let physical_scale = Scale::from(scale);
    let elements: Vec<WaylandSurfaceRenderElement<R>> =
        AsRenderElements::<R>::render_elements(window, renderer, (0, 0).into(), physical_scale, alpha);

    let local_x = rect.x - output_origin.x;
    let local_y = rect.y - output_origin.y;
    let target_origin: Point<i32, Physical> =
        Point::<i32, Logical>::from((local_x, local_y)).to_physical_precise_round(scale);

    elements
        .into_iter()
        .map(|element| {
            let geo = element.geometry(physical_scale);
            let loc: Point<i32, Physical> = Point::from((
                target_origin.x + (geo.loc.x as f64 * kx).round() as i32,
                target_origin.y + (geo.loc.y as f64 * ky).round() as i32,
            ));
            let size: Size<i32, Physical> = Size::from((
                ((geo.size.w as f64 * kx).round() as i32).max(1),
                ((geo.size.h as f64 * ky).round() as i32).max(1),
            ));
            ScaledElement::new(element, Rectangle::new(loc, size))
        })
        .collect()
}
