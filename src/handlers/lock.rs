use smithay::{delegate_session_lock, output::Output, utils::SERIAL_COUNTER, wayland::session_lock::SessionLockHandler};

use crate::{Alice, state::backend::Backend};






impl<BackendData: Backend + 'static> SessionLockHandler for Alice<BackendData> {
    fn lock_state(&mut self) -> &mut smithay::wayland::session_lock::SessionLockManagerState {
         &mut self.session_lock_manager_state
    }

    fn lock(&mut self, confirmation: smithay::wayland::session_lock::SessionLocker) {
        self.locked = true;
        self.pending_locker = Some(confirmation);
        self.try_lock();
    }

    fn unlock(&mut self) {
        self.try_unlock();
    }

    fn new_surface(&mut self, surface: smithay::wayland::session_lock::LockSurface, output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            return
        };
        self.lock_surfaces.insert(output.clone(), surface.clone());
        if !self.locked {
            return
        }

        let focused_output = self.outputs.get_focused().output.clone();
        let already_focused = self.lock_focus_output.is_some();

        if output == focused_output || !already_focused {
            let serial = SERIAL_COUNTER.next_serial();
            self.seat.get_keyboard()
                .unwrap()
                .set_focus(self, Some(surface.wl_surface().clone()), serial);
            self.lock_focus_output = Some(output);
        }
    }
}

delegate_session_lock!(@<BackendData: Backend + 'static> Alice<BackendData>);
