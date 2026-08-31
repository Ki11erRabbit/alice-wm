use smithay::{
    delegate_xdg_shell,
    desktop::{find_popup_root_surface, get_popup_toplevel_coords, PopupKind, Window},
    input::{
        pointer::{Focus, GrabStartData as PointerGrabStartData},
        Seat,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            protocol::{wl_seat, wl_surface::WlSurface},
            Resource,
        },
    },
    utils::{Rectangle, Serial},
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use crate::{
    Alice, grabs::{MoveSurfaceGrab, ResizeSurfaceGrab}, output::{LayoutScope, TagId}, state::backend::{Backend, winit::WinitData}, window::WindowInfo
};

impl<BackendData: Backend + 'static> XdgShellHandler for Alice<BackendData> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let pointer_loc = self.seat.get_pointer().unwrap()
            .current_location();

        let info = match self.space.output_under(pointer_loc).next() {
            Some(output) => {
                let info = match self.outputs.get(&output.name()) {
                    Some(output) => output,
                    None => self.outputs.get_focused(),
                };
                info
            }
            None => {
                self.outputs.get_focused()
            }
        };
        let output = info.id;
        let focused_tag = match self.outputs.get_focused_tag(info.id) {
            Some(tag) => tag,
            None => TagId(0),
        };
        //eprintln!("new_toplevel: assigning to output={:?} tag={:?}", output.0, focused_tag.0);

        // A toplevel that declares a parent via xdg_toplevel.set_parent is a
        // transient/dialog window — e.g. the "Save As"/"Open" file picker a
        // browser spawns for a download or upload — rather than an
        // independent application window. Tiling these in with everything
        // else forces them (and shrinks their parent to make room) into an
        // arbitrary tile-sized rect they were never designed for, which is
        // exactly the kind of thing that looks like "the picker never
        // opened": it did map, just squeezed into a tile somewhere instead
        // of appearing as the small window it actually is. Keep them out of
        // the tiling grid entirely (see `relayout_single`/`apply_floating`).
        //
        // NOTE: we can't check `surface.parent()` here. `set_parent` is a
        // request the client sends on the xdg_toplevel object — which means
        // it can only be sent *after* xdg_surface::get_toplevel has created
        // that object and this `new_toplevel` handler has already run.
        // `parent()` is therefore always `None` at this point regardless of
        // what the client will go on to do; every dialog would silently get
        // tiled instead of floated. The floating flag is determined for
        // real on the surface's first commit instead (see `handle_commit`
        // below), by which point any `set_parent` the client sent has
        // already been applied. Start non-floating here; it gets corrected
        // (and relayout re-triggered) before the initial configure goes out.
        let floating = false;

        let window = Window::new_wayland_window(surface);

        let id = self.window_registry.insert(WindowInfo::new(focused_tag, info.id, window.clone(), floating));

        self.space.map_element(window.clone(), (0, 0), true);
        self.undo_all_fullscreen();

        self.relayout(Some(LayoutScope {
            output,
            tag: focused_tag,
        }));
        self.change_focus(id, window);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let surface = surface.wl_surface();

        self.remove_window(surface);

        // TODO: this should probably only be the output the window is on.
        self.relayout(None);
        BackendData::schedule_render(self);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        return;
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        return;
        let seat = Seat::from_resource(&seat).unwrap();

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == wl_surface)
                .unwrap()
                .clone();
            let initial_window_location = self.space.element_location(&window).unwrap();

            let grab = MoveSurfaceGrab {
                start_data,
                window,
                initial_window_location,
            };

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        return;
        let seat = Seat::from_resource(&seat).unwrap();

        let wl_surface = surface.wl_surface();

        if let Some(start_data) = check_grab(&seat, wl_surface, serial) {
            let pointer = seat.get_pointer().unwrap();

            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == wl_surface)
                .unwrap()
                .clone();
            let initial_window_location = self.space.element_location(&window).unwrap();
            let initial_window_size = window.geometry().size;

            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Resizing);
            });

            surface.send_pending_configure();

            let grab = ResizeSurfaceGrab::start(
                start_data,
                window,
                edges.into(),
                Rectangle::new(initial_window_location, initial_window_size),
            );

            pointer.set_grab(self, grab, serial, Focus::Clear);
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // TODO popup grabs
    }
}

// Xdg Shell
delegate_xdg_shell!(@<BackendData: Backend + 'static> Alice<BackendData>);

fn check_grab<BackendData: Backend + 'static>(
    seat: &Seat<Alice<BackendData>>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<Alice<BackendData>>> {
    let pointer = seat.get_pointer()?;

    // Check that this surface has a click grab.
    if !pointer.has_grab(serial) {
        return None;
    }

    let start_data = pointer.grab_start_data()?;

    let (focus, _) = start_data.focus.as_ref()?;
    // If the focus was for a different surface, ignore the request.
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

/// Should be called on `WlSurface::commit`.
///
/// `window`, when present, is `surface`'s own toplevel window — resolved
/// once by the caller (`compositor::commit`) via an O(1) index instead of
/// scanning every mapped window here on every single commit.
pub fn handle_commit<BackendData: Backend + 'static>(state: &mut Alice<BackendData>, surface: &WlSurface, window: Option<&Window>) {
    // Handle toplevel commits.
    if let Some(window) = window.cloned() {
        let window = window.clone();
        let toplevel = window.toplevel().unwrap();

        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        if !initial_configure_sent {
            // This is the surface's first commit. Any `set_parent` request
            // the client sent is guaranteed to have already been applied by
            // now (unlike at `new_toplevel` time — see the comment there),
            // so this is the first point we can trust `parent()`. Recompute
            // `floating` for real here and relayout if it changed, before
            // the initial configure (which carries the tiled-vs-floating
            // size hint) goes out to the client.
            let floating = toplevel.parent().is_some();
            if let Some(id) = state.window_registry.find_by_surface(surface) {
                if let Some(info) = state.window_registry.get_mut(&id) {
                    if info.floating != floating {
                        info.floating = floating;
                        let scope = LayoutScope {
                            output: info.output,
                            tag: info.tag,
                        };
                        state.relayout(Some(scope));
                    }
                }
            }

            toplevel.send_configure();
        }
    }

    // Handle popup commits.
    state.popups.commit(surface);
    if let Some(popup) = state.popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    // NOTE: This should never fail as the initial configure is always
                    // allowed.
                    xdg.send_configure().expect("initial configure failed");
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }
}

impl<BackendData: Backend + 'static> Alice<BackendData> {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().unwrap().wl_surface() == &root)
        else {
            return;
        };

        let output = self.space.outputs().next().unwrap();
        let output_geo = self.space.output_geometry(output).unwrap();
        let window_geo = self.space.element_geometry(window).unwrap();

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
