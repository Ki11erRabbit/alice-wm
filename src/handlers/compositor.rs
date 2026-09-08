use crate::{Alice, grabs::resize_grab, state::{ClientState, backend::{Backend, winit::WinitData}}};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    reexports::wayland_server::{
        Client, protocol::{wl_buffer, wl_surface::WlSurface}
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            self, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, get_parent, is_sync_subsurface
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
        if self.lock_surfaces.values().any(|ls| ls.wl_surface() == surface) {
        eprintln!(
            "commit: lock surface, has_buffer={}",
            compositor::with_states(surface, |states| {
                states.cached_state.get::<SurfaceAttributes>().current().buffer.is_some()
            })
        );
    }
        on_commit_buffer_handler::<Self>(surface);

        // Resolved once via the O(1) surface index and reused below, instead
        // of every downstream handler re-scanning every mapped window to
        // find (or rule out) the same window. This runs on every single
        // `wl_surface::commit` — including subsurface commits (video/canvas
        // layers redrawing at 60+ fps) that never match a window at all —
        // so a linear scan here is pure waste multiplied by client frame rate.
        //
        // `root_output` piggybacks on this same lookup: it's the one piece
        // of information that lets the `schedule_render_output` call below
        // target just the output this commit can actually affect, instead
        // of every connected output (see the doc comment on
        // `Backend::schedule_render_output`/its `udev` override for why
        // that fan-out matters — a video's own surface commits are exactly
        // the high-frequency case that makes it expensive on a multi-output
        // setup).
        let mut root = None;
        let mut root_output = None;
        if !is_sync_subsurface(surface) {
            let mut r = surface.clone();
            while let Some(parent) = get_parent(&r) {
                r = parent;
            }
            if let Some(info) = self.window_registry.find_by_surface(&r)
                .and_then(|id| self.window_registry.get(&id))
            {
                let window = info.window.clone();
                root_output = Some(info.output);
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

        match root_output {
            // We know exactly which output this surface lives on (the
            // overwhelmingly common case: any toplevel or subsurface
            // commit, including every video frame) — only that output
            // needs re-rendering.
            Some(output_id) => {
                let output = self.outputs.get_id(output_id).output.clone();
                BackendData::schedule_render_output(self, &output);
            }
            // No associated window (a cursor surface, a not-yet-mapped
            // popup, etc.) — fall back to the old "render everything"
            // behavior; these are rare enough that the fan-out cost
            // doesn't matter.
            None => BackendData::schedule_render(self),
        }
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
