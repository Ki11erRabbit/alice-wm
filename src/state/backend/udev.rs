use std::{collections::HashMap, os::unix::raw::dev_t, path::Path, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc, gbm::{GbmAllocator, GbmBufferFlags, GbmDevice}
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType, compositor::FrameFlags, exporter::gbm::GbmFramebufferExporter, output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements}
        },
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            damage::OutputDamageTracker,
            element::{AsRenderElements, surface::WaylandSurfaceRenderElement},
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, primary_gpu},
    },
    desktop::{Window, space::SpaceRenderElements},
    output::{Mode as WlMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{EventLoop, LoopHandle},
        drm::control::{ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::Display,
    },
    utils::DeviceFd,
};
use smithay::desktop::space::OutputError;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{state::backend::Backend, Alice, CalloopData};

// ---------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------

/// The renderer type used across the udev backend: a MultiRenderer that can
/// render on one GPU and export buffers for scanout on another.
type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

/// The concrete render element type produced when gathering a Space's
/// contents for a given output under the udev backend.
type UdevRenderElement<'a> =
    SpaceRenderElements<UdevRenderer<'a>, <Window as AsRenderElements<UdevRenderer<'a>>>::RenderElement>;

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
    /// GBM devices for GPUs that can render but have no display outputs of
    /// their own (e.g. Apple's AGX under Asahi). Display-only devices (e.g.
    /// DCP) borrow from here instead of opening a second fd on the same GPU.
    pub render_gbm_devices: HashMap<DrmNode, GbmDevice<DrmDeviceFd>>,
    pub keyboards: Vec<smithay::reexports::input::Device>,
}

/// A KMS-capable device whose `DrmDevice` is already open, but which is
/// waiting on a render-capable GPU to become available before it can finish
/// building its allocator/DrmOutputManager. Held onto rather than dropped and
/// reopened, since reopening the same device path in quick succession can
/// fail under libseat.
struct PendingKmsDevice {
    node: DrmNode,
    fd: DrmDeviceFd,
    drm_device: DrmDevice,
}

const SUPPORTED_FORMATS: &[Fourcc] = &[
    Fourcc::Argb2101010,
    Fourcc::Abgr2101010,
    Fourcc::Argb8888,
    Fourcc::Abgr8888,
];

// ---------------------------------------------------------------------
// Backend impl
// ---------------------------------------------------------------------

impl Backend for UdevData {
    const HAS_RELATIVE_MOTION: bool = true;
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
            render_gbm_devices: HashMap::new(),
            keyboards: Vec::with_capacity(1),
        };

        let display: Display<Alice<Self>> = Display::new()?;
        let display_handle = display.handle();
        let mut alice = Alice::new(backend_data, event_loop, display);

        let handle = event_loop.handle();

        // Single pass: open + classify every device exactly once. KMS
        // devices that can't finish yet get held (not dropped) in
        // `pending_kms`, then drained once every render-only GPU on the
        // system has had a chance to register.
        let udev_backend = UdevBackend::new(&seat_name)?;
        let mut pending_kms: Vec<PendingKmsDevice> = Vec::new();

        for (device_id, path) in udev_backend.device_list() {
            if let Err(err) = device_added(&mut alice, &handle, device_id, &path, &mut pending_kms) {
                eprintln!("Failed to add device {:?}: {}", device_id, err);
            }
        }

        for pending in pending_kms {
            let node = pending.node;
            match finish_kms_device(&mut alice, pending.node, pending.fd, pending.drm_device) {
                Ok(()) => device_changed(&mut alice, &handle, node),
                Err(err) => eprintln!("Failed to finish display device {:?}: {}", node, err),
            }
        }

        // A LoopHandle is cheaply Clone — capture our own clone into the
        // closure so it's usable for the lifetime of the callback without
        // needing to re-derive it from anything at call time.
        let udev_handle = handle.clone();
        event_loop.handle().insert_source(udev_backend, move |event, _, data| {
            let mut pending_kms: Vec<PendingKmsDevice> = Vec::new();
            match event {
                UdevEvent::Added { device_id, path } => {
                    if let Err(err) = device_added(&mut data.state, &udev_handle, device_id, &path, &mut pending_kms) {
                        eprintln!("Failed to add device {:?}: {}", device_id, err);
                    }
                    for pending in pending_kms {
                        let node = pending.node;
                        match finish_kms_device(&mut data.state, pending.node, pending.fd, pending.drm_device) {
                            Ok(()) => device_changed(&mut data.state, &udev_handle, node),
                            Err(err) => eprintln!("Failed to finish display device {:?}: {}", node, err),
                        }
                    }
                }
                UdevEvent::Changed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        device_changed(&mut data.state, &udev_handle, node);
                    }
                }
                UdevEvent::Removed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        device_removed(&mut data.state, node);
                    }
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

    fn schedule_render(alice: &mut Alice<Self>) {
        let targets: Vec<(DrmNode, crtc::Handle)> = alice
            .backend_data
            .backends
            .iter()
            .flat_map(|(node, backend)| backend.surfaces.keys().map(move |crtc| (*node, *crtc)))
            .collect();
        for (node, crtc) in targets {
            render_surface(alice, node, crtc);
        }
    }
}

// ---------------------------------------------------------------------
// Device lifecycle
// ---------------------------------------------------------------------

