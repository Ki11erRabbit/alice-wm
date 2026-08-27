use std::collections::HashMap;

use smithay::desktop::LayerSurface as DesktopLayerSurface;
use smithay::wayland::shell::wlr_layer::LayerSurface as WlrLayerSurface;

use crate::output::OutputId;




pub struct LayerRegistry {
    map: HashMap<OutputId, Vec<DesktopLayerSurface>>,
}

impl LayerRegistry {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn get(&self, id: &OutputId) -> Option<&Vec<DesktopLayerSurface>> {
        self.map.get(id)
    }

    pub fn get_mut(&mut self, id: &OutputId) -> Option<&mut Vec<DesktopLayerSurface>> {
        self.map.get_mut(id)
    }

    pub fn insert(&mut self, id: OutputId, surface: DesktopLayerSurface) {
        self.map.entry(id)
            .or_default()
            .push(surface);
    }

    pub fn find(&self, surface: &WlrLayerSurface) -> Option<(OutputId, DesktopLayerSurface)> {
        for (output_id, surfaces) in &self.map {
            if let Some(found) = surfaces.iter().find(|s| s.layer_surface() == surface) {
                return Some((*output_id, found.clone()))
            }
        }
        None
    }
}
