use crate::{Alice, grabs::resize_grab, state::{ClientState, backend::{Backend, winit::WinitData}}};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    reexports::wayland_server::{
        protocol::{wl_buffer, wl_surface::WlSurface},
        Client,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler, CompositorState,
        },
        shm::{ShmHandler, ShmState},
    },
};

use super::xdg_shell;
use super::wlr_shell;

impl<BackendData: Backend + 'static> CompositorHandler for Alice<BackendData> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // Resolved once via the O(1) surface index and reused below, instead
        // of every downstream handler re-scanning every mapped window to
        // find (or rule out) the same window. This runs on every single
        // `wl_surface::commit` — including subsurface commits (video/canvas
        // layers redrawing at 60+ fps) that never match a window at all —
        // so a linear scan here is pure waste multiplied by client frame rate.
        let mut root = None;
        if !is_sync_subsurface(surface) {
            let mut r = surface.clone();
            while let Some(parent) = get_parent(&r) {
                r = parent;
            }
            if let Some(window) = self.window_registry.find_by_surface(&r)
                .and_then(|id| self.window_registry.get(&id))
                .map(|info| info.window.clone())
            {
                window.on_commit();
                root = Some((r, window));
            }
        }
        // Downstream handlers only care about a window when `surface` is
        // that window's *own* toplevel surface (not some subsurface
        // underneath it) — mirrors the exact-equality check the old
        // per-handler scans used.
        let own_window = root.as_ref()
            .filter(|(r, _)| r == surface)
            .map(|(_, w)| w);

        xdg_shell::handle_commit(self, surface, own_window);
        resize_grab::handle_commit(&mut self.space, surface, own_window);
        wlr_shell::handle_commit(self, surface);

        BackendData::schedule_render(self);
    }
}

impl<BackendData: Backend + 'static> BufferHandler for Alice<BackendData> {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl<BackendData: Backend + 'static> ShmHandler for Alice<BackendData> {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(@<BackendData: Backend + 'static> Alice<BackendData>);
delegate_shm!(@<BackendData: Backend + 'static> Alice<BackendData>);
