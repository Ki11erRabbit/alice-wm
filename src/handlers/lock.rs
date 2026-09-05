use smithay::{delegate_session_lock, output::Output, wayland::session_lock::SessionLockHandler};

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
        self.lock_surfaces.insert(output, surface);
    }
}

delegate_session_lock!(@<BackendData: Backend + 'static> Alice<BackendData>);
