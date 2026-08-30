#![allow(irrefutable_let_patterns)]

mod handlers;

mod grabs;
mod input;
mod state;
//mod winit;
mod layout;
mod output;
mod window;
mod config;
mod layer;

use smithay::{backend::{drm::{DrmNode, NodeType}, input::{DeviceCapability, InputEvent}, libinput::{LibinputInputBackend, LibinputSessionInterface}, session::{Session, libseat::LibSeatSession}, udev::{UdevBackend, primary_gpu}}, output::{Mode, Output, PhysicalProperties, Scale, Subpixel}, reexports::{
    calloop::EventLoop, input::Libinput, wayland_server::{Display, DisplayHandle}
}, utils::Transform};
pub use state::Alice;

use crate::state::backend::{Backend, udev::UdevData, winit::WinitData};

pub struct CalloopData<BackendData: Backend + 'static> {
    state: Alice<BackendData>,
    display_handle: DisplayHandle,
}


fn spawn_loop() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
        let mut event_loop: EventLoop<'static, CalloopData<WinitData>> = EventLoop::try_new()?;
        let mut data = WinitData::setup(&mut event_loop)?;

        let mut args = std::env::args().skip(1);
        let flag = args.next();
        let arg = args.next();

        match (flag.as_deref(), arg) {
            (Some("-c") | Some("--command"), Some(command)) => {
                std::process::Command::new(command).spawn().ok();
            }
            _ => {
                std::process::Command::new("weston-terminal").spawn().ok();
            }
        }

        event_loop.run(None, &mut data, move |_| {
            // Smallvil is running
        })?;
    } else {
        let mut event_loop: EventLoop<'static, CalloopData<UdevData>> = EventLoop::try_new()?;
        let mut data = UdevData::setup(&mut event_loop)?;

        let mut args = std::env::args().skip(1);
        let flag = args.next();
        let arg = args.next();

        match (flag.as_deref(), arg) {
            (Some("-c") | Some("--command"), Some(command)) => {
                std::process::Command::new(command).spawn().ok();
            }
            _ => {
                std::process::Command::new("weston-terminal").spawn().ok();
            }
        }

        event_loop.run(None, &mut data, move |_| {
            // Smallvil is running
            let _ = data.display_handle.flush_clients();
        })?;
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }



    spawn_loop()?;

    Ok(())
}
