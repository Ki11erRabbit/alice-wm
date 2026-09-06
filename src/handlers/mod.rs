mod compositor;
mod xdg_shell;
mod wlr_shell;
mod capture;
mod lock;

use crate::Alice;
use crate::state::backend::Backend;

//
// Wl Seat
//

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::{delegate_data_device, delegate_output, delegate_seat};

impl<BackendData: Backend + 'static> SeatHandler for Alice<BackendData> {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Alice<BackendData>> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}

delegate_seat!(@<BackendData: Backend + 'static> Alice<BackendData>);

//
// Wl Data Device
//

impl<BackendData: Backend + 'static> SelectionHandler for Alice<BackendData> {
    type SelectionUserData = ();
}

impl<BackendData: Backend + 'static> DataDeviceHandler for Alice<BackendData> {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl<BackendData: Backend + 'static> ClientDndGrabHandler for Alice<BackendData> {}
impl<BackendData: Backend + 'static> ServerDndGrabHandler for Alice<BackendData> {}

delegate_data_device!(@<BackendData: Backend + 'static> Alice<BackendData>);

//
// Wl Output & Xdg Output
//

impl<BackendData: Backend + 'static> OutputHandler for Alice<BackendData> {}
delegate_output!(@<BackendData: Backend + 'static> Alice<BackendData>);

//
// Wp Viewporter & Wp Fractional Scale
//
// Together these give clients (notably Firefox/GTK) true fractional
// scaling. Without `wp_fractional_scale_v1` a client only sees the
// output's rounded-up *integer* `wl_output` scale, sizes its buffer for
// that integer scale, and the compositor then composites it back down
// using the real fractional scale — the two disagree, so the surface
// ends up larger than its allotted logical space and spills off-screen.
// `wp_viewporter` lets a client's buffer size and its displayed logical
// size differ cleanly, which fractional scaling relies on.

use smithay::{
    delegate_fractional_scale, delegate_viewporter,
    wayland::{
        compositor::{get_parent, with_states},
        fractional_scale::{with_fractional_scale, FractionalScaleHandler},
    },
};

impl<BackendData: Backend + 'static> FractionalScaleHandler for Alice<BackendData> {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // Pick a sensible initial scale for a surface that just bound the
        // protocol: prefer the output backing the window this surface
        // belongs to (walking up to the toplevel first, since this may be
        // a subsurface/popup), falling back to the first output known to
        // the compositor. Later frames correct this via
        // `Alice::refresh_fractional_scale_for_output`, called once per
        // rendered frame from both backends.
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        let output = self
            .window_registry
            .find_by_surface(&root)
            .and_then(|id| self.window_registry.get(&id))
            .and_then(|info| self.space.outputs_for_element(&info.window).first().cloned())
            .or_else(|| self.space.outputs().next().cloned());

        if let Some(output) = output {
            with_states(&surface, |states| {
                with_fractional_scale(states, |fractional_scale| {
                    fractional_scale.set_preferred_scale(output.current_scale().fractional_scale());
                });
            });
        }
    }
}

delegate_fractional_scale!(@<BackendData: Backend + 'static> Alice<BackendData>);
delegate_viewporter!(@<BackendData: Backend + 'static> Alice<BackendData>);
