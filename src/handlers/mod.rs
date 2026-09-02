mod compositor;
mod xdg_shell;
mod wlr_shell;
mod capture;

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
