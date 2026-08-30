
use std::time::Duration;

use smithay::{backend::{drm::{DrmNode, NodeType}, input::{DeviceCapability, InputEvent}, libinput::{LibinputInputBackend, LibinputSessionInterface}, renderer::{damage::OutputDamageTracker, element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer}, session::{Session, libseat::LibSeatSession}, udev::{UdevBackend, primary_gpu}, winit::{self, WinitEvent, WinitGraphicsBackend}}, output::{Mode, Output, PhysicalProperties, Scale, Subpixel}, reexports::{
    calloop::EventLoop, input::Libinput, wayland_server::{Display, DisplayHandle, protocol::wl_surface}
}, utils::{Rectangle, Transform}};
use crate::{Alice, CalloopData, state::backend::Backend};




pub struct WinitData {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    pub damage_tracker: OutputDamageTracker,
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
        };


        let display: Display<Alice<Self>> = Display::new()?;
        let display_handle = display.handle();
        let mut alice = Alice::new(backend_data, event_loop, display);
        alice.space.map_output(&output, (0, 0));
        alice.outputs.insert(output.clone());
        output.create_global::<Alice<Self>>(&display_handle);

        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", &alice.socket_name);
        }

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
                    let size = state.backend_data.backend.window_size();
                    let damage = Rectangle::from_size(size);

                    {
                        let (renderer, mut framebuffer) = state.backend_data.backend.bind().unwrap();
                        smithay::desktop::space::render_output::<
                            _,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                            _,
                            _,
                        >(
                            &output,
                            renderer,
                            &mut framebuffer,
                            1.0,
                            0,
                            [&state.space],
                            &[],
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
}
