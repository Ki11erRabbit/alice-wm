//! Dispatch handlers for wlr-screencopy-unstable-v1.
//!
//! Generic over `B: Backend` — works for `Alice<UdevData>`, `Alice<WinitData>`,
//! or any other backend, as long as that backend implements
//! `Backend::screencopy_id`, `Backend::output_physical_size`, and
//! `Backend::copy_frame` (see `state/backend/mod.rs`).
//!
//! Wiring, once this module exists (e.g. `src/handlers/screencopy.rs`, add
//! `mod screencopy;` in `src/handlers/mod.rs`):
//!
//!   In `Backend::setup` (udev.rs / winit.rs), replace the TODO with:
//!
//!     use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
//!     let screencopy_global = display_handle
//!         .create_global::<Alice<Self>, ZwlrScreencopyManagerV1, _>(SCREENCOPY_VERSION, ());
//!     alice.backend_data.screencopy_global = Some(screencopy_global);
//!
//!   (`SCREENCOPY_VERSION` is defined below — import it or just hardcode `3`.)

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use smithay::reexports::wayland_server::{
    protocol::wl_buffer::WlBuffer, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Physical, Rectangle};

use crate::{state::backend::Backend, Alice};

/// Highest protocol version advertised. v3 gets `buffer_done` (so multiple
/// `buffer` events, e.g. once dmabuf support is added, are safe to send)
/// and `copy_with_damage`.
pub const SCREENCOPY_VERSION: u32 = 3;

/// Per-frame data attached to each `ZwlrScreencopyFrameV1` object.
#[derive(Debug)]
pub struct FrameData {
    output: Output,
    overlay_cursor: bool,
    region: Option<Rectangle<i32, Physical>>,
    /// Guards against a client sending `copy`/`copy_with_damage` twice on
    /// the same frame object (not spec-legal, but don't double-send events
    /// if a client does it anyway).
    copied: AtomicBool,
}

// ---- Manager global ----

impl<B> GlobalDispatch<ZwlrScreencopyManagerV1, (), Alice<B>> for Alice<B>
where
    B: Backend + 'static,
{
    fn bind(
        _state: &mut Alice<B>,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Alice<B>>,
    ) {
        data_init.init(resource, ());
    }
}

