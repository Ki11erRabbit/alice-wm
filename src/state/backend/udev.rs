use std::{collections::HashMap, os::unix::raw::dev_t, path::Path, time::{Duration, Instant}};

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
            element::{AsRenderElements, Element, Kind, RenderElement, surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree}},
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
        wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        wayland_server::{Display, backend::GlobalId, protocol::{wl_buffer::WlBuffer, wl_output::WlOutput}},
    },
    // NB: deliberately NOT importing `smithay::utils::Scale` here — `Scale`
    // above (from `smithay::output`) is a different type used for output
    // scale factors. Rendering code below refers to the utils one via its
    // full path (`smithay::utils::Scale::from(...)`) to avoid the collision.
    utils::{DeviceFd, Physical, Point, Rectangle, Transform},
    wayland::{
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier}, session_lock::LockSurface, shell::wlr_layer::Layer, shm::with_buffer_contents_mut
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
/// space's contents, the software cursor drawn on top, and any windows
/// currently animating a box change (open/close/reorder/reflow — see
/// `animation.rs`), drawn scaled instead of through `Space`'s normal
/// per-element path (they're unmapped from `Space` for exactly this
/// reason while animating — see `Alice::start_window_morph`).
smithay::backend::renderer::element::render_elements! {
    UdevFrameRenderElement<='a, UdevRenderer<'a>>;
    Space=UdevRenderElement<'a>,
    Cursor=crate::cursor::PointerRenderElement<UdevRenderer<'a>>,
    Morph=crate::animation::ScaledElement<WaylandSurfaceRenderElement<UdevRenderer<'a>>>,
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

        // Registers `zwlr_screencopy_manager_v1` so grim/grimshot/OBS-via-
        // xdg-desktop-portal-wlr can find it. Hardcoded version `3` here to
        // match the constant of the same value in the dispatch module —
        // swap this for an import of that constant (e.g.
        // `crate::handlers::screencopy::SCREENCOPY_VERSION`) if you'd
        // rather not have the magic number duplicated across udev.rs and
        // winit.rs.
        let screencopy_global = display_handle.create_global::<Alice<UdevData>, ZwlrScreencopyManagerV1, _>(3, ());
        alice.backend_data.screencopy_global = Some(screencopy_global);

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
        // Mirrors the DeviceAdded handler in setup() below, which does the
        // same `device.led_update(led_state.into())` call for a single
        // newly-plugged-in keyboard. This does it for every keyboard
        // already being tracked, whenever the compositor's LED state
        // itself changes (e.g. caps lock toggled).
        for device in &mut self.keyboards {
            device.led_update(led_state.into());
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
        //eprintln!("[{:?}] schedule_render: {} targets", alice.start_time.elapsed(), targets.len());
        for (node, crtc) in targets {
            render_surface(alice, node, crtc);
        }
    }

    // The default `schedule_render` above re-renders *every* CRTC on
    // *every* call — correct, but on a multi-output setup it means a
    // single `wl_surface.commit` on one monitor (a video frame, a
    // cursor blink, anything) pays the full cost of re-gathering
    // elements and running damage tracking for every other connected
    // monitor too, every single time, regardless of whether they have
    // anything new to show. That fan-out is silent (each of those other
    // outputs reports no damage and skips the actual DRM submit — see
    // `frame_queued` further down in `render_surface`) but not free:
    // building `output_space_elements` and importing the cursor texture
    // still happen for all of them. On a 3-output rig, that's roughly
    // 3x the CPU work behind every commit-triggered redraw, which is
    // squarely in the kind of overhead that fits inside a 60Hz frame's
    // budget and doesn't fit inside a 120Hz one.
    //
    // `(DrmNode, crtc::Handle)` is stashed on every `Output` at creation
    // time (see `connector_connected`'s `output.user_data().insert_if_missing`
    // call) specifically so call sites that already know which output
    // changed — `compositor.rs`'s commit handler, primarily — can look
    // it up and render just that one CRTC instead of going through the
    // default's "every CRTC" path.
    fn schedule_render_output(alice: &mut Alice<Self>, output: &Output) {
        let Some(&(node, crtc)) = output.user_data().get::<(DrmNode, crtc::Handle)>() else {
            // Not (yet) an output this backend knows the CRTC for —
            // fall back to the safe, do-everything default rather than
            // silently rendering nothing.
            return Self::schedule_render(alice);
        };
        let pending = alice
            .backend_data
            .backends
            .get(&node)
            .and_then(|b| b.surfaces.get(&crtc))
            .map(|s| s.frame_pending)
            .unwrap_or(true);
        if !pending {
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
            output_space_elements(&mut renderer, &alice.space, &alice.window_registry, output, scope, &alice.lock_surfaces, alice.locked)
                .map_err(|e| format!("failed to gather render elements: {e:?}"))?;

        let mut elements: Vec<UdevFrameRenderElement<'_>> =
            space_elements.into_iter().map(UdevFrameRenderElement::Space).collect();

        // The real output scale (Apple Silicon panels commonly run at 2x
        // HiDPI) — needed both for the cursor overlay below and, more
        // importantly, for querying each element's geometry further down.
        // `RenderElement::geometry(scale)` uses this `scale` to convert an
        // element's *size* from logical to physical pixels, while its
        // *position* is already stored pre-converted to physical pixels at
        // the real output scale (baked in when `space_render_elements`
        // built these elements). Querying geometry with the wrong scale
        // therefore doesn't move or corrupt anything — it desyncs size
        // from position: elements land at their correct on-screen
        // location but sized as if the display were 1x, so on a 2x output
        // everything draws at a quarter of its real area, all bunched
        // toward each element's top-left corner. That reads exactly like
        // "windows moved for the draw."
        let output_scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());

        if overlay_cursor {
            let output_geo = alice
                .space
                .output_geometry(output)
                .ok_or("output has no geometry in space")?;
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

        // This backend actually deals in TWO differently "kinded" but
        // numerically-identical sizes here — `render`/`clear`/element
        // `draw` want `Physical`-kind, while `create_buffer` and
        // `copy_framebuffer` want `Buffer`-kind (confirmed against this
        // Smithay version's real signatures via the compiler, not guessed).
        // These kinds are phantom types Smithay uses to stop coordinate
        // spaces from being mixed up by accident; since no buffer scaling
        // is applied here, the numeric values are the same either way — we
        // just need both differently-typed handles to satisfy each call.
        //
        // `full_size` is always the output's real, untransformed mode size
        // (matching `OutputModeSource::Auto`'s own resolution, i.e. exactly
        // what the working scanout path renders into) — NOT the requested
        // crop. Rendering a pre-shifted, crop-sized canvas together with
        // this output's *real* transform doesn't work: `render()`'s
        // transform math needs the true full-output size to compute a
        // correct rotation, so a rotated output (e.g. this one, configured
        // at 270°) rendered into an undersized, pre-shifted canvas produces
        // nonsense — elements can land somewhere that happens to overlap
        // where a *different* output's content would be, which is exactly
        // what "getting my other monitor's content, chopped up" looks like.
        // So: always render the whole output, then crop by copying just
        // the requested sub-rectangle out of the readback — the same
        // approach `winit.rs`'s `copy_frame` already uses.
        let mode = output.current_mode().ok_or("output has no current mode")?;
        let full_size = mode.size;
        let full_size_buf: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (full_size.w, full_size.h).into();
        let capture_size = region.map(|r| r.size).unwrap_or(full_size);
        let capture_loc: Point<i32, Physical> = region.map(|r| r.loc).unwrap_or((0, 0).into());

        // --- render into an offscreen target ---
        //
        // `GlesTexture` — not a "Multi"-prefixed type. `GlesRenderer` (the
        // concrete backend `MultiRenderer` wraps here) implements
        // `Offscreen<GlesTexture>` and `Offscreen<GlesRenderbuffer>`
        // directly; `MultiRenderer` forwards through to whichever of those
        // the inner renderer supports, rather than defining its own
        // separate offscreen target type. Also: `bind()`'s real signature
        // is `fn bind<'a>(&mut self, target: &'a mut Target) -> ...` — it
        // takes `&mut Target`, not an owned `Target`, hence `target` needs
        // to be `mut` and passed as `&mut target` below.
        let mut target: smithay::backend::renderer::gles::GlesTexture = renderer
            .create_buffer(Fourcc::Argb8888, full_size_buf)
            .map_err(|e| format!("failed to create offscreen buffer: {e}"))?;

        let mut fb = renderer
            .bind(&mut target)
            .map_err(|e| format!("failed to bind offscreen target: {e}"))?;

        // The output's *real* transform, not a hardcoded `Transform::Normal`
        // — this was previously ignoring any configured rotation entirely,
        // which is the other half of the bug described above.
        // NOTE: `output.current_transform().invert()`, not the raw value.
        // Real scanout (`DrmCompositor`, working correctly — this is only
        // about the screenshot) evidently doesn't feed this stored value
        // straight into a `render()` call the way this function does; it
        // likely goes through a hardware plane rotation property or some
        // other internal path with its own direction convention. Using the
        // raw transform here produced an exact 180° mismatch on this
        // rotated output (upside-down, nothing else wrong) — and
        // `Transform::_270.invert() == Transform::_90`, exactly 180° apart,
        // which is why `.invert()` corrects it. `Transform::Normal` and
        // `Transform::_180` are their own inverses, so this is also a
        // no-op for every non-rotated output — it shouldn't be able to
        // regress the center monitor.
        let mut frame = renderer
            .render(&mut fb, full_size, output.current_transform().invert())
            .map_err(|e| format!("failed to start frame: {e}"))?;
        frame
            .clear([0.0, 0.0, 0.0, 1.0].into(), &[Rectangle::from_size(full_size)])
            .map_err(|e| format!("failed to clear frame: {e}"))?;

        // Elements are gathered top-most-first (cursor first, then space
        // elements); draw back-to-front, so iterate in reverse. No crop
        // shift here — elements are drawn at their true output-local
        // position, matching the full-size canvas above; cropping happens
        // after readback instead (see below).
        for element in elements.iter().rev() {
            let src = element.src();
            let dst = element.geometry(output_scale);
            let damage = [Rectangle::from_size(dst.size)];
            if let Err(err) = element.draw(&mut frame, src, dst, &damage, &[]) {
                eprintln!("screencopy: failed to draw element for {:?}: {:?}", output.name(), err);
            }
        }

        // `Frame::finish()` returns a `SyncPoint`, not a completed render.
        // On any driver with EGL fence support (i.e. basically every real
        // GBM/DRM setup, which is exactly this backend) `finish_internal`
        // only calls `glFlush()` and hands back an *unsignaled* fence —
        // GPU work is submitted, not necessarily done. Smithay marks the
        // return type `#[must_use = "... failing to [wait] may result in
        // unexpected rendering artifacts"]` for exactly this reason. The
        // previous code bound it to `_sync_point` (silencing that lint)
        // and never waited, so `copy_framebuffer`/`map_texture` below could
        // read the target texture while the GPU was still mid-draw —
        // a race that shows up as a garbled/incomplete screenshot,
        // independent of what region was requested or where on the output
        // it was. Waiting here blocks until the fence actually signals.
        let sync_point = frame.finish().map_err(|e| format!("failed to finish frame: {e}"))?;
        sync_point
            .wait()
            .map_err(|e| format!("failed waiting for GPU render to finish before readback: {e}"))?;


        // --- read back the full output, then crop into the client's shm buffer ---
        let mapping = renderer
            .copy_framebuffer(&fb, Rectangle::from_size(full_size_buf), Fourcc::Argb8888)
            .map_err(|e| format!("failed to read back framebuffer: {e}"))?;
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("failed to map readback texture: {e}"))?;

        with_buffer_contents_mut(buffer, |ptr, len, data| {
            let dst_stride = data.stride as usize;

            // The client's actual buffer was allocated based on the
            // width/height/stride advertised in `init_frame`'s `frame.buffer(...)`
            // call, at `capture_output`/`capture_output_region` time. `capture_size`
            // here is recomputed independently from the *current* output mode/region
            // math at `copy` time, which is not guaranteed to still agree (a modeset
            // race, a rounding difference, or simply a misbehaving/malicious client
            // that calls `copy` with a buffer that doesn't match what it was told).
            // Trusting `capture_size` blindly here would walk off the end of the
            // client's shared memory mapping and corrupt whatever heap data happens
            // to sit past it *in the client's own process* — silent memory
            // corruption there, not a crash here, which is exactly why it's so hard
            // to trace back. Validate against the buffer's own reported dimensions
            // and the real mapped length before writing a single byte.
            if capture_size.w < 0
                || capture_size.h < 0
                || capture_size.w > data.width
                || capture_size.h > data.height
            {
                return Err(format!(
                    "requested capture size {:?} exceeds client buffer dimensions {}x{}",
                    capture_size, data.width, data.height
                ));
            }
            let needed = dst_stride.saturating_mul(capture_size.h as usize);
            if needed > len {
                return Err(format!(
                    "requested capture needs {needed} bytes but client buffer is only {len} bytes"
                ));
            }

            let full_stride = full_size.w as usize * 4;
            let row_bytes = (capture_size.w as usize * 4).min(dst_stride);
            let x_offset_bytes = capture_loc.x as usize * 4;
            for row in 0..capture_size.h as usize {
                let src_offset = (capture_loc.y as usize + row) * full_stride + x_offset_bytes;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(src_offset),
                        ptr.add(row * dst_stride),
                        row_bytes,
                    );
                }
            }
            Ok(())
        })
        .map_err(|e| format!("failed to write into client shm buffer: {e:?}"))??;

        Ok(())
    }

    fn change_vt(&mut self, vt: i32) {
        _ = self.session.change_vt(vt);
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

    // Compute this before `change_current_state` (not after, as it was) —
    // see the location argument below.
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

    // The 4th argument here is `new_location: Option<Point<i32, Logical>>`.
    // This was previously always `None` — which doesn't mean "leave it
    // unset", it means "don't touch `Output`'s own location field (and
    // don't tell xdg-output listeners it changed) at all". `Space` tracks
    // each output's position completely separately from the `Output`
    // object itself: `space.map_output()` below only updates `Space`'s own
    // internal bookkeeping (what `space.output_geometry()` reads, which is
    // what real rendering/hit-testing/window placement all correctly use —
    // why multi-monitor otherwise looks fine). It does NOT touch
    // `Output`'s own `location` field, which is the only thing xdg-output's
    // `logical_position` event is ever derived from. With every output
    // stuck reporting (0, 0) here, any client that lays out multiple
    // monitors from xdg-output geometry — slurp drawing its selection
    // overlay, grim converting a selected global-space rectangle into an
    // output-relative one for `CaptureOutputRegion` — sees every monitor
    // as occupying the same (0, 0) origin. That's the selection overlay
    // "showing up on every monitor" and captures landing on the wrong
    // output entirely. Passing the real position here keeps `Output`'s own
    // state (and therefore xdg-output) in sync with where `Space` actually
    // places it.
    output.change_current_state(
        Some(WlMode { size: (w as i32, h as i32).into(), refresh }),
        Some(transform),
        scale,
        Some(position.into()),
    );
    output.set_preferred(WlMode { size: (w as i32, h as i32).into(), refresh });

    alice.space.map_output(&output, position);
    output.create_global::<Alice<UdevData>>(&alice.display_handle);
    let output_id = alice.outputs.insert(output.clone());
    alice.workspace_output_added(output_id);
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

    if let Some(id) = alice.outputs.get(&surface.output.name()).map(|info| info.id) {
        alice.workspace_output_removed(id);
    }
    alice.outputs.deactivate(&surface.output.name());
    alice.space.unmap_output(&surface.output);
}

