use std::{collections::HashMap, os::unix::raw::dev_t, path::Path, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc, dmabuf::Dmabuf, gbm::{GbmAllocator, GbmBufferFlags, GbmDevice}
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmNode, NodeType, compositor::FrameFlags, exporter::gbm::GbmFramebufferExporter, output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements}
        },
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            Bind, ExportMem, Frame, ImportDma, Offscreen, Renderer,
            damage::OutputDamageTracker,
            element::{AsRenderElements, Element, RenderElement, surface::WaylandSurfaceRenderElement},
            gles::GlesRenderer,
            multigpu::{GpuManager, MultiRenderer, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, primary_gpu},
    },
    desktop::{Space, Window, layer_map_for_output, space::{SpaceRenderElements, space_render_elements}},
    output::{Mode as WlMode, Output, OutputNoMode, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::{EventLoop, LoopHandle},
        drm::control::{ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
        wayland_server::{Display, backend::GlobalId, protocol::wl_buffer::WlBuffer},
    },
    // NB: deliberately NOT importing `smithay::utils::Scale` here — `Scale`
    // above (from `smithay::output`) is a different type used for output
    // scale factors. Rendering code below refers to the utils one via its
    // full path (`smithay::utils::Scale::from(...)`) to avoid the collision.
    utils::{DeviceFd, Physical, Point, Rectangle, Transform},
    wayland::{
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        shell::wlr_layer::Layer,
        shm::with_buffer_contents_mut,
    },
};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use crate::{Alice, CalloopData, config::Config, output::{LayoutScope, Outputs}, state::backend::Backend};

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

/// Everything actually handed to the DRM compositor for a frame: the
/// space's contents plus the software cursor drawn on top.
smithay::backend::renderer::element::render_elements! {
    UdevFrameRenderElement<='a, UdevRenderer<'a>>;
    Space=UdevRenderElement<'a>,
    Cursor=crate::cursor::PointerRenderElement<UdevRenderer<'a>>,
}

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
    pub frame_pending: bool,
}

