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

use crate::state::backend::{Backend, winit::WinitData};

pub struct CalloopData<BackendData: Backend + 'static> {
    state: Alice<BackendData>,
    display_handle: DisplayHandle,
}


fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut event_loop: EventLoop<'static, CalloopData<WinitData>> = EventLoop::try_new()?;
    /*
    let (session, notifier) = LibSeatSession::new()?;

    event_loop.handle()
        .insert_source(notifier, move |event, &mut (), state| match event {
            SessionEvent::PauseSession => {

            }
            SessionEvent::ActivateSession => {
            }
        })?;

    let seat_name = session.seat();

    let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
        session.clone().into(),
    );

    libinput_context.udev_assign_seat(&seat_name).unwrap();

    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    event_loop.handle()
        .insert_source(libinput_backend, move |mut event, _, state| {
            let dh = state.display_handle.clone();

            if let InputEvent::DeviceAdded { device } = &mut event {
                if device.has_capability(DeviceCapability::Keyboard) {
                    if let Some(led_state) = state.state.seat.get_keyboard()
                        .map(|kb| kb.led_state()) {
                        device.led_update(led_state.into());
                    }

                }
            } else if let InputEvent::DeviceRemoved { ref device } = event {
                if device.has_capability(DeviceCapability::Keyboard) {
                }

            }
            state.state.process_input_event(&dh, event);
        })?;


    let primary_gpu = primary_gpu(&seat_name)?
        .and_then(|x| DrmNode::from_path(x).ok()?.node_with_type(NodeType::Render)?.ok())
        .ok_or(String::from("No GPU!"))?

    let udev_backend = UdevBackend::new(&seat_name)?;


    let display: Display<Alice> = Display::new()?;
    let display_handle = display.handle();
    let mut state = Alice::new(&mut event_loop, display);

    let mut data = CalloopData {
        state,
        display_handle,
    };

    crate::winit::init_winit(&mut event_loop, &mut data, output)?;
    */

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

    Ok(())
}
