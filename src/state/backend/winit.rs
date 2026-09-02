use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        drm::{DrmNode, NodeType},
        input::{DeviceCapability, InputEvent},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            damage::OutputDamageTracker, gles::{GlesRenderer, GlesTexture}, Bind, ExportMem, Offscreen,
        },
        session::{Session, libseat::LibSeatSession},
        udev::{UdevBackend, primary_gpu},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::EventLoop,
        input::Libinput,
        wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
        wayland_server::{
            backend::GlobalId, protocol::{wl_buffer::WlBuffer, wl_surface}, Display, DisplayHandle,
        },
    },
    utils::{Physical, Point, Rectangle, Transform},
    wayland::shm::with_buffer_contents_mut,
};
use crate::{Alice, CalloopData, config::Config, state::backend::Backend};



pub struct WinitData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
    pub pointer_element: crate::cursor::PointerElement,
    /// `None` until the screencopy global is registered in `setup()` below
    /// — see the equivalent field/comment in `udev.rs` for why this can't
    /// be set any earlier.
    pub screencopy_global: Option<GlobalId>,
}


impl Backend for WinitData {
    const HAS_RELATIVE_MOTION: bool =  false;
    const HAS_GESTURES: bool =  false;

    fn setup(event_loop: &mut EventLoop<'static, CalloopData<Self>>) -> Result<CalloopData<Self>, Box<dyn std::error::Error>> {

        let output = Output::new(
            "output-0".into(),
            PhysicalProperties {
                size: (1920, 1080).into(),
                subpixel: Subpixel::HorizontalRgb,
                make: "Screens Inc".into(),
                model: "Monitor Ultra".into(),
            },
        );

        output.change_current_state(
            Some(Mode {
                 size: (1920, 1080).into(),
                 refresh: 60
            }),
            Some(Transform::Flipped180),
            Some(Scale::Integer(1)),
            Some((0, 0).into())
        );
        output.set_preferred(
            Mode {
                 size: (1920, 1080).into(),
                 refresh: 60
            }
        );

        let (mut backend, winit) = winit::init()?;
        let mut damage_tracker = OutputDamageTracker::from_output(&output);

        let backend_data = WinitData {
            backend,
            damage_tracker,
            pointer_element: crate::cursor::PointerElement::default(),
            screencopy_global: None,
        };


        let display: Display<Alice<Self>> = Display::new()?;
        let display_handle = display.handle();
        let mut alice = Alice::new(backend_data, event_loop, display);
        let position = alice.config.get_output_position(&output.name())
            .map(|pos| (pos.x, pos.y))
            .unwrap_or((0, 0));
        alice.space.map_output(&output, position);
        alice.outputs.insert(output.clone());
        output.create_global::<Alice<Self>>(&display_handle);

        // TODO(screencopy): SCREENCOPY_VERSION currently lives in the
        // dispatch module (e.g. `crate::handlers::screencopy`) — adjust the
        // path below to wherever you actually placed it, or just hardcode
        // `3` if you'd rather not import it here.
        let screencopy_global = display_handle
            .create_global::<Alice<Self>, ZwlrScreencopyManagerV1, _>(3, ());
        alice.backend_data.screencopy_global = Some(screencopy_global);

        // WAYLAND_DISPLAY/XDG_CURRENT_DESKTOP are exported (to our own
        // process env *and* the D-Bus/systemd activation environment) by
        // Alice::new -> export_activation_environment, right after the
        // socket is created.

        let data = CalloopData {
            state: alice,
            display_handle,
        };

        event_loop.handle().insert_source(winit, move |event, _, data: &mut CalloopData<Self>| {
            let display = &mut data.display_handle;
            let state = &mut data.state;

            match event {
                WinitEvent::Resized { size, .. } => {
                    output.change_current_state(
                        Some(Mode {
                            size,
                            refresh: 60_000,
                        }),
                        None,
                        None,
                        None,
                    );
                }
                WinitEvent::Input(event) => state.process_input_event(event),
                WinitEvent::Redraw => {
                    crate::cursor::reset_cursor_if_dead(&mut state.cursor_status);

                    let output_geo = state.space.output_geometry(&output).unwrap();
                    let output_scale =
                        smithay::utils::Scale::from(output.current_scale().fractional_scale());
                    let cursor_pos = state.seat.get_pointer().unwrap().current_location()
                        - output_geo.loc.to_f64();
                    let cursor_status = state.cursor_status.clone();

                    let size = state.backend_data.backend.window_size();
                    let damage = Rectangle::from_size(size);

                    {
                        let (renderer, mut framebuffer) = state.backend_data.backend.bind().unwrap();

                        let cursor_elements: Vec<crate::cursor::PointerRenderElement<GlesRenderer>> =
                            crate::cursor::cursor_render_elements(
                                &mut state.backend_data.pointer_element,
                                &cursor_status,
                                renderer,
                                cursor_pos,
                                output_scale,
                            );

                        smithay::desktop::space::render_output::<
                            _,
                            crate::cursor::PointerRenderElement<GlesRenderer>,
                            _,
                            _,
                        >(
                            &output,
                            renderer,
                            &mut framebuffer,
                            1.0,
                            0,
                            [&state.space],
                            &cursor_elements,
                            &mut state.backend_data.damage_tracker,
                            [0.1, 0.1, 0.1, 1.0],
                        )
                        .unwrap();
                    }
                    state.backend_data.backend.submit(Some(&[damage])).unwrap();

                    state.space.elements().for_each(|window| {
                        window.send_frame(
                            &output,
                            state.start_time.elapsed(),
                            Some(Duration::ZERO),
                            |_, _| Some(output.clone()),
                        )
                    });

                    if let Some(layers) = state.layer_surfaces.get(
                        &state.outputs.get(&output.name()).unwrap().id
                    ) {
                        for layer in layers {
                            layer.surface.send_frame(
                                &output,
                                state.start_time.elapsed(),
                                Some(Duration::ZERO),
                                |_, _| Some(output.clone())
                            );
                        }

                    }

                    state.space.refresh();
                    state.popups.cleanup();
                    let _ = display.flush_clients();

                    // Ask for redraw to schedule new frame.
                    state.backend_data.backend.window().request_redraw();
                }
                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }
                _ => (),
            };
        })?;