impl<B> Dispatch<ZwlrScreencopyManagerV1, (), Alice<B>> for Alice<B>
where
    B: Backend + 'static,
{
    fn request(
        state: &mut Alice<B>,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, Alice<B>>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput { frame, overlay_cursor, output } => {
                let Some(output) = Output::from_resource(&output) else {
                    return;
                };
                init_frame(state, frame, data_init, output, overlay_cursor != 0, None);
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let Some(output) = Output::from_resource(&output) else {
                    return;
                };

                // Per the wlr-screencopy protocol spec, x/y/width/height
                // are given in *output logical coordinates* (see
                // xdg_output.logical_size) — not physical/buffer pixels.
                // On a 1x output those are numerically identical, which is
                // presumably how this went unnoticed, but on any scaled
                // output (2x HiDPI here) treating them as physical directly
                // would put the region at up to half the requested
                // position/size. Build it as `Logical` first and convert
                // properly using the output's real scale before it's used
                // as `Physical` anywhere downstream (both backends'
                // `copy_frame` — and the clamping right below — assume
                // `region` is already physical).
                let requested_logical: Rectangle<i32, smithay::utils::Logical> = Rectangle {
                    loc: (x, y).into(),
                    size: (width.max(0), height.max(0)).into(),
                };
                let output_scale = output.current_scale().fractional_scale();
                let requested = requested_logical.to_physical_precise_round(output_scale);

                // Client-supplied and arriving completely unchecked (a
                // slurp selection dragged a pixel past an edge, a
                // fullscreen-region tool rounding differently than we do,
                // or just a buggy/hostile client). Clamp against the
                // output's real geometry before this ever reaches a
                // backend: both `copy_frame` impls trust `region` outright,
                // and the winit one indexes its source framebuffer
                // directly with it — an out-of-bounds rectangle there reads
                // past the end of that buffer (UB, seen as a
                // corrupted/garbage capture) rather than erroring.
                let (out_w, out_h) = state.backend_data.output_physical_size(&output);
                let output_rect: Rectangle<i32, Physical> = Rectangle::from_size((out_w, out_h).into());

                match requested.intersection(output_rect) {
                    Some(region) if region.size.w > 0 && region.size.h > 0 => {
                        init_frame(state, frame, data_init, output, overlay_cursor != 0, Some(region));
                    }
                    // Requested rectangle doesn't overlap the output at
                    // all: still create the frame object (required so the
                    // client can destroy it per the protocol's object
                    // lifetime rules), but reject it immediately rather
                    // than arming a `copy`/`copy_with_damage` that could
                    // never produce a valid image.
                    _ => init_failed_frame(frame, data_init, output),
                }
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

fn init_frame<B>(
    state: &mut Alice<B>,
    frame: New<ZwlrScreencopyFrameV1>,
    data_init: &mut DataInit<'_, Alice<B>>,
    output: Output,
    overlay_cursor: bool,
    region: Option<Rectangle<i32, Physical>>,
) where
    B: Backend + 'static,
{
    let frame_data = FrameData {
        output: output.clone(),
        overlay_cursor,
        region,
        copied: AtomicBool::new(false),
    };
    let frame = data_init.init(frame, frame_data);

    let (out_w, out_h) = state.backend_data.output_physical_size(&output);
    let (width, height) = region.map(|r| (r.size.w, r.size.h)).unwrap_or((out_w, out_h));

    // Argb8888-only for now, matching `copy_frame`'s offscreen format.
    let stride = width as u32 * 4;
    frame.buffer(smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888, width as u32, height as u32, stride);

    // `buffer_done` only exists from v2 onward.
    if frame.version() >= 2 {
        frame.buffer_done();
    }
}

/// Creates the `ZwlrScreencopyFrameV1` object (required even on a request
/// we're going to refuse — clients still need something to `destroy`) and
/// immediately sends `failed()` instead of a `buffer`/`buffer_done` pair.
/// `copied` starts `true` so a client that ignores `failed()` and calls
/// `copy`/`copy_with_damage` anyway hits `handle_copy`'s existing
/// already-copied guard and gets silently ignored rather than acted on.
fn init_failed_frame<B>(
    frame: New<ZwlrScreencopyFrameV1>,
    data_init: &mut DataInit<'_, Alice<B>>,
    output: Output,
) where
    B: Backend + 'static,
{
    let frame_data = FrameData { output, overlay_cursor: false, region: None, copied: AtomicBool::new(true) };
    let frame = data_init.init(frame, frame_data);
    frame.failed();
}

// ---- Frame object ----

impl<B> Dispatch<ZwlrScreencopyFrameV1, FrameData, Alice<B>> for Alice<B>
where
    B: Backend + 'static,
{
    fn request(
        state: &mut Alice<B>,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Alice<B>>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                handle_copy(state, resource, data, &buffer);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                // Simplification: identical to `copy`. A real implementation
                // would defer until the next output frame that actually has
                // damage and then also send a `damage` event before `ready`
                // — worth doing once basic capture is confirmed working,
                // since it's what makes continuous recording (wf-recorder,
                // OBS) cheap instead of re-copying full frames every tick.
                handle_copy(state, resource, data, &buffer);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

fn handle_copy<B>(state: &mut Alice<B>, frame: &ZwlrScreencopyFrameV1, data: &FrameData, buffer: &WlBuffer)
where
    B: Backend + 'static,
{
    if data.copied.swap(true, Ordering::SeqCst) {
        return;
    }

    // `B::copy_frame` takes `alice: &mut Alice<Self>` alone (no separate
    // `&mut self`) — see the note on the trait definition for why: `self`
    // would be `&mut alice.backend_data`, a field of `alice`, so a caller
    // could never legally hold both at once. This call is exactly what
    // that signature shape was designed to make possible: one borrow of
    // `state`, splitting `backend_data` from the rest internally.
    match B::copy_frame(state, &data.output, data.region, data.overlay_cursor, buffer) {
        Ok(()) => {
            // y_invert left unset — assumes `copy_frame`'s readback already
            // produces top-down rows. If captures come out upside-down,
            // this is the flag to flip instead of changing the row copy.
            frame.flags(zwlr_screencopy_frame_v1::Flags::empty());

            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
            let secs = now.as_secs();
            frame.ready((secs >> 32) as u32, (secs & 0xFFFF_FFFF) as u32, now.subsec_nanos());
        }
        Err(err) => {
            eprintln!("screencopy: copy_frame failed: {err}");
            frame.failed();
        }
    }
}
