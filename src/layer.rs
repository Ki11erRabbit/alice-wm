use std::collections::HashMap;

use smithay::desktop::{LayerSurface as DesktopLayerSurface, layer_map_for_output};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::shell::wlr_layer::LayerSurface as WlrLayerSurface;

use crate::Alice;
use crate::output::{LayoutScope, OutputId};

#[derive(Clone)]
pub struct LayerInfo {
    pub surface: DesktopLayerSurface,
    pub init_config: bool,
}


pub struct LayerRegistry {
    map: HashMap<OutputId, Vec<LayerInfo>>,
}

impl LayerRegistry {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn get(&self, id: &OutputId) -> Option<&Vec<LayerInfo>> {
        self.map.get(id)
    }

    pub fn get_mut(&mut self, id: &OutputId) -> Option<&mut Vec<LayerInfo>> {
        self.map.get_mut(id)
    }

    pub fn insert(&mut self, id: OutputId, surface: DesktopLayerSurface) {
        self.map.entry(id)
            .or_default()
            .push(LayerInfo {
                surface,
                init_config: false,
            });
    }

    pub fn find(&self, surface: &WlrLayerSurface) -> Option<(OutputId, LayerInfo)> {
        for (output_id, surfaces) in &self.map {
            if let Some(found) = surfaces.iter().find(|s| s.surface.layer_surface() == surface) {
                return Some((*output_id, found.clone()))
            }
        }
        None
    }

    pub fn find_by_wl_surface(&mut self, surface: &WlSurface) -> Option<(OutputId, &mut LayerInfo)> {
        for (output_id, surfaces) in &mut self.map {
            if let Some(found) = surfaces.iter_mut()
                .find(|s| s.surface.wl_surface() == surface) {
                return Some((*output_id, found))
            }
        }
        None
    }
}

