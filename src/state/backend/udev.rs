use std::{collections::HashMap, os::unix::raw::dev_t, path::Path, time::Duration};

use smithay::{
    backend::{
        allocator::{
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Fourcc,
        },
        drm::{
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType,
        },
        input::{InputEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
            multigpu::{gbm::GbmGlesBackend, GpuManager},
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{primary_gpu, UdevBackend, UdevEvent},
    },
    output::{Mode as WlMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{EventLoop, LoopHandle},
        drm::control::{connector, crtc, ModeTypeFlags},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::Display,
    },
    utils::DeviceFd,
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{state::backend::Backend, Alice, CalloopData};

type UdevRenderer<'a> = smithay::backend::renderer::multigpu::MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

pub struct GpuBackendData {
    pub drm_output_manager:
        DrmOutputManager<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>,
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
    const HAS_RELATIVE_MOTION: bool = false;
    const HAS_GESTURES: bool = false;

    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn setup(
        event_loop: &mut EventLoop<'static, crate::CalloopData<Self>>,
    ) -> Result<crate::CalloopData<Self>, Box<dyn std::error::Error>> {
        let (session, notifier) = LibSeatSession::new()?;
        let seat_name = session.seat();

        let primary_gpu = primary_gpu(&seat_name)?
            .and_then(|x| DrmNode::from_path(x).ok()?.node_with_type(NodeType::Render)?.ok())
            .ok_or(String::from("No GPU!"))?;

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
        let mut alice = Alice::new(backend_data, event_loop, display);

        // Need the handle before the initial device scan, since device_added takes it.
        let handle = event_loop.handle();

        let udev_backend = UdevBackend::new(&seat_name)?;
        for (device_id, path) in udev_backend.device_list() {
            if let Err(err) = device_added(&mut alice, &handle, device_id, &path) {
                eprintln!("Failed to add device {:?}: {}", device_id, err);
            }
        }

        event_loop
            .handle()
            .insert_source(udev_backend, move |event, _, data| match event {
                UdevEvent::Added { device_id, path } => {
                    if let Err(err) = device_added(&mut data.state, &handle, device_id, &path) {
                        eprintln!("Failed to add device {:?}: {}", device_id, err);
                    }
                }
                UdevEvent::Changed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        device_changed(&mut data.state, &handle, node);
                    }
                }
                UdevEvent::Removed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        device_removed(&mut data.state, node);
                    }
                }
            })?;

        let mut libinput_context = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
            session.clone().into(),
        );
        libinput_context.udev_assign_seat(&seat_name).unwrap();
        let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

        event_loop
            .handle()
            .insert_source(libinput_backend, move |mut event, _, state| {
                let dh = state.display_handle.clone();

                if let InputEvent::DeviceAdded { device } = &mut event {
                    if device.has_capability(smithay::reexports::input::DeviceCapability::Keyboard) {
                        if let Some(led_state) = state.state.seat.get_keyboard().map(|kb| kb.led_state()) {
                            device.led_update(led_state.into());
                        }
                        state.state.backend_data.keyboards.push(device.clone());
                    }
                } else if let InputEvent::DeviceRemoved { ref device } = event {
                    if device.has_capability(smithay::reexports::input::DeviceCapability::Keyboard) {
                        state.state.backend_data.keyboards.retain(|d| d != device);
                    }
                }
                state.state.process_input_event(event);
            })?;

        event_loop
            .handle()
            .insert_source(notifier, move |event, &mut (), state| match event {
                SessionEvent::PauseSession => {
                    libinput_context.suspend();
                    for backend in state.state.backend_data.backends.values_mut() {
                        backend.drm_output_manager.pause();
                    }
                }
                SessionEvent::ActivateSession => {
                    if let Err(err) = libinput_context.resume() {
                        eprintln!("Failed to resume libinput context: {:?}", err);
                    }
                    for (_node, backend) in state.state.backend_data.backends.iter_mut() {
                        backend
                            .drm_output_manager
                            .activate(false)
                            .expect("failed to activate drm backend");
                    }
                }
            })?;

        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", &alice.socket_name);
        }

        Ok(CalloopData { state: alice, display_handle })
    }

    fn reset_buffers(&mut self, _output: &Output) {}
    fn early_import(&mut self, _surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {}
    fn update_led_state(&mut self, _led_state: smithay::input::keyboard::LedState) {}
}

const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Argb2101010,
    Fourcc::Abgr2101010,
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
];