pub struct UdevData {
    pub session: LibSeatSession,
    pub primary_gpu: DrmNode,
    pub gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    pub backends: HashMap<DrmNode, GpuBackendData>,
    pub render_gbm_devices: HashMap<DrmNode, GbmDevice<DrmDeviceFd>>,
    pub keyboards: Vec<smithay::reexports::input::Device>,
    pub dmabuf_state: Option<(DmabufState, DmabufGlobal)>,
    pub pointer_element: crate::cursor::PointerElement,
    /// `None` until the screencopy global is registered. This can't happen
    /// inside `UdevData`'s own construction (it needs a `DisplayHandle`,
    /// which doesn't exist yet at that point) and, more importantly,
    /// shouldn't happen until `Alice<UdevData>` actually implements
    /// `GlobalDispatch<ZwlrScreencopyManagerV1, ()>` — see the TODO in
    /// `setup()` below for where to wire it up once that dispatch code
    /// exists.
    pub screencopy_global: Option<GlobalId>,
}

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
            dmabuf_state: None,
            pointer_element: crate::cursor::PointerElement::default(),
            screencopy_global: None,
        };

        let display: Display<Alice<Self>> = Display::new()?;
        let display_handle = display.handle();
        let mut alice = Alice::new(backend_data, event_loop, display);

        let handle = event_loop.handle();

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
        let dmabuf_formats = alice
            .backend_data
            .gpus
            .single_renderer(&alice.backend_data.primary_gpu)?
            .dmabuf_formats();
        let default_feedback = DmabufFeedbackBuilder::new(alice.backend_data.primary_gpu.dev_id(), dmabuf_formats)
            .build()
            .unwrap();
        let mut dmabuf_state = DmabufState::new();
        let global = dmabuf_state
            .create_global_with_default_feedback::<Alice<UdevData>>(&display_handle, &default_feedback);
        alice.backend_data.dmabuf_state = Some((dmabuf_state, global));

        // TODO(screencopy): once `Alice<UdevData>` implements
        // `GlobalDispatch<ZwlrScreencopyManagerV1, ()>` (the dispatch code
        // you're adding separately), register the global here:
        //
        //   use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
        //   let screencopy_global = display_handle
        //       .create_global::<Alice<UdevData>, ZwlrScreencopyManagerV1, _>(3, ());
        //   alice.backend_data.screencopy_global = Some(screencopy_global);

        let udev_handle = handle.clone();
        event_loop.handle().insert_source(udev_backend, move |event, _, data| {
            let mut pending_kms: Vec<PendingKmsDevice> = Vec::new();
            match event {
                UdevEvent::Added { device_id, path } => {
                    if let Err(err) = device_added(&mut data.state, &udev_handle, device_id, &path, &mut pending_kms) {
                        //eprintln!("Failed to add device {:?}: {}", device_id, err);
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

        Ok(CalloopData { state: alice, display_handle })
    }

    fn reset_buffers(&mut self, _output: &Output) {}
    fn early_import(&mut self, _surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) {}
    fn update_led_state(&mut self, led_state: smithay::input::keyboard::LedState) {
        for keyboard in self.keyboards.iter_mut() {
            keyboard.led_update(led_state.into());
        }

    }

    fn schedule_render(alice: &mut Alice<Self>) {
        let targets: Vec<(DrmNode, crtc::Handle)> = alice
            .backend_data
            .backends
            .iter()
            .flat_map(|(node, backend)| {
                backend
                    .surfaces
                    .iter()
                    .filter(|(_, surface)| !surface.frame_pending)
                    .map(|(crtc, _)| (*node, *crtc))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (node, crtc) in targets {
            render_surface(alice, node, crtc);
        }
    }

    fn make_config() -> Config {
        match crate::config::execute_lua_config(false) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("Error while loading config: {err}");
                Config::default()
            }
        }
    }

    fn screencopy_id(&mut self) -> GlobalId {
        self.screencopy_global
            .clone()
            .expect("screencopy_id called before the screencopy global was registered")
    }

    fn output_physical_size(&self, output: &Output) -> (i32, i32) {
        let mode = output.current_mode().expect("queried size of output with no current mode");
        (mode.size.w, mode.size.h)
    }

    // NOTE: this deliberately does NOT take `&mut self` alongside `alice` —
    // `self`/`backend_data` is a FIELD of `Alice<Self>`, not a sibling of
    // it, so a caller could never legally hold `&mut self` and `&mut
    // Alice<Self>` at once (they'd overlap). Same shape as
    // `schedule_render` above: take `alice` alone, reach backend-owned
    // state via `alice.backend_data` inside the body.
    fn copy_frame(
        alice: &mut crate::Alice<Self>,
        output: &Output,
        region: Option<Rectangle<i32, Physical>>,
        overlay_cursor: bool,
        buffer: &WlBuffer,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let &(node, _crtc) = output
            .user_data()
            .get::<(DrmNode, crtc::Handle)>()
            .ok_or("output has no associated (DrmNode, crtc::Handle)")?;

        let render_node = alice
            .backend_data
            .backends
            .get(&node)
            .and_then(|b| b.render_node)
            .ok_or("no render node for this output's device")?;

        let mut renderer = alice
            .backend_data
            .gpus
            .single_renderer(&render_node)
            .map_err(|e| format!("failed to acquire renderer: {e}"))?;

        let scope = output_scope(&alice.outputs, output).ok_or("no LayoutScope for output")?;

        let space_elements =
            output_space_elements(&mut renderer, &alice.space, &alice.window_registry, output, scope)
                .map_err(|e| format!("failed to gather render elements: {e:?}"))?;

        let mut elements: Vec<UdevFrameRenderElement<'_>> =
            space_elements.into_iter().map(UdevFrameRenderElement::Space).collect();

        if overlay_cursor {
            let output_geo = alice
                .space
                .output_geometry(output)
                .ok_or("output has no geometry in space")?;
            let output_scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
            let cursor_pos = alice
                .seat
                .get_pointer()
                .ok_or("seat has no pointer")?
                .current_location()
                - output_geo.loc.to_f64();
            let cursor_elements = crate::cursor::cursor_render_elements(
                &mut alice.backend_data.pointer_element,
                &alice.cursor_status,
                &mut renderer,
                cursor_pos,
                output_scale,
            );
            elements.splice(0..0, cursor_elements.into_iter().map(UdevFrameRenderElement::Cursor));
        }

        // Capture rectangle. This backend actually deals in TWO differently
        // "kinded" but numerically-identical sizes here — `render`/`clear`/
        // element `draw` want `Physical`-kind, while `create_buffer` and
        // `copy_framebuffer` want `Buffer`-kind (confirmed against this
        // Smithay version's real signatures via the compiler, not guessed).
        // These kinds are phantom types Smithay uses to stop coordinate
        // spaces from being mixed up by accident; since no buffer scaling
        // is applied here, the numeric values are the same either way — we
        // just need both differently-typed handles to satisfy each call.
        let mode = output.current_mode().ok_or("output has no current mode")?;
        let capture_size = region.map(|r| r.size).unwrap_or(mode.size);
        let capture_size_buf: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (capture_size.w, capture_size.h).into();
        let capture_loc: Point<i32, Physical> = region.map(|r| r.loc).unwrap_or((0, 0).into());

        // --- render into an offscreen target ---
        //
        // `GlesTexture` — not a "Multi"-prefixed type. The compiler's error
        // showed `GlesRenderer` (the concrete backend `MultiRenderer` wraps
        // here) implementing `Offscreen<GlesTexture>` and
        // `Offscreen<GlesRenderbuffer>` directly; `MultiRenderer` forwards
        // through to whichever of those the inner renderer supports, rather
        // than defining its own separate offscreen target type. Also:
        // `bind()`'s real signature is `fn bind<'a>(&mut self, target: &'a
        // mut Target) -> ...` — it takes `&mut Target`, not an owned
        // `Target`, hence `target` needs to be `mut` and passed as
        // `&mut target` below.
        let mut target: smithay::backend::renderer::gles::GlesTexture = renderer
            .create_buffer(Fourcc::Argb8888, capture_size_buf)
            .map_err(|e| format!("failed to create offscreen buffer: {e}"))?;

        let mut fb = renderer
            .bind(&mut target)
            .map_err(|e| format!("failed to bind offscreen target: {e}"))?;

        let mut frame = renderer
            .render(&mut fb, capture_size, Transform::Normal)
            .map_err(|e| format!("failed to start frame: {e}"))?;
        frame
            .clear([0.0, 0.0, 0.0, 1.0].into(), &[Rectangle::from_size(capture_size)])
            .map_err(|e| format!("failed to clear frame: {e}"))?;

        // Elements are gathered top-most-first (cursor first, then space
        // elements); draw back-to-front, so iterate in reverse.
        for element in elements.iter().rev() {
            let src = element.src();
            let mut dst = element.geometry(smithay::utils::Scale::from(1.0));
            dst.loc -= capture_loc; // shift so a cropped region lands at (0, 0)
            let damage = [Rectangle::from_size(dst.size)];
            if let Err(err) = element.draw(&mut frame, src, dst, &damage, &[]) {
                eprintln!("screencopy: failed to draw element for {:?}: {:?}", output.name(), err);
            }
        }
        let _sync_point = frame.finish().map_err(|e| format!("failed to finish frame: {e}"))?;
        // _sync_point.wait(); // uncomment if readback below ever shows
        // torn/stale content — some versions require an explicit wait
        // before the framebuffer is readback-safe.

        // --- read back and copy into the client's shm buffer ---
        let mapping = renderer
            .copy_framebuffer(&fb, Rectangle::from_size(capture_size_buf), Fourcc::Argb8888)
            .map_err(|e| format!("failed to read back framebuffer: {e}"))?;
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("failed to map readback texture: {e}"))?;

        with_buffer_contents_mut(buffer, |ptr, _len, data| {
            let dst_stride = data.stride as usize;
            let src_stride = capture_size.w as usize * 4;
            let copy_len = src_stride.min(dst_stride);
            for row in 0..capture_size.h as usize {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(row * src_stride),
                        ptr.add(row * dst_stride),
                        copy_len,
                    );
                }
            }
        })
        .map_err(|e| format!("failed to write into client shm buffer: {e:?}"))?;

        Ok(())
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

fn finish_kms_device(
    alice: &mut Alice<UdevData>,
    node: DrmNode,
    fd: DrmDeviceFd,
    drm_device: DrmDevice,
) -> Result<(), Box<dyn std::error::Error>> {
    let gbm = GbmDevice::new(fd)?;

    let own_render_node = node.node_with_type(NodeType::Render).and_then(|r| r.ok());
    let render_node = own_render_node.unwrap_or(alice.backend_data.primary_gpu);

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
    let name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());

    let output_cfg = alice.config.get_output_position(&name);

    let modes = connector.modes();
    let preferred_size = modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| modes.first())
        .map(|m| m.size());

    let mode = match output_cfg.and_then(|cfg| cfg.refresh) {
        Some(target_hz) => modes
            .iter()
            .filter(|m| preferred_size.map_or(true, |size| m.size() == size))
            .min_by(|a, b| {
                let da = (a.vrefresh() as f64 - target_hz).abs();
                let db = (b.vrefresh() as f64 - target_hz).abs();
                da.total_cmp(&db)
            })
            .or_else(|| modes.iter().find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)))
            .or_else(|| modes.first())
            .cloned(),
        None => modes
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| modes.first())
            .cloned(),
    };

    let Some(drm_mode) = mode else {
        eprintln!("No mode available for connector {:?}", connector.interface());
        return;
    };

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

    let transform = output_cfg.map(|cfg| cfg.transform).unwrap_or(Transform::Normal);
    let scale = output_cfg.and_then(|cfg| cfg.scale).map(Scale::Fractional);

    output.change_current_state(
        Some(WlMode { size: (w as i32, h as i32).into(), refresh }),
        Some(transform),
        scale,
        None,
    );
    output.set_preferred(WlMode { size: (w as i32, h as i32).into(), refresh });

    let position = match output_cfg {
        Some(cfg) => (cfg.x, cfg.y),
        None => {
            let x_offset: i32 = alice
                .outputs
                .iter()
                .filter_map(|info| alice.space.output_geometry(&info.output))
                .map(|geo| geo.size.w)
                .sum();
            (x_offset, 0)
        }
    };

    alice.space.map_output(&output, position);
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
                    frame_pending: false,
                },
            );
            drop(renderer);
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

    surface.frame_pending = false;

    render_surface(alice, node, crtc);
}

