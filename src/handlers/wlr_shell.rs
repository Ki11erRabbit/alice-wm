use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::PopupSurface;
use smithay::wayland::shell::wlr_layer::{Anchor, LayerSurfaceCachedState};
use smithay::{output::Output, wayland::shell::wlr_layer::WlrLayerShellHandler};
use smithay::desktop::{LayerSurface as DesktopLayerSurface, layer_map_for_output};
use smithay::utils::{Logical, Rectangle, Size};

use crate::Alice;
use crate::output::LayoutScope;

/// The wlr-layer-shell protocol expects the compositor to hint a real size on any
/// axis the surface has anchored to both opposing edges (it needs to know how much
/// space to fill). On an axis that isn't fully anchored, 0 tells the client "you
/// decide" (this is how a centered, content-sized popup like wofi is meant to work).
fn suggested_size(surface: &WlSurface, zone: Rectangle<i32, Logical>) -> Size<i32, Logical> {
    let anchor = with_states(surface, |states| {
        states
            .cached_state
            .get::<LayerSurfaceCachedState>()
            .current()
            .anchor
    });

    let width = if anchor.contains(Anchor::LEFT) && anchor.contains(Anchor::RIGHT) {
        zone.size.w
    } else {
        0
    };
    let height = if anchor.contains(Anchor::TOP) && anchor.contains(Anchor::BOTTOM) {
        zone.size.h
    } else {
        0
    };

    (width, height).into()
}




impl WlrLayerShellHandler for Alice {
    fn shell_state(&mut self) -> &mut smithay::wayland::shell::wlr_layer::WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: smithay::wayland::shell::wlr_layer::LayerSurface,
        output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        layer: smithay::wayland::shell::wlr_layer::Layer,
        namespace: String,
    )
    {
        let output = output
            .and_then(|output| {
                Output::from_resource(&output)
            })
            .unwrap_or_else(|| self.outputs.get_focused().output.clone());


        let Some(info) = self.outputs.get(&output.name()) else {
            return;
        };
        let id = info.id;
        let Some(tag) = self.outputs.get_focused_tag(id) else {
            return;
        };


        let desktop_surface = DesktopLayerSurface::new(surface, namespace);

        let mut map = layer_map_for_output(&output);
        if let Err(e) = map.map_layer(&desktop_surface) {
            eprintln!("failed to map layer surface: {e:?}");
            return
        }
        drop(map);

        self.layer_surfaces.insert(id, desktop_surface);
        self.relayout(Some(LayoutScope {
            output: id,
            tag,
        }));
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        let Some((output_id, surface)) = self.layer_surfaces.find(&surface) else {
            return;
        };

        let output = &self.outputs.get_id(output_id).output;

        let mut map = layer_map_for_output(output);
        map.unmap_layer(&surface.surface);
        drop(map);

        self.layer_surfaces.get_mut(&output_id)
            .map(|layers| layers.retain(|s| s.surface != surface.surface));

        let Some(tag) = self.outputs.get_focused_tag(output_id) else {
            return;
        };

        self.relayout(Some(LayoutScope {
            output: output_id,
            tag,
        }));
    }

}

pub fn handle_commit(state: &mut Alice, surface: &WlSurface) {
    let Some((output_id, info)) = state.layer_surfaces.find_by_wl_surface(surface) else {
        return;
    };

    let output = state.outputs.get_id(output_id).output.clone();
    let mut map = layer_map_for_output(&output);
    map.arrange();

    if !info.init_config {
        let zone = map.non_exclusive_zone();
        drop(map);

        let size = suggested_size(info.surface.wl_surface(), zone);
        info.surface.layer_surface().with_pending_state(|pending| {
            pending.size = Some(size);
        });
        info.surface.layer_surface().send_configure();
        info.init_config = true;
    } else {
        drop(map);
    }

    let Some(tag) = state.outputs.get_focused_tag(output_id) else {
        return;
    };
    state.relayout(Some(LayoutScope {
        output: output_id,
        tag,
    }));
}

smithay::delegate_layer_shell!(Alice);
