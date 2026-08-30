pub mod winit;

use smithay::{input::keyboard::LedState, output::Output, reexports::{calloop::EventLoop, wayland_server::{Display, protocol::wl_surface}}};

use crate::{Alice, CalloopData};



pub trait Backend: Sized {
    const HAS_RELATIVE_MOTION: bool;
    const HAS_GESTURES: bool;

    fn setup(event_loop: &mut EventLoop<CalloopData<Self>>) -> Result<CalloopData<Self>, Box<dyn std::error::Error>>;
    fn seat_name(&self) -> String;
    fn reset_buffers(&mut self, output: &Output);
    fn early_import(&mut self, surface: &wl_surface::WlSurface);
    fn update_led_state(&mut self, led_state: LedState);
}

