use std::collections::HashMap;

use smithay::desktop::Window;

use crate::output::{LayoutScope, OutputId, TagId};




#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct WindowId(pub u64);

impl WindowId {
    pub fn next(&mut self) -> WindowId {
        let out = *self;
        self.0 += 1;
        out
    }
}


pub struct WindowInfo {
    pub tag: TagId,
    pub output: OutputId,
    pub window: Window,
}

pub struct WindowRegistry {
    map: HashMap<WindowId, WindowInfo>,
    available_ids: Vec<WindowId>,
    next_window_id: WindowId,
}

impl WindowRegistry {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            available_ids: Vec::new(),
            next_window_id: WindowId(0),
        }
    }

    pub fn insert(&mut self, info: WindowInfo) -> WindowId {
        let id = if let Some(id) = self.available_ids.pop() {
            id
        } else {
            self.next_window_id.next()
        };
        self.map.insert(id, info);
        id
    }

    pub fn remove(&mut self, id: WindowId) {
        self.map.remove(&id);
        self.available_ids.push(id);
    }

    pub fn filter(&self, scope: &LayoutScope) -> impl Iterator<Item = WindowId> {
        self.map.iter()
            .filter_map(|(id, info)| {
                if info.tag == scope.tag && info.output == scope.output {
                    Some(*id)
                } else {
                    None
                }
            })
    }

    pub fn find(&mut self, window: Window) -> Option<WindowId> {
        let mut out_id = None;
        for (id, info) in self.map.iter() {
            if info.window == window {
                out_id = Some(*id);
                break;
            }
        }
        out_id
    }

    pub fn get(&self,id: &WindowId) -> Option<&WindowInfo> {
        self.map.get(id)
    }
}