pub fn device_added(
    alice: &mut Alice<UdevData>,
    event_loop: &LoopHandle<'static, CalloopData<UdevData>>,
    device_id: dev_t,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let node = DrmNode::from_dev_id(device_id)?;

    let fd = alice
        .backend_data
        .session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK)?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (drm_device, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
    let gbm = GbmDevice::new(fd)?;
    let allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING);

    let render_node = node
        .node_with_type(NodeType::Render)
        .and_then(|r| r.ok())
        .unwrap_or(node);

    if let Err(err) = alice.backend_data.gpus.as_mut().add_node(render_node, gbm.clone()) {
        eprintln!("Failed to add render node {:?}: {}", render_node, err);
    }

    // NOTE: this is the one spot I'd double check against docs.rs for your exact
    // smithay 0.7.0 build — `.shm_formats()` is wrong (that's for wl_shm imports,
    // not scanout), swapped for the renderer's dmabuf/scanout-capable formats.
    let renderer_formats = alice
        .backend_data
        .gpus
        .single_renderer(&render_node)?
        .as_mut()
        .egl_context()
        .dmabuf_render_formats()
        .clone();

    let drm_output_manager = DrmOutputManager::new(
        drm_device,
        allocator,
        GbmFramebufferExporter::new(gbm.clone(), render_node.into()),
        Some(gbm),
        SUPPORTED_FORMATS.iter().copied(),
        renderer_formats,
    );

    event_loop.insert_source(drm_notifier, move |event, meta, data| match event {
        DrmEvent::VBlank(crtc) => {
            frame_finish(&mut data.state, node, crtc, meta);
        }
        DrmEvent::Error(err) => {
            eprintln!("DRM error on device {:?}: {}", node, err);
        }
    })?;

    alice.backend_data.backends.insert(
        node,
        GpuBackendData {
            drm_output_manager,
            drm_scanner: DrmScanner::new(),
            render_node: Some(render_node),
            surfaces: HashMap::new(),
        },
    );

    device_changed(alice, event_loop, node);

    Ok(())
}

pub fn device_changed(alice: &mut Alice<UdevData>, event_loop: &LoopHandle<'static, CalloopData<UdevData>>, node: DrmNode) {
    let Some(backend) = alice.backend_data.backends.get_mut(&node) else {
        return;
    };

    let drm_device = backend.drm_output_manager.device();
    let scan_events: Vec<_> = backend
        .drm_scanner
        .scan_connectors(drm_device)
        .unwrap()
        .into_iter()
        .collect();

    for event in scan_events {
        match event {
            DrmScanEvent::Connected { connector, crtc: Some(crtc) } => {
                connector_connected(alice, node, connector, crtc);
            }
            DrmScanEvent::Disconnected { connector, crtc: Some(crtc) } => {
                connector_disconnected(alice, node, connector, crtc);
            }
            _ => {}
        }
    }
    let _ = event_loop; // reserved for future use (e.g. re-registering per-output sources)
}