/// Looks up the `LayoutScope` (output id + focused tag) for `output`, the
/// same lookup both `render_surface` and `Backend::copy_frame` need before
/// they can gather render elements. Takes `&Outputs` specifically (not
/// `&Alice<UdevData>`) so it can be called from contexts — like
/// `copy_frame`, which receives `alice` and `self: &mut UdevData` as
/// separate parameters — that don't have a single combined `Alice` borrow
/// available.
fn output_scope(outputs: &Outputs, output: &Output) -> Option<LayoutScope> {
    let info = outputs.get(&output.name())?;
    let tag = outputs.get_focused_tag(info.id)?;
    Some(LayoutScope { output: info.id, tag })
}

/// Builds the space's render elements for `output` — fullscreen-window
/// substitution if applicable, otherwise the normal layered space. Shared
/// between the scanout path (`render_surface`) and screencopy
/// (`Backend::copy_frame`) so the two never drift out of sync with each
/// other.
fn output_space_elements<'a>(
    renderer: &mut UdevRenderer<'a>,
    space: &Space<Window>,
    window_registry: &crate::window::WindowRegistry,
    output: &Output,
    scope: LayoutScope,
) -> Result<Vec<UdevRenderElement<'a>>, OutputNoMode> {
    if let Some(fs_window) = window_registry.fullscreen_window_for_output(&scope) {
        fullscreen_output_elements(renderer, space, output, &fs_window, 1.0)
    } else {
        // `Space::render_elements_for_output` (the method) positions layer-shell
        // elements using only the output's own location, never the position
        // `LayerMap::arrange` actually computed for them (`layer_geometry`) — so
        // every layer surface (panels, bars, launchers) renders pinned near its
        // own local (0, 0) regardless of anchor/centering, while hit-testing
        // (which does read `layer_geometry` — see `Alice::layer_under` /
        // `surface_under`) reports the correct arranged position. The visible
        // result is a bar stuck in a corner whose click target is wherever it
        // was actually supposed to be. `space_render_elements` (the free
        // function; this is what winit's `render_output` already uses
        // internally, which is why this backend didn't show the bug) builds the
        // same element list but positions layers via `layer_geometry` correctly.
        space_render_elements(renderer, [space], output, 1.0)
    }
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

    let Some(scope) = output_scope(&alice.outputs, &output) else {
        return;
    };

    let space_elements = match output_space_elements(
        &mut renderer,
        &alice.space,
        &alice.window_registry,
        &output,
        scope,
    ) {
        Ok(elements) => elements,
        Err(err) => {
            eprintln!("Failed to gather render elements for {:?}: {:?}", output.name(), err);
            return;
        }
    };

    crate::cursor::reset_cursor_if_dead(&mut alice.cursor_status);
    let output_geo = alice.space.output_geometry(&output).unwrap();
    let output_scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
    let cursor_status = alice.cursor_status.clone();
    let cursor_pos = alice.seat.get_pointer().unwrap().current_location() - output_geo.loc.to_f64();

    let cursor_elements: Vec<crate::cursor::PointerRenderElement<UdevRenderer<'_>>> =
        crate::cursor::cursor_render_elements(
            &mut alice.backend_data.pointer_element,
            &cursor_status,
            &mut renderer,
            cursor_pos,
            output_scale,
        );

    let elements: Vec<UdevFrameRenderElement<'_>> = cursor_elements
        .into_iter()
        .map(UdevFrameRenderElement::Cursor)
        .chain(space_elements.into_iter().map(UdevFrameRenderElement::Space))
        .collect();

    match surface
        .drm_output
        .render_frame(&mut renderer, &elements, [0.1, 0.1, 0.1, 1.0], FrameFlags::DEFAULT)
    {
        Ok(res) if !res.is_empty => {
            if let Err(err) = surface.drm_output.queue_frame(()) {
                eprintln!("Failed to queue frame on crtc {:?}: {}", crtc, err);
            } else {
                surface.frame_pending = true;
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

fn fullscreen_output_elements<'a>(
    renderer: &mut UdevRenderer<'a>,
    _space: &Space<Window>,
    output: &Output,
    fs_window: &Window,
    scale: f64,
) -> Result<Vec<UdevRenderElement<'a>>, OutputNoMode> {
    let scale: smithay::utils::Scale<f64> = smithay::utils::Scale::from(scale);
    let mut elements = Vec::new();
    let layer_map = layer_map_for_output(output);

    for layer in layer_map.layers_on(Layer::Overlay) {
        let Some(geo) = layer_map.layer_geometry(layer) else { continue };
        elements.extend(layer.render_elements(
            renderer,
            geo.loc.to_physical_precise_round(scale),
            scale,
            1.0,
        ));
    }

    elements.extend(fs_window.render_elements(
        renderer,
        (0, 0).into(),
        scale,
        1.0,
    ));

    for layer_kind in [Layer::Bottom, Layer::Background] {
        for layer in layer_map.layers_on(layer_kind) {
            let Some(geo) = layer_map.layer_geometry(layer) else { continue };
            elements.extend(layer.render_elements(
                renderer,
                geo.loc.to_physical_precise_round(scale),
                scale,
                1.0,
            ));
        }
    }

    Ok(elements)
}

impl DmabufHandler for Alice<UdevData> {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.backend_data.dmabuf_state.as_mut().unwrap().0
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if self
            .backend_data
            .gpus
            .single_renderer(&self.backend_data.primary_gpu)
            .and_then(|mut renderer| renderer.import_dmabuf(&dmabuf, None))
            .is_ok()
        {
            let _ = notifier.successful::<Alice<UdevData>>();
        } else {
            notifier.failed();
        }
    }
}

smithay::delegate_dmabuf!(Alice<UdevData>);
