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
    order: HashMap<LayoutScope, Vec<WindowId>>,
    /// This field is to satisfy the typechecker on WindowRegistry::filter
    empty: Vec<WindowId>,
}

impl WindowRegistry {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            available_ids: Vec::new(),
            next_window_id: WindowId(0),
            order: HashMap::new(),
            empty: Vec::new(),
        }
    }

    pub fn insert(&mut self, info: WindowInfo) -> WindowId {
        let id = if let Some(id) = self.available_ids.pop() {
            id
        } else {
            self.next_window_id.next()
        };
        self.order.entry(LayoutScope {
            output: info.output,
            tag: info.tag,
        })
        .and_modify(|list| {
            list.push(id);
        })
        .or_insert(vec![id]);
        self.map.insert(id, info);
        id
    }

    pub fn remove(&mut self, id: WindowId) {
        self.map.remove(&id);
        for value in self.order.values_mut() {
            let mut index = None;
            for i in 0..value.len() {
                if value[i] == id {
                    index = Some(i);
                    break;
                }
            }
            if let Some(index) = index {
                value.remove(index);
                break;
            }
        }
        self.available_ids.push(id);
    }

    pub fn filter(&self, scope: &LayoutScope) -> impl Iterator<Item = WindowId> {
        let Some(ordering) = self.order.get(scope) else {
            return self.empty.iter().rev().cloned();
        };
        ordering.iter().rev().cloned()
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