pub fn device_removed(alice: &mut Alice<UdevData>, node: DrmNode) {
    if let Some(backend) = alice.backend_data.backends.remove(&node) {
        for (_, surface) in backend.surfaces {
            if let Some(id) = alice.outputs.get(&surface.output.name()).map(|info| info.id) {
                alice.workspace_output_removed(id);
            }
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
    tracing::trace!("[{:?}] frame_finish: crtc={:?} (real vblank)", alice.start_time.elapsed(), crtc);

    if alice.locked {
        alice.blanked_outputs.insert(surface.output.clone());
        alice.try_lock();
    }

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
    lock_surfaces: &HashMap<Output, LockSurface>,
    locked: bool,
) -> Result<Vec<UdevRenderElement<'a>>, OutputNoMode> {
    // The output's real (possibly fractional) scale — e.g. `1.5` for a
    // HiDPI panel configured with a fractional scale. `fullscreen_output_elements`
    // and `render_lock_surfaces` (both defined further down in this file)
    // take this as an explicit `scale: f64` parameter, and were being
    // called with a hardcoded `1.0` regardless of the output's actual
    // scale — every element in the fullscreen and locked-session paths
    // was built sized/positioned as if the output were unscaled, while
    // the real framebuffer is provisioned at the output's true (larger)
    // physical resolution. (`space_render_elements` just below, by
    // contrast, takes `alpha` as its fourth argument, not scale — it
    // reads the output's real scale internally via `output.current_scale()`,
    // so `1.0` there is correctly "fully opaque", not a scale bug.)
    let scale = output.current_scale().fractional_scale();

    if !locked {
        if let Some(fs_window) = window_registry.fullscreen_window_for_output(&scope) {
                fullscreen_output_elements(renderer, space, output, &fs_window, scale)
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
                // Its fourth argument is `alpha` (opacity), not scale — see the comment
                // above — so `1.0` here is correct as-is.
                space_render_elements(renderer, [space], output, 1.0)
            }
    } else if let Some(surface) = lock_surfaces.get(output) {
        render_lock_surfaces(renderer, output, surface, scale)
    } else {
        Ok(Vec::new())
    }
}

fn render_surface(alice: &mut Alice<UdevData>, node: DrmNode, crtc: crtc::Handle) {
    // Advance any in-flight tag-slide animations first, before anything
    // else in this function. This needs a full `&mut alice` — once
    // `renderer` is acquired just below, it holds a borrow of
    // `alice.backend_data.gpus` for the rest of the function, and nothing
    // requiring `&mut alice` as a whole can run after that (that's the
    // E0499 this was hitting when the call sat further down).
    //
    // Unlike the winit backend, this render path only runs again once the
    // previous frame's pageflip completes (`frame_finish`, further up
    // this file, calls back into `render_surface`) or something external
    // asks for a redraw (`schedule_render`) — there's no free-running
    // loop to piggyback on. Updating animated positions here is still
    // enough on its own though: as long as a position actually changed
    // this frame, `render_frame` below reports damage, which makes this
    // backend queue and later flip a frame, which is what triggers the
    // next `frame_finish` call — so the chain keeps this function
    // re-running every frame for exactly as long as any animation on
    // this output has a position left to update, then stops itself the
    // moment there's nothing left to change.
    alice.advance_tag_animations();

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
        &alice.lock_surfaces,
        alice.locked,
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

    // Any window currently animating a box change (open/close/reorder/
    // reflow) is drawn here, scaled, instead of through `Space`'s normal
    // per-element path above — see the doc comment on `UdevFrameRenderElement`.
    let morph_elements = crate::state::morph_elements_for_output(
        &mut renderer,
        &mut alice.window_morphs,
        &mut alice.space,
        scope.output,
        output_geo.loc,
        output.current_scale().fractional_scale(),
    );

    let elements: Vec<UdevFrameRenderElement<'_>> = cursor_elements
        .into_iter()
        .map(UdevFrameRenderElement::Cursor)
        .chain(morph_elements.into_iter().map(UdevFrameRenderElement::Morph))
        .chain(space_elements.into_iter().map(UdevFrameRenderElement::Space))
        .collect();

    let element_count = elements.len();
    let render_start = Instant::now();

    let frame_queued = match surface
        .drm_output
        .render_frame(&mut renderer, &elements, [0.1, 0.1, 0.1, 1.0], FrameFlags::DEFAULT)
    {
        Ok(res) if !res.is_empty => {
            if let Err(err) = surface.drm_output.queue_frame(()) {
                eprintln!("Failed to queue frame on crtc {:?}: {}", crtc, err);
                false
            } else {
                surface.frame_pending = true;
                true
            }
        }
        Ok(_) => false,
        Err(err) => {
            eprintln!("render_frame failed on crtc {:?}: {:?}", crtc, err);
            false
        }
    };

    // Temporary diagnostic: only fires when a single frame's CPU-side
    // render_frame()+queue_frame() cost blows past half of a 120Hz
    // frame's budget (8.3ms total, so 4ms here leaves room for the rest
    // of render_surface plus actual GPU/DRM submit time on top). A
    // healthy frame should be well under this; if this line shows up
    // repeatedly while the lag is happening, that pins the cost on
    // render_frame itself (texture import, damage computation, or the
    // GPU work it kicks off) rather than anything upstream in relayout/
    // focus/animation logic — remove once you've got your answer.
    let render_elapsed = render_start.elapsed();
    if render_elapsed > Duration::from_millis(4) {
        eprintln!(
            "[{:?}] SLOW FRAME: crtc={:?} elements={} render_frame+queue took {:?}",
            alice.start_time.elapsed(), crtc, element_count, render_elapsed
        );
    }

    alice.refresh_fractional_scale_for_output(&output);

    // Only actually tell clients "your last frame was shown, send the next
    // one" (`wl_surface.frame`'s done callback) when a frame genuinely got
    // queued to the display above — never unconditionally on every call to
    // this function. `schedule_render` calls this directly (not just
    // `frame_finish`, the real page-flip-completion callback) any time
    // something asks for a redraw while no flip is currently in flight,
    // and `Ok(_) => {}` above (no damage, nothing queued) is a completely
    // ordinary outcome of that — most redraws asked for by an input event
    // or a commit find nothing new to actually display. Sending the "go
    // ahead" callback anyway, as this used to do regardless of whether
    // anything was queued, invites every mapped client to immediately
    // commit again — which promptly asks for another redraw, which (still
    // often) finds nothing new either, and sends the same invitation
    // again. With nothing here ever actually gated on a real vsync tick,
    // that loop runs as fast as the CPU allows, entirely decoupled from
    // the display's actual refresh rate — which is what produced the
    // constant redraw storm (tens of thousands of `render_surface` calls
    // per second, evenly across the whole session, regardless of whether
    // any window was even open) rather than anything actually tied to
    // tiling/layout. Gating on `frame_queued` restores the normal
    // Wayland contract — a client only gets told to render again once its
    // last frame was actually shown — so a quiescent scene naturally goes
    // quiet instead of self-sustaining.
    if frame_queued {
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
        if let Some(lock_surface) = alice.lock_surfaces.get(&output) {
            smithay::desktop::utils::send_frames_surface_tree(
                lock_surface.wl_surface(),
                &output,
                alice.start_time.elapsed(),
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
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


fn render_lock_surfaces<'a>(
    renderer: &mut UdevRenderer<'a>,
    output: &Output,
    lock_surface: &LockSurface,
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
    let new_elements = render_elements_from_surface_tree(
        renderer,
        lock_surface.wl_surface(),
        (0, 0),
        scale,
        1.0,
        Kind::Unspecified,
    );
    tracing::trace!("render_lock_surfaces: {} elements", elements.len());
    elements.extend(new_elements);

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
