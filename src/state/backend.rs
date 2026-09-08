pub mod winit;
pub mod udev;

use smithay::{input::keyboard::LedState, output::Output, reexports::{calloop::EventLoop, wayland_server::{backend::GlobalId, protocol::{wl_buffer::WlBuffer, wl_surface}}}, utils::{Physical, Rectangle}};

use crate::{CalloopData, config::Config};



pub trait Backend: Sized {
    const HAS_RELATIVE_MOTION: bool;
    const HAS_GESTURES: bool;

    fn setup(event_loop: &mut EventLoop<'static, CalloopData<Self>>) -> Result<CalloopData<Self>, Box<dyn std::error::Error>>;
    fn seat_name(&self) -> String;
    fn reset_buffers(&mut self, output: &Output);
    fn early_import(&mut self, surface: &wl_surface::WlSurface);
    fn update_led_state(&mut self, led_state: LedState);
    fn schedule_render(alice: &mut crate::Alice<Self>);
    /// Same as `schedule_render`, but scoped to the one output that
    /// actually has new content — the common case, e.g. a single
    /// surface's `wl_surface.commit`. Backends that can cheaply target
    /// just that output's CRTC (see `udev`'s override) should; the
    /// default here just falls back to re-rendering everything, which
    /// is always correct, just not free on a multi-output setup where
    /// most commits only ever affect one of them.
    fn schedule_render_output(alice: &mut crate::Alice<Self>, output: &Output) {
        let _ = output;
        Self::schedule_render(alice);
    }
    fn make_config() -> Config;
    fn screencopy_id(&mut self) -> GlobalId;
    fn output_physical_size(&self, output: &Output) -> (i32, i32);
    fn copy_frame(
        alice: &mut crate::Alice<Self>,
        output: &Output,
        region: Option<Rectangle<i32, Physical>>,
        overlay_cursor: bool,
        buffer: &WlBuffer,
    ) -> Result<(), Box<dyn std::error::Error>>;
    fn change_vt(&mut self, vt: i32);

}