pub fn device_added(
    alice: &mut Alice<UdevData>,
    event_loop: &LoopHandle<'static, CalloopData<UdevData>>,
    device_id: dev_t,
    path: &Path,
    pending_kms: &mut Vec<PendingKmsDevice>,
) -> Result<(), Box<dyn std::error::Error>> {
    let node = DrmNode::from_dev_id(device_id)?;

    let fd = alice
        .backend_data
        .session
        .open(path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK)?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));

    match DrmDevice::new(fd.clone(), true) {
        Ok((drm_device, drm_notifier)) => {
            event_loop.insert_source(drm_notifier, move |event, meta, data| match event {
                DrmEvent::VBlank(crtc) => frame_finish(&mut data.state, node, crtc, meta),
                DrmEvent::Error(err) => eprintln!("DRM error on device {:?}: {}", node, err),
            })?;

            let render_node_ready = node.node_with_type(NodeType::Render).and_then(|r| r.ok()).is_some()
                || alice
                    .backend_data
                    .render_gbm_devices
                    .contains_key(&alice.backend_data.primary_gpu);

            if render_node_ready {
                finish_kms_device(alice, node, fd, drm_device)?;
                device_changed(alice, event_loop, node);
            } else {
                pending_kms.push(PendingKmsDevice { node, fd, drm_device });
            }
            Ok(())
        }
        Err(_) => {
            // No KMS capability — register as a render-only GPU (e.g. AGX).
            let gbm = GbmDevice::new(fd)?;
            let render_node = node
                .node_with_type(NodeType::Render)
                .and_then(|r| r.ok())
                .unwrap_or(node);

            if let Err(err) = alice.backend_data.gpus.as_mut().add_node(render_node, gbm.clone()) {
                eprintln!("Failed to add render-only node {:?}: {}", render_node, err);
            }
            alice.backend_data.render_gbm_devices.insert(render_node, gbm);
            eprintln!("Registered {:?} as a render-only GPU (no display outputs)", render_node);
            Ok(())
        }
    }
}

/// Finishes setting up a KMS device once a render-capable GPU is known to be
/// available — builds the allocator, registers with GpuManager, and
/// constructs the DrmOutputManager. Takes an already-open fd/DrmDevice;
/// never opens anything itself.
fn finish_kms_device(
    alice: &mut Alice<UdevData>,
    node: DrmNode,
    fd: DrmDeviceFd,
    drm_device: DrmDevice,
) -> Result<(), Box<dyn std::error::Error>> {
    // Always local to this device's own fd — required so AddFB2 targets
    // the right DRM device (the one that will actually scan out).
    let gbm = GbmDevice::new(fd)?;

    let own_render_node = node.node_with_type(NodeType::Render).and_then(|r| r.ok());
    let render_node = own_render_node.unwrap_or(alice.backend_data.primary_gpu);

    // Buffer allocation must happen on a render-capable device. If this
    // node can't render itself (e.g. DCP), borrow the cached render GBM
    // device (e.g. AGX) purely for allocation.
    let alloc_gbm = if let Some(_) = own_render_node {
        gbm.clone()
    } else {
        alice
            .backend_data
            .render_gbm_devices
            .get(&render_node)
            .ok_or("expected a cached render GPU by this point")?
            .clone()
    };
    let allocator = GbmAllocator::new(alloc_gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);

    // Only register this node with GpuManager if it's actually the
    // render-capable one — the render-only node was already registered
    // in device_added's fallback branch.
    if own_render_node.is_some() {
        if let Err(err) = alice.backend_data.gpus.as_mut().add_node(render_node, gbm.clone()) {
            eprintln!("Failed to add render node {:?}: {}", render_node, err);
        }
    }

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

    alice.backend_data.backends.insert(
        node,
        GpuBackendData {
            drm_output_manager,
            drm_scanner: DrmScanner::new(),
            render_node: Some(render_node),
            surfaces: HashMap::new(),
        },
    );

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
                connector_connected(alice, event_loop, node, connector, crtc);
            }
            DrmScanEvent::Disconnected { connector, crtc: Some(crtc) } => {
                connector_disconnected(alice, node, connector, crtc);
            }
            _ => {}
        }
    }
}

fn connector_connected(
    alice: &mut Alice<UdevData>,
    event_loop: &LoopHandle<'static, CalloopData<UdevData>>,
    node: DrmNode,
    connector: connector::Info,
    crtc: crtc::Handle) {
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

    match backend.drm_output_manager.initialize_output(
        crtc,
        drm_mode,
        &[connector.handle()],
        &output,
        None,
        &mut renderer,
        &DrmOutputRenderElements::<UdevRenderer<'_>, UdevRenderElement<'_>>::default(),
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
            drop(renderer);
            // Defer the first frame instead of rendering inline: give the
            // event loop a chance to dispatch the modeset commit's
            // completion event first, so the CRTC isn't still "busy" when
            // we submit the first real page flip.
            event_loop.insert_idle(move |data| {
                render_surface(&mut data.state, node, crtc);
            });
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
    if let Some(backend) = alice.backend_data.backends.remove(&node) {
        for (_, surface) in backend.surfaces {
            alice.outputs.deactivate(&surface.output.name());
            alice.space.unmap_output(&surface.output);
        }
        if let Some(render_node) = backend.render_node {
            alice.backend_data.gpus.as_mut().remove_node(&render_node);
        }
    }

    // If this was a render-only GPU (e.g. AGX unplugged, external eGPU, etc.)
    if alice.backend_data.render_gbm_devices.remove(&node).is_some() {
        alice.backend_data.gpus.as_mut().remove_node(&node);
    }
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

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

    let elements: Vec<UdevRenderElement<'_>> =
        match alice.space.render_elements_for_output(&mut renderer, &output, 1.0) {
            Ok(elements) => elements,
            Err(OutputError::Unmapped) => return,
            Err(err) => {
                eprintln!("Failed to gather render elements for {:?}: {:?}", output.name(), err);
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


