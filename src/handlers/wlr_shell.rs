use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::shell::xdg::PopupSurface;
use smithay::{output::Output, wayland::shell::wlr_layer::WlrLayerShellHandler};
use smithay::desktop::{LayerSurface as DesktopLayerSurface, layer_map_for_output};

use crate::Alice;
use crate::output::LayoutScope;




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
    drop(map);

    if !info.init_config {
        info.surface.layer_surface().send_configure();
        info.init_config = true;
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
