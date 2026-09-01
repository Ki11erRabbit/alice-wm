use std::collections::HashMap;

use smithay::{
    desktop::Window,
    reexports::wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface, Resource},
};

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
    pub fullscreen: bool,
    /// True for transient windows (those with an xdg_toplevel `parent` set) —
    /// dialogs like a GTK/Qt "Save As" file picker spawned from a browser.
    /// These are deliberately kept out of the tiling grid: see
    /// `Alice::relayout_single`/`apply_floating`.
    pub floating: bool,
}

impl WindowInfo {
    pub fn new(
        tag: TagId,
        output: OutputId,
        window: Window,
        floating: bool,
    ) -> Self {
        Self {
            tag,
            output,
            window,
            fullscreen: false,
            floating,
        }
    }
}

pub struct LayoutInfo {
    stack: Vec<WindowId>,
    focused_window: usize,
}

impl LayoutInfo {
    pub fn new(stack: Vec<WindowId>) -> Self {
        let focused_window = stack.len().saturating_sub(1);
        Self {
            stack,
            focused_window
        }
    }

    pub fn push(&mut self, id: WindowId) {
        self.stack.push(id);
        self.focused_window = self.stack.len().saturating_sub(1);
    }

    pub fn pop(&mut self) -> Option<WindowId> {
        let out = self.stack.pop();
        if self.focused_window >= self.stack.len() {
            self.focused_window = self.focused_window.saturating_sub(1);
        }
        out
    }

    pub fn focused(&self) -> Option<WindowId> {
        if self.stack.is_empty() {
            return None;
        }
        self.stack.get(self.focused_window).copied()
    }

    pub fn focus_up(&mut self) {
        self.focused_window = (self.focused_window + 1) % self.stack.len();
    }

    pub fn focus_down(&mut self) {
        let new = self.focused_window.saturating_sub(1);
        if new == self.focused_window {
            self.focused_window = self.stack.len() - 1;
        } else {
            self.focused_window = new;
        }
    }

    pub fn move_up(&mut self) {
        let (next, current) = if self.focused_window + 1 == self.stack.len() {
            (0, self.focused_window)
        } else {
            (self.focused_window + 1, self.focused_window)
        };
        self.stack.swap(next, current);
        self.focus_up();
    }

    pub fn move_down(&mut self) {
        let next = self.focused_window.saturating_sub(1);
        let next = if next == self.focused_window {
            self.stack.len() - 1
        } else {
            next
        };
        let (next, current) = (next, self.focused_window);
        self.stack.swap(next, current);
        self.focus_down();
    }

    pub fn remove_window(&mut self, id: WindowId) {
        let mut index = None;
        for i in 0..self.stack.len() {
            if self.stack[i] == id {
                index = Some(i);
                break;
            }
        }
        if let Some(index) = index {
            self.stack.remove(index);
            if self.focused_window >= self.stack.len() {
                self.focused_window = self.focused_window.saturating_sub(1);
            }
        }
    }

    pub fn change_focus(&mut self,id: WindowId) {
        for (i, stack_id) in self.stack.iter().enumerate() {
            if *stack_id == id {
                self.focused_window = i;
                break;
            }
        }
    }
}

pub struct WindowRegistry {
    map: HashMap<WindowId, WindowInfo>,
    available_ids: Vec<WindowId>,
    next_window_id: WindowId,
    order: HashMap<LayoutScope, LayoutInfo>,
    /// This field is to satisfy the typechecker on WindowRegistry::filter
    empty: Vec<WindowId>,
    focused_window: Option<WindowId>,
    /// O(1) surface -> window lookup, keyed by the toplevel's own wl_surface
    /// id. Without this, every `wl_surface::commit` — which fires on every
    /// single frame a client draws, including subsurfaces like video/canvas
    /// layers, not just toplevel window changes — had to linearly scan every
    /// mapped window (and previously did so three separate times per commit
    /// across compositor.rs/xdg_shell.rs/resize_grab.rs) just to find the
    /// same window, or determine there wasn't one.
    surface_index: HashMap<ObjectId, WindowId>,
}

impl WindowRegistry {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            available_ids: Vec::new(),
            next_window_id: WindowId(0),
            order: HashMap::new(),
            empty: Vec::new(),
            focused_window: None,
            surface_index: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
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
        .or_insert(LayoutInfo::new(vec![id]));
        if let Some(toplevel) = info.window.toplevel() {
            self.surface_index.insert(toplevel.wl_surface().id(), id);
        }
        self.map.insert(id, info);
        self.focused_window = Some(id);
        id
    }

    pub fn remove(&mut self, id: WindowId) {
        let info = self.map.remove(&id);
        self.available_ids.push(id);
        if let Some(info) = info {
            if let Some(toplevel) = info.window.toplevel() {
                self.surface_index.remove(&toplevel.wl_surface().id());
            }
            let Some(layout) = self.order.get_mut(&LayoutScope {
                output: info.output,
                tag: info.tag,
            }) else {
                return
            };

            layout.remove_window(id);
        }
        if self.focused_window == Some(id) {
            self.focused_window = None;
        }
    }

    pub fn filter(&self, scope: &LayoutScope) -> impl Iterator<Item = WindowId> {
        let Some(ordering) = self.order.get(scope) else {
            return self.empty.iter().rev().cloned();
        };
        ordering.stack.iter().rev().cloned()
    }

    pub fn find(&self, window: Window) -> Option<WindowId> {
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

    pub fn get_mut(&mut self,id: &WindowId) -> Option<&mut WindowInfo> {
        self.map.get_mut(id)
    }

    pub fn get_stack_mut(&mut self, scope: &LayoutScope) -> Option<&mut LayoutInfo> {
        self.order.get_mut(scope)
    }

    pub fn change_focus(&mut self, id: Option<WindowId>) {
        self.focused_window = id;
    }

    pub fn get_focused(&self) -> Option<&WindowInfo> {
        self.focused_window.and_then(|id| {
            self.get(&id)
        })
    }

    pub fn get_focused_mut(&mut self) -> Option<&mut WindowInfo> {
        self.focused_window.and_then(|id| {
            self.get_mut(&id)
        })
    }

    pub fn stack_entry(
        &mut self,
        scope: LayoutScope
    ) -> std::collections::hash_map::Entry<'_, LayoutScope, LayoutInfo> {
        self.order.entry(scope)
    }

    pub fn focused_window(&self) -> Option<WindowId> {
        self.focused_window
    }

    /// O(1): only matches a window whose *own* toplevel wl_surface is
    /// `surface` — i.e. this returns `None` (cheaply) for subsurface/popup
    /// commits, exactly like the old linear-scan version did, just without
    /// walking every window to find that out.
    pub fn find_by_surface(&self, surface: &WlSurface) -> Option<WindowId> {
        self.surface_index.get(&surface.id()).copied()
    }

    pub fn fullscreen_window_for_output(&self, scope: &LayoutScope) -> Option<Window> {
        let iter = self.filter(&scope);
        for id in iter {
            if let Some(window) = self.get(&id) {
                return Some(window.window.clone());
            }
        }
        None
    }
}
