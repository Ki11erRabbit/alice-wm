pub mod winit;
pub mod udev;

use smithay::{input::keyboard::LedState, output::Output, reexports::{calloop::EventLoop, wayland_server::{protocol::wl_surface}}};

use crate::CalloopData;



pub trait Backend: Sized {
    const HAS_RELATIVE_MOTION: bool;
    const HAS_GESTURES: bool;

    fn setup(event_loop: &mut EventLoop<'static, CalloopData<Self>>) -> Result<CalloopData<Self>, Box<dyn std::error::Error>>;
    fn seat_name(&self) -> String;
    fn reset_buffers(&mut self, output: &Output);
    fn early_import(&mut self, surface: &wl_surface::WlSurface);
    fn update_led_state(&mut self, led_state: LedState);
    fn schedule_render(alice: &mut crate::Alice<Self>);
}

