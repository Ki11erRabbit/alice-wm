use std::cell::Cell;
use std::collections::HashMap;

use smithay::{
    desktop::Window,
    reexports::wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface, Resource},
};

use crate::{layout::Rect, output::{LayoutScope, OutputId, TagId}};




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
    /// The (rect, fullscreen) this window's toplevel was last sent a
    /// `configure` for, via `apply_rects`/`apply_floating` — `None` until
    /// the first one goes out. Used to skip redundant configures: see the
    /// comment on `apply_rects` for why sending one unconditionally on
    /// every `relayout` call, even when nothing about this window actually
    /// changed, is what was flooding clients with new serials fast enough
    /// to crash them.
    pub last_configured: Option<(Rect, bool)>,
    /// The `wp_fractional_scale` value this window's surface tree was last
    /// actually told about, via `Alice::refresh_fractional_scale_for_output`.
    /// `None` until the first call. `Cell` rather than a plain field since
    /// that function only takes `&self` (it's called from deep inside the
    /// render path alongside a live `&mut` borrow of other `Alice` fields)
    /// but still needs to record what it just sent.
    ///
    /// Same idea as `last_configured` above: without this, that function
    /// walked every window's *entire* surface tree and poked its
    /// fractional-scale protocol object on *every single rendered frame*,
    /// forever, for as long as anything kept the output rendering — which
    /// a video playing does continuously. The output's real scale only
    /// ever changes on a mode/scale reconfiguration (rare), so nearly all
    /// of that work was pure repetition: comparing against this cached
    /// value lets the common case (unchanged scale) skip the surface walk
    /// and protocol call entirely instead of redoing it up to 144 times a
    /// second.
    pub last_fractional_scale: Cell<Option<f64>>,
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
            last_configured: None,
            last_fractional_scale: Cell::new(None),
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

    /// Swaps the stack positions of `a` and `b` (a no-op if either isn't
    /// actually in this stack, or they're the same window). Used by the
    /// Tablet layout's click-to-swap-with-master behavior (see
    /// `Alice::swap_to_master` in `state.rs`) — unlike `move_up`/
    /// `move_down`, which always swap with whichever slot is adjacent to
    /// the currently-focused one, this swaps two specific windows
    /// regardless of where either currently sits.
    ///
    /// Whichever of the two was already focused stays focused afterward
    /// — the swap moves it to a new stack index, but shouldn't change
    /// *which* window is focused.
    pub fn swap_ids(&mut self, a: WindowId, b: WindowId) {
        if a == b {
            return;
        }
        let Some(ai) = self.stack.iter().position(|&x| x == a) else { return };
        let Some(bi) = self.stack.iter().position(|&x| x == b) else { return };
        self.stack.swap(ai, bi);
        if self.focused_window == ai {
            self.focused_window = bi;
        } else if self.focused_window == bi {
            self.focused_window = ai;
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
            if let Some(layout) = self.order.get_mut(&LayoutScope {
                output: info.output,
                tag: info.tag,
            }) {
                layout.remove_window(id);
            }
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

    /// Every mapped window's info, regardless of output or tag — unlike
    /// `filter`, which is scoped to one `LayoutScope`. Used by
    /// `Alice::refresh_fractional_scale_for_output` to find "every window
    /// truly on this output" from our own tiling assignment (`WindowInfo
    /// ::output`) rather than from `Space`'s geometric bbox overlap, which
    /// a client's own CSD shadow margin can push across the seam between
    /// two adjacent outputs even though the window is unambiguously tiled
    /// on just one of them.
    pub fn iter(&self) -> impl Iterator<Item = &WindowInfo> {
        self.map.values()
    }

    pub fn get_stack_mut(&mut self, scope: &LayoutScope) -> Option<&mut LayoutInfo> {
        self.order.get_mut(scope)
    }

    pub fn change_focus(&mut self, id: Option<WindowId>) {
        self.focused_window = id;
    }

    /// The currently-focused window's id, if any. Lets a caller check
    /// "is this already the focused window" *before* doing any of the
    /// work `Alice::change_focus` does to actually focus one — see its
    /// use in `Alice::focus_window`.
    pub fn focused_id(&self) -> Option<WindowId> {
        self.focused_window
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

    /// The window occupying the layout's "master" slot for `scope` — the
    /// first non-floating window in render order (`filter`'s order,
    /// i.e. the raw stack *reversed* — see `filter`'s doc comment), which
    /// is exactly the window every `Layout::arrange_*` impl puts at
    /// index 0 (the big tile in `MasterStack`/`Tablet`, the first split
    /// in `Fibonacci`). Floating windows (dialogs) are skipped since
    /// they're excluded from the tiling grid entirely and never occupy
    /// that slot — see `Alice::relayout_single`'s `floating`/`windows`
    /// partition, which this mirrors.
    pub fn master(&self, scope: &LayoutScope) -> Option<WindowId> {
        self.filter(scope)
            .find(|id| !self.get(id).map(|w| w.floating).unwrap_or(false))
    }

    pub fn fullscreen_window_for_output(&self, scope: &LayoutScope) -> Option<Window> {
        let iter = self.filter(&scope);
        for id in iter {
            if let Some(window) = self.get(&id) && window.fullscreen {
                return Some(window.window.clone());
            }
        }
        None
    }
}
