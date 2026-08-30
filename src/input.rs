use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent
    },
    input::{
        keyboard::{FilterResult, Keysym},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
};

use crate::{output::TagId, state::{Alice, backend::Backend}};

impl<BackendData: Backend + 'static> Alice<BackendData> {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);
                let key_state = event.state();

                self.seat.get_keyboard().unwrap().input::<(), _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |state, modifiers, keysym_handle| {
                        let keysym = keysym_handle.modified_sym();
                        if key_state == KeyState::Pressed && state.try_handle_keypress(modifiers, keysym) {
                            return FilterResult::Intercept(());
                        }
                        FilterResult::Forward
                    },
                );
            }
            InputEvent::PointerMotion { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();

                let output = self.space.outputs().next().unwrap();
                let output_geo = self.space.output_geometry(output).unwrap();

                let mut pos = pointer.current_location() + event.delta();
                pos.x = pos.x.clamp(output_geo.loc.x as f64, (output_geo.loc.x + output_geo.size.w) as f64);
                pos.y = pos.y.clamp(output_geo.loc.y as f64, (output_geo.loc.y + output_geo.size.h) as f64);

                let under = self.surface_under(pos);
                let surface_under = self.space.element_under(pos);
                if let Some((window, _)) = surface_under {
                    self.focus_window(window.clone());
                }

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                Backend::schedule_render(self);
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let output = self.space.outputs().next().unwrap();

                let output_geo = self.space.output_geometry(output).unwrap();

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let serial = SERIAL_COUNTER.next_serial();

                let pointer = self.seat.get_pointer().unwrap();

                let under = self.surface_under(pos);
                let surface_under = self.space.element_under(pointer.current_location());
                if let Some((window, _)) = surface_under {
                    self.focus_window(window.clone());
                }


                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
                Backend::schedule_render(self);
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let keyboard = self.seat.get_keyboard().unwrap();

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    if let Some((window, _loc)) = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, l)| (w.clone(), l))
                    {
                        self.space.raise_element(&window, true);
                        keyboard.set_focus(
                            self,
                            Some(window.toplevel().unwrap().wl_surface().clone()),
                            serial,
                        );
                        self.space.elements().for_each(|window| {
                            window.toplevel().unwrap().send_pending_configure();
                        });
                    } else if let Some(layer) = self.layer_under(pointer.current_location()) {
                        // A click landed on a layer-shell surface (panel, launcher, etc).
                        // Only OnDemand surfaces should be granted keyboard focus on click;
                        // surfaces with no keyboard interest (e.g. waybar) should just get
                        // the button event routed to them without disturbing focus, and
                        // Exclusive surfaces already grabbed focus on commit.
                        let interactivity = smithay::wayland::compositor::with_states(
                            layer.wl_surface(),
                            |states| {
                                states
                                    .cached_state
                                    .get::<smithay::wayland::shell::wlr_layer::LayerSurfaceCachedState>()
                                    .current()
                                    .keyboard_interactivity
                            },
                        );

                        if interactivity == smithay::wayland::shell::wlr_layer::KeyboardInteractivity::OnDemand {
                            keyboard.set_focus(self, Some(layer.wl_surface().clone()), serial);
                        }
                    } else {
                        self.space.elements().for_each(|window| {
                            window.set_activated(false);
                            window.toplevel().unwrap().send_pending_configure();
                        });
                        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                    }
                };

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();

                let horizontal_amount = event
                    .amount(Axis::Horizontal)
                    .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.);
                let vertical_amount = event
                    .amount(Axis::Vertical)
                    .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.);
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }
}