fn connector_connected(alice: &mut Alice<UdevData>, node: DrmNode, connector: connector::Info, crtc: crtc::Handle) {
    let mode = connector
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
        .cloned();

    let Some(drm_mode) = mode else {
        eprintln!("No mode available for connector {:?}", connector.interface());
        return;
    };

    let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
    let (w, h) = drm_mode.size();
    let refresh = drm_mode.vrefresh() as i32 * 1000;

    let output = Output::new(
        name.clone(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Unknown".into(),
            model: "Unknown".into(),
        },
    );

    output.change_current_state(
        Some(WlMode { size: (w as i32, h as i32).into(), refresh }),
        None,
        None,
        None,
    );
    output.set_preferred(WlMode { size: (w as i32, h as i32).into(), refresh });

    let x_offset: i32 = alice
        .outputs
        .iter()
        .filter_map(|info| alice.space.output_geometry(&info.output))
        .map(|geo| geo.size.w)
        .sum();

    alice.space.map_output(&output, (x_offset, 0));
    output.create_global::<Alice<UdevData>>(&alice.display_handle);
    alice.outputs.insert(output.clone());
    output.user_data().insert_if_missing(|| (node, crtc));

    let Some(backend) = alice.backend_data.backends.get_mut(&node) else {
        return;
    };
    let Some(render_node) = backend.render_node else {
        return;
    };
    let Ok(mut renderer) = alice.backend_data.gpus.single_renderer(&render_node) else {
        return;
    };
    let empty_elements: &[WaylandSurfaceRenderElement<UdevRenderer<'_>>] = &[];
    match backend.drm_output_manager.initialize_output(
        crtc,
        drm_mode,
        &[connector.handle()],
        &output,
        None,
        &mut renderer,
        empty_elements,
    ) {
        Ok(drm_output) => {
            backend.surfaces.insert(
                crtc,
                SurfaceData {
                    drm_output,
                    damage_tracker: OutputDamageTracker::from_output(&output),
                    output: output.clone(),
                },
            );
        }
        Err(err) => {
            eprintln!("Failed to initialize output on crtc {:?}: {}", crtc, err);
        }
    }
}

fn connector_disconnected(alice: &mut Alice<UdevData>, node: DrmNode, _connector: connector::Info, crtc: crtc::Handle) {
    let Some(backend) = alice.backend_data.backends.get_mut(&node) else {
        return;
    };
    let Some(surface) = backend.surfaces.remove(&crtc) else {
        return;
    };

    alice.outputs.deactivate(&surface.output.name());
    alice.space.unmap_output(&surface.output);
}

pub fn device_removed(alice: &mut Alice<UdevData>, node: DrmNode) {
    let Some(backend) = alice.backend_data.backends.remove(&node) else {
        return;
    };

    for (_, surface) in backend.surfaces {
        alice.outputs.deactivate(&surface.output.name());
        alice.space.unmap_output(&surface.output);
    }

    if let Some(render_node) = backend.render_node {
        alice.backend_data.gpus.as_mut().remove_node(&render_node);
    }
}

pub fn frame_finish(
    alice: &mut Alice<UdevData>,
    node: DrmNode,
    crtc: crtc::Handle,
    _metadata: &mut Option<DrmEventMetadata>,
) {
    let Some(backend) = alice.backend_data.backends.get_mut(&node) else {
        return;
    };
    let Some(surface) = backend.surfaces.get_mut(&crtc) else {
        return;
    };

    if let Err(err) = surface.drm_output.frame_submitted() {
        eprintln!("frame_submitted failed for crtc {:?}: {}", crtc, err);
        return;
    }

    render_surface(alice, node, crtc);
}

fn render_surface(alice: &mut Alice<UdevData>, node: DrmNode, crtc: crtc::Handle) {
    let Some(render_node) = alice.backend_data.backends.get(&node).and_then(|b| b.render_node) else {
        return;
    };

    let mut renderer = match alice.backend_data.gpus.single_renderer(&render_node) {
        Ok(r) => r,
        Err(err) => {
            eprintln!("Failed to acquire renderer for {:?}: {}", render_node, err);
            return;
        }
    };

    let Some(backend) = alice.backend_data.backends.get_mut(&node) else {
        return;
    };
    let Some(surface) = backend.surfaces.get_mut(&crtc) else {
        return;
    };
    let output = surface.output.clone();

    let render_result = smithay::desktop::space::render_output::<
        _,
        WaylandSurfaceRenderElement<_>,
        _,
        _,
    >(
        &output,
        &mut renderer,
        1.0,
        0,
        [&alice.space],
        &[],
        &mut surface.damage_tracker,
        [0.1, 0.1, 0.1, 1.0],
    );

    let elements = match render_result {
        Ok(res) => res,
        Err(err) => {
            eprintln!("Render error on crtc {:?}: {:?}", crtc, err);
            return;
        }
    };

    match surface
        .drm_output
        .render_frame(&mut renderer, &elements, [0.1, 0.1, 0.1, 1.0], FrameFlags::DEFAULT)
    {
        Ok(res) if !res.is_empty => {
            if let Err(err) = surface.drm_output.queue_frame(()) {
                eprintln!("Failed to queue frame on crtc {:?}: {}", crtc, err);
            }
        }
        Ok(_) => {}
        Err(err) => {
            eprintln!("render_frame failed on crtc {:?}: {:?}", crtc, err);
        }
    }

    alice.space.elements().for_each(|window| {
        window.send_frame(&output, alice.start_time.elapsed(), Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        })
    });
    if let Some(id) = alice.outputs.get(&output.name()).map(|info| info.id) {
        if let Some(layers) = alice.layer_surfaces.get(&id) {
            for layer in layers {
                layer.surface.send_frame(&output, alice.start_time.elapsed(), Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            }
        }
    }

    alice.space.refresh();
    alice.popups.cleanup();
    let _ = alice.display_handle.flush_clients();
}
