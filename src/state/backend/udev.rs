use std::{collections::HashMap, os::unix::raw::dev_t, path::Path};

use smithay::{backend::{allocator::gbm::GbmAllocator, drm::{DrmDevice, DrmDeviceFd, DrmNode, NodeType, exporter::gbm::GbmFramebufferExporter, output::{DrmOutput, DrmOutputManager}}, input::{DeviceCapability, InputEvent}, libinput::{LibinputInputBackend, LibinputSessionInterface}, renderer::{damage::OutputDamageTracker, gles::GlesRenderer, multigpu::{GpuManager, gbm::{GbmGlesBackend, GbmGlesDevice}}}, session::{Session, libseat::LibSeatSession}, udev::{UdevBackend, UdevEvent, primary_gpu}}, output::Output, reexports::{calloop::{EventLoop, LoopHandle}, drm::control::crtc, input::Libinput, rustix::fs::OFlags, wayland_server::Display}, utils::DeviceFd};
use smithay_drm_extras::drm_scanner::DrmScanner;
use smithay::backend::session::Event as SessionEvent;

use crate::{Alice, CalloopData, state::backend::Backend};


pub struct GpuBackendData {
    pub drm_output_manager: DrmOutputManager<GbmAllocator<DrmDeviceFd>>,
    pub drm_scanner: DrmScanner,
    pub render_node: Option<DrmNode>,
    pub surfaces: HashMap<crtc::Handle, SurfaceData>,
}

pub struct SurfaceData {
    pub drm_output: DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>,
    pub damage_tracker: OutputDamageTracker,
    pub output: Output,
}

pub struct UdevData {
    pub session: LibSeatSession,
    pub primary_gpu: DrmNode,
    pub gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    pub backends: HashMap<DrmNode, GpuBackendData>,
    pub keyboards: Vec<smithay::reexports::input::Device>,
}

impl Backend for UdevData {
    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn setup(event_loop: &mut EventLoop<crate::CalloopData<Self>>) -> Result<crate::CalloopData<Self>, Box<dyn std::error::Error>> {

        let (session, notifier) = LibSeatSession::new()?;
        let seat_name = session.seat();

        let primary_gpu = primary_gpu(&seat_name)?
            .and_then(|x| DrmNode::from_path(x).ok()?.node_with_type(NodeType::Render)?.ok())
            .ok_or(String::from("No GPU!"))?

        let gpus = GpuManager::new(GbmGlesBackend::default())?;

        let backend_data = UdevData {
            session: session.clone(),
            primary_gpu,
            gpus,
            backends: HashMap::new(),
            keyboards: Vec::with_capacity(1),
        };

        let display: Display<Alice<Self>> = Display::new()?;
        let display_handle = display.handle();
        let mut alice = Alice::new(backend_data, event_loop, display)?;

        let udev_backend = UdevBackend::new(&seat_name)?;
        for (device_id, path) in udev_backend.device_list() {
            device_added(&mut alice, device_id, &path)?;
        }

        event_loop.handle().insert_source(udev_backend, move |event, _, data| {
            match event {
                UdevEvent::Added { device_id, path } => device_added(&mut data.state, device_id, &path),
                UdevEvent::Changed { device_id } => device_changed(&mut data.state, device_id),
                UdevEvent::Removed { device_id } => device_removed(&mut data.state. device_id),
            }
        })?;


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
                        state.state.backend_data.keyboards.push(device.clone());

                    }
                } else if let InputEvent::DeviceRemoved { ref device } = event {
                    if device.has_capability(DeviceCapability::Keyboard) {
                        state.state.backend_data.keyboards.retain(|d| d != device);
                    }

                }
                state.state.process_input_event(&dh, event);
            })?;

        event_loop.handle()
            .insert_source(notifier, move |event, &mut (), state| match event {
                SessionEvent::PauseSession => {
                    libinput_context.suspend();

                    for backend in state.state.backend_data.backends.values_mut() {
                        backend.drm_output_manager.pause();;
                    }
                }
                SessionEvent::ActivateSession => {
                    if let Err(err) = libinput_context.resume() {
                        eprintln!("Failed to resume libinput context: {:?}, err");
                    }

                    for (node, backend) in state.state.backend_data.backends.iter_mut() {
                        backend.drm_output_manager
                            .activate(false)
                            .expect("failed to activate drm backend");




                    }
                }
            })?;


        unsafe { std::env::set_var("WAYLAND_DISPLAY", &alice.socket_name); }

        Ok(CalloopData { state: alice, display_handle })
    }

    fn reset_buffers(&mut self, output: &smithay::output::Output) {

    }

    fn early_import(&mut self, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {

    }

    fn update_led_state(&mut self, led_state: smithay::input::keyboard::LedState) {

    }
}