        Ok(data)
    }

    fn seat_name(&self) -> String {
        String::from("winit")
    }

    fn reset_buffers(&mut self, _output: &smithay::output::Output) {

    }

    fn early_import(&mut self, _surface: &wl_surface::WlSurface) {

    }

    fn update_led_state(&mut self, _led_state: smithay::input::keyboard::LedState) {

    }

    fn schedule_render(_alice: &mut crate::Alice<Self>) {

    }

    fn make_config() -> Config {
        let config = match crate::config::execute_lua_config(true) {
            Ok(config) => config,
            Err(err) => {
                //eprintln!("Error while loading config: {err}");
                Config::default()
            }
        };
        config
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

    // Same "no separate `self`" shape as udev's `copy_frame` and for the
    // same reason — `self`/`backend_data` is a field of `alice`, so a
    // caller can only ever hold one mutable borrow that covers both.
    //
    // Unlike udev's version, this doesn't manually walk render elements:
    // `WinitGraphicsBackend<GlesRenderer>` gives a plain `GlesRenderer`
    // (no `MultiRenderer` wrapper), and `smithay::desktop::space::render_output`
    // — already used above for the real swapchain frame — does the
    // "gather elements for this output + draw them" work in one call. We
    // just point it at our own offscreen framebuffer instead of the
    // window's.
    //
    // Region cropping is done post-render rather than during rendering:
    // always render the FULL output offscreen, then copy only the
    // requested sub-rectangle out when writing into the client's shm
    // buffer. Simpler than reproducing per-element geometry math, and
    // correct for any region as long as it's within the output.
    fn copy_frame(
        alice: &mut crate::Alice<Self>,
        output: &Output,
        region: Option<Rectangle<i32, Physical>>,
        overlay_cursor: bool,
        buffer: &WlBuffer,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mode = output.current_mode().ok_or("output has no current mode")?;
        let full_size = mode.size; // Physical
        let capture_loc: Point<i32, Physical> = region.map(|r| r.loc).unwrap_or((0, 0).into());
        let capture_size = region.map(|r| r.size).unwrap_or(full_size);

        let (renderer, _window_fb) = alice
            .backend_data
            .backend
            .bind()
            .map_err(|e| format!("failed to bind winit backend: {e}"))?;

        // `create_buffer`/`copy_framebuffer` want `Buffer`-kind sizes;
        // `render_output` wants `Physical`-kind — same split as udev.rs,
        // confirmed against this Smithay version's real signatures there.
        let full_size_buf: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (full_size.w, full_size.h).into();

        let mut target: GlesTexture = renderer
            .create_buffer(Fourcc::Argb8888, full_size_buf)
            .map_err(|e| format!("failed to create offscreen buffer: {e}"))?;

        let mut fb = renderer
            .bind(&mut target)
            .map_err(|e| format!("failed to bind offscreen target: {e}"))?;

        let output_scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
        let cursor_elements: Vec<crate::cursor::PointerRenderElement<GlesRenderer>> = if overlay_cursor {
            let output_geo = alice.space.output_geometry(output).ok_or("output has no geometry in space")?;
            let cursor_pos = alice
                .seat
                .get_pointer()
                .ok_or("seat has no pointer")?
                .current_location()
                - output_geo.loc.to_f64();
            let cursor_status = alice.cursor_status.clone();
            crate::cursor::cursor_render_elements(
                &mut alice.backend_data.pointer_element,
                &cursor_status,
                renderer,
                cursor_pos,
                output_scale,
            )
        } else {
            Vec::new()
        };

        // A fresh, throwaway tracker (age 0 → forces a full redraw every
        // call) rather than reusing `alice.backend_data.damage_tracker` —
        // that one tracks the real swapchain's frame-to-frame damage, and
        // feeding it an unrelated offscreen render would desync its aging.
        let mut scratch_damage_tracker = OutputDamageTracker::from_output(output);

        smithay::desktop::space::render_output::<
            _,
            crate::cursor::PointerRenderElement<GlesRenderer>,
            _,
            _,
        >(
            output,
            renderer,
            &mut fb,
            1.0,
            0,
            [&alice.space],
            &cursor_elements,
            &mut scratch_damage_tracker,
            [0.0, 0.0, 0.0, 1.0],
        )
        .map_err(|e| format!("failed to render offscreen: {e:?}"))?;

        let mapping = renderer
            .copy_framebuffer(&fb, Rectangle::from_size(full_size_buf), Fourcc::Argb8888)
            .map_err(|e| format!("failed to read back framebuffer: {e}"))?;
        let pixels = renderer
            .map_texture(&mapping)
            .map_err(|e| format!("failed to map readback texture: {e}"))?;

        with_buffer_contents_mut(buffer, |ptr, _len, data| {
            let dst_stride = data.stride as usize;
            let full_stride = full_size.w as usize * 4;
            let row_bytes = (capture_size.w as usize * 4).min(dst_stride);
            let x_offset_bytes = capture_loc.x as usize * 4;
            for row in 0..capture_size.h as usize {
                let src_offset = (capture_loc.y as usize + row) * full_stride + x_offset_bytes;
                let dst_offset = row * dst_stride;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        pixels.as_ptr().add(src_offset),
                        ptr.add(dst_offset),
                        row_bytes,
                    );
                }
            }
        })
        .map_err(|e| format!("failed to write into client shm buffer: {e:?}"))?;

        Ok(())
    }
}
