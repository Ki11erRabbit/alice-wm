pub mod backend;

use std::{ffi::OsString, sync::Arc};

use smithay::{
    backend::renderer::{Renderer, element::{AsRenderElements, RenderElement}}, desktop::{PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output, space::space_render_elements}, input::{Seat, SeatState, keyboard::{Keysym, ModifiersState}}, output::Output, reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic}, wayland_protocols::xdg::shell::server::xdg_toplevel, wayland_server::{
            Display, DisplayHandle, backend::{ClientData, ClientId, DisconnectReason}, protocol::wl_surface::WlSurface
        }
    }, utils::{Logical, Point, SERIAL_COUNTER}, wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::{wlr_layer::{self, WlrLayerShellState}, xdg::XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
    }
};

use crate::{CalloopData, config::{Action, Config, KeyPress, execute_lua_config}, layer::LayerRegistry, layout::Rect, output::{LayoutRegistry, LayoutScope, OutputId, OutputInfo, Outputs, TagId}, state::backend::{Backend, udev::UdevData, winit::WinitData}, window::{LayoutInfo, WindowId, WindowRegistry}};

pub struct Alice<BackendData: Backend + 'static> {
    pub backend_data: BackendData,

    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    pub window_registry: WindowRegistry,
    pub outputs: Outputs,
    pub layout_registry: LayoutRegistry,

    pub layer_surfaces: LayerRegistry,
    pub layer_shell_state: WlrLayerShellState,

    pub config: Config,
    pub done_autostart: bool,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Alice<BackendData>>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,

    pub seat: Seat<Self>,

    /// What the pointer cursor should currently look like, as last reported
    /// by the seat (default arrow, hidden, or a client-provided surface).
    pub cursor_status: smithay::input::pointer::CursorImageStatus,
}

impl<BackendData: Backend + 'static> Alice<BackendData> {
    pub fn new(
        backend: BackendData,
        event_loop: &mut EventLoop<CalloopData<BackendData>>,
        display: Display<Self>
    ) -> Self {
        let start_time = std::time::Instant::now();

        let dh: DisplayHandle  = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let popups = PopupManager::default();

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        // Notify clients that we have a keyboard, for the sake of the example we assume that keyboard is always present.
        // You may want to track keyboard hot-plug in real compositor.
        seat.add_keyboard(Default::default(), 200, 25).unwrap();

        // Notify clients that we have a pointer (mouse)
        // Here we assume that there is always pointer plugged in
        seat.add_pointer();

        // A space represents a two-dimensional plane. Windows and Outputs can be mapped onto it.
        //
        // Windows get a position and stacking order through mapping.
        // Outputs become views of a part of the Space and can be rendered via Space::render_output.
        let space = Space::default();

        let socket_name = Self::init_wayland_listener(display, event_loop);
        Self::export_activation_environment(&socket_name);

        // Get the loop signal, used to stop the event loop
        let loop_signal = event_loop.get_signal();

        let config = BackendData::make_config();

        let layer_shell_state = WlrLayerShellState::new::<Alice<BackendData>>(&dh);

        let mut out = Self {
            backend_data: backend,

            start_time,
            display_handle: dh,

            space,
            loop_signal,
            socket_name,

            window_registry: WindowRegistry::new(),
            outputs: Outputs::new(),
            layout_registry: LayoutRegistry::new(),

            layer_surfaces: LayerRegistry::new(),
            layer_shell_state,

            config,
            done_autostart: false,

            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            popups,
            seat,

            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
        };
        out.apply_keyboard_layout();
        out
    }

    fn init_wayland_listener(
        display: Display<Alice<BackendData>>,
        event_loop: &mut EventLoop<CalloopData<BackendData>>,
    ) -> OsString {
        // Creates a new listening socket, automatically choosing the next available `wayland` socket name.
        let listening_socket = ListeningSocketSource::new_auto().unwrap();

        // Get the name of the listening socket.
        // Clients will connect to this socket.
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                //eprintln!("accepting new client connection");
                match state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    Ok(_) => eprintln!("client inserted successfully"),
                    Err(err) => eprintln!("insert_client failed: {:?}", err),
                }
            })
            .expect("Failed to init the wayland event source.");

        // You also need to add the display itself to the event loop, so that client events will be processed by wayland-server.
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    //,eprintln!("dispatch_clients firing");
                    // Safety: we don't drop the display
                    unsafe {
                        display.get_mut().dispatch_clients(&mut state.state).unwrap();
                    }
                    let _ = state.display_handle.flush_clients();
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    /// Publishes WAYLAND_DISPLAY / XDG_CURRENT_DESKTOP into both our own
    /// process environment and the systemd user manager / D-Bus session
    /// activation environment.
    ///
    /// Setting `std::env::set_var` alone (which is all the udev/winit
    /// backend setup used to do) only affects *this* process and anything
    /// it `fork`s afterwards, e.g. autostart commands run via `Self::spawn`.
    /// It does nothing for services that are D-Bus-activated on demand,
    /// which is exactly how xdg-desktop-portal and its backends
    /// (xdg-desktop-portal-gtk, -wlr, ...) are started. Those inherit the
    /// systemd --user manager's environment as it was at login, which
    /// predates the compositor and so never contains our Wayland socket
    /// name. The portal process then has no display to connect to, so
    /// when an app asks it to open a file picker the D-Bus call succeeds
    /// but no window is ever created for us to show — the picker silently
    /// "does nothing" instead of erroring visibly.
    ///
    /// `dbus-update-activation-environment --systemd` pushes the named
    /// variables into both the D-Bus session bus's activation environment
    /// and the systemd --user manager's environment, so anything they
    /// spawn from here on (including the portal, launched lazily the
    /// first time an app calls it) sees them.
    fn export_activation_environment(socket_name: &OsString) {
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", socket_name);
            std::env::set_var("XDG_CURRENT_DESKTOP", "alice-wm");
        }

        let status = std::process::Command::new("dbus-update-activation-environment")
            .arg("--systemd")
            .arg("WAYLAND_DISPLAY")
            .arg("XDG_CURRENT_DESKTOP")
            .status();

        match status {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!(
                "dbus-update-activation-environment exited with {status}; \
                 D-Bus-activated services (e.g. xdg-desktop-portal) may not \
                 see WAYLAND_DISPLAY/XDG_CURRENT_DESKTOP"
            ),
            Err(err) => eprintln!(
                "failed to run dbus-update-activation-environment: {err}; \
                 D-Bus-activated services (e.g. xdg-desktop-portal) may not \
                 see WAYLAND_DISPLAY/XDG_CURRENT_DESKTOP. Is dbus installed \
                 and on PATH?"
            ),
        }
    }

    /// Finds the topmost layer-shell surface under `pos`, restricted to the
    /// given layers (checked in the order given). Callers that care about
    /// z-order relative to regular windows should pass just
    /// `[Overlay, Top]` (above windows) or `[Bottom, Background]` (below
    /// windows) rather than all four — see `surface_under` below, which
    /// does exactly that for hover/motion targeting.
    pub fn layer_under(&self, pos: Point<f64, Logical>, layers: &[wlr_layer::Layer]) -> Option<smithay::desktop::LayerSurface> {
        let output = self
            .space
            .outputs()
            .find(|o| {
                self.space
                    .output_geometry(o)
                    .map(|geo| geo.to_f64().contains(pos))
                    .unwrap_or(false)
            })?;
        let output_loc = self.space.output_geometry(output)?.loc.to_f64();
        let map = layer_map_for_output(output);
        let point = pos - output_loc;

        layers.iter().find_map(|layer| map.layer_under(*layer, point)).cloned()
    }

    /// Grants keyboard focus to a layer-shell surface that was just clicked,
    /// but only if it asked for on-demand focus: surfaces with no keyboard
    /// interest (e.g. a bar) just get the button event routed to them
    /// without disturbing focus, and Exclusive surfaces already grabbed
    /// focus on commit.
    pub fn focus_layer_on_demand(&mut self, layer: &smithay::desktop::LayerSurface, serial: smithay::utils::Serial) {
        let interactivity = smithay::wayland::compositor::with_states(layer.wl_surface(), |states| {
            states
                .cached_state
                .get::<wlr_layer::LayerSurfaceCachedState>()
                .current()
                .keyboard_interactivity
        });

        if interactivity == wlr_layer::KeyboardInteractivity::OnDemand {
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(layer.wl_surface().clone()), serial);
            }
        }
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self
            .space
            .outputs()
            .find(|o| {
                self.space
                    .output_geometry(o)
                    .map(|geo| geo.to_f64().contains(pos))
                    .unwrap_or(false)
            })?;
        let output_loc = self.space.output_geometry(output)?.loc.to_f64();
        let layers = layer_map_for_output(output);

        let under_layer = |layer: wlr_layer::Layer| {
            let l = layers.layer_under(layer, pos - output_loc)?;
            let layer_loc = layers.layer_geometry(l)?.loc.to_f64();
            l.surface_under(pos - output_loc - layer_loc, WindowSurfaceType::ALL)
                .map(|(s, p)| (s, Point::<f64, Logical>::new(p.x as f64 + layer_loc.x as f64 + output_loc.x, (p.y as f64 + layer_loc.y as f64 + output_loc.y).into())))
        };

        // Overlay and Top surfaces (bars, launchers, notifications) sit above windows.
        if let Some(hit) = under_layer(wlr_layer::Layer::Overlay).or_else(|| under_layer(wlr_layer::Layer::Top)) {
            return Some(hit);
        }

        if let Some((window, location)) = self.space.element_under(pos) {
            if let Some(hit) = window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
            {
                return Some(hit);
            }
        }

        // Bottom and Background surfaces (wallpapers, widgets) sit below windows.
        under_layer(wlr_layer::Layer::Bottom).or_else(|| under_layer(wlr_layer::Layer::Background))
    }

    /// Clamp a pointer position to the combined area of every mapped
    /// output, instead of to a single one. Motion handlers previously
    /// clamped to `self.space.outputs().next()` unconditionally, which
    /// pinned the cursor to whichever output happened to be first in the
    /// space's (arbitrary) iteration order and made it physically
    /// impossible to move onto any other output.
    ///
    /// This mirrors the approach used by other Smithay compositors: clamp
    /// x against the full span of all outputs, then clamp y against
    /// whichever output the clamped x actually falls under (falling back to
    /// leaving y untouched if it doesn't land on any output, e.g. in the
    /// gap between two outputs of different heights).
    pub fn clamp_to_outputs(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        if self.space.outputs().next().is_none() {
            return pos;
        }

        let min_x = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .map(|geo| geo.loc.x)
            .min()
            .unwrap_or(0);
        let max_x = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .map(|geo| geo.loc.x + geo.size.w)
            .max()
            .unwrap_or(0);
        let clamped_x = pos.x.clamp(min_x as f64, max_x as f64);

        let y_bounds = self
            .space
            .outputs()
            .filter_map(|o| self.space.output_geometry(o))
            .find(|geo| clamped_x >= geo.loc.x as f64 && clamped_x <= (geo.loc.x + geo.size.w) as f64)
            .map(|geo| (geo.loc.y, geo.loc.y + geo.size.h));

        let clamped_y = match y_bounds {
            Some((min_y, max_y)) => pos.y.clamp(min_y as f64, max_y as f64),
            None => pos.y,
        };

        (clamped_x, clamped_y).into()
    }

    /// Keep `outputs.focused_output` in sync with whichever output the
    /// pointer physically sits over — the "current" output that keybindings,
    /// new-window placement, and layer-shell fallback lookups use.
    ///
    /// This is distinct from `focus_output`, which is for keybinding-driven
    /// output switching and deliberately warps the pointer to the target
    /// output's center. Called on every pointer motion, this must do
    /// neither of those things — it only updates which output is
    /// considered "focused" as the cursor crosses between them, mirroring
    /// the window-level focus-follows-mouse behavior already applied via
    /// `focus_window`. Without this, moving the mouse onto another monitor
    /// visually moves the cursor there but leaves keybindings, new windows,
    /// and layer-shell surfaces with no explicit output still targeting
    /// whichever output was last focused via a keybinding.
    pub fn follow_pointer_output_focus(&mut self, pos: Point<f64, Logical>) {
        let Some(output_id) = self
            .outputs
            .iter()
            .find(|info| {
                self.space
                    .output_geometry(&info.output)
                    .map(|geo| geo.to_f64().contains(pos))
                    .unwrap_or(false)
            })
            .map(|info| info.id)
        else {
            return;
        };

        if self.outputs.get_focused().id != output_id {
            self.outputs.change_focus(output_id);
        }
    }

    /// Pass in an scope to target only that output
    pub fn relayout(&mut self, scope: Option<LayoutScope>) {
        if let Some(scope) = scope {
            let output = self.outputs.get_id(scope.output).clone();
            self.relayout_single(output);
            return;
        }

        let outputs = self.outputs.iter()
            .cloned()
            .collect::<Vec<_>>();

        for output in outputs {
            self.relayout_single(output);
        }
    }

    fn relayout_single(&mut self, output: OutputInfo) {
        let tag = self.outputs.get_focused_tag(output.id).unwrap_or(TagId(0));
        let area = self.usable_area(&output.output);

        let scope = LayoutScope {
            output: output.id,
            tag,
        };

        // Floating windows (currently: transient dialogs such as a
        // "Save As" file picker — see `WindowInfo::floating`) sit outside
        // the tiling grid entirely, so they're excluded before handing the
        // rest to the layout algorithm: a dialog shouldn't shrink/reshuffle
        // real application windows, and it shouldn't be shrunk/reshuffled
        // by them either.
        let (floating, windows): (Vec<WindowId>, Vec<WindowId>) = self.window_registry.filter(&scope)
            .partition(|id| self.window_registry.get(id).map(|w| w.floating).unwrap_or(false));

        if self.try_full_screen(area, &windows) {
            for id in &floating {
                self.apply_floating(*id, area);
            }
            BackendData::schedule_render(self);
            return;
        }

        let layout = self.layout_registry.get_layout(&scope);
        let rects = layout.arrange(area, &windows);
        //,eprintln!("relayout_single: area={:?} windows={} rects={:?}", area, windows.len(), rects);

        for (id, rect) in windows.iter().zip(rects) {
            self.apply_rects(*id, rect);
        }
        for id in &floating {
            self.apply_floating(*id, area);
        }

        BackendData::schedule_render(self);
    }

    fn apply_rects(&mut self, id: WindowId, rect: Rect) {
        let Some(window) = self.window_registry.get(&id) else {
            return;
        };
        window.window.toplevel().unwrap().with_pending_state(|state| {
            state.size = Some((rect.width, rect.height).into());
            if window.fullscreen {
                state.states.set(xdg_toplevel::State::Fullscreen)
            } else {
                state.states.unset(xdg_toplevel::State::Fullscreen)
            }
        });
        window.window.toplevel().unwrap().send_configure();
        self.space.map_element(window.window.clone(), (rect.x, rect.y), false);
        //,eprintln!("apply_rects: window {:?} -> {:?}", id, rect);
    }

    /// Places a floating window (see `WindowInfo::floating`) centered
    /// within `area`, at its own size rather than a forced tile rect.
    ///
    /// The configure sent has no size — 0 on both axes is the standard
    /// "you choose" hint for a toplevel — so the client (a dialog, in the
    /// common case) keeps using whatever size it actually wants instead of
    /// being stretched or squeezed to fit a tile. Before the client has
    /// committed any real content, `geometry()` reports a default/empty
    /// size; a reasonable fixed fallback is used for that first placement
    /// so it isn't pinned into a corner at 0x0 in the meantime.
    fn apply_floating(&mut self, id: WindowId, area: Rect) {
        let Some(window) = self.window_registry.get(&id) else {
            return;
        };

        let geo = window.window.geometry();
        let (w, h) = if geo.size.w > 0 && geo.size.h > 0 {
            (geo.size.w, geo.size.h)
        } else {
            (640, 480)
        };

        window.window.toplevel().unwrap().with_pending_state(|state| {
            state.size = None;
            state.states.unset(xdg_toplevel::State::Fullscreen);
        });
        window.window.toplevel().unwrap().send_configure();

        let x = area.x + (area.width - w).max(0) / 2;
        let y = area.y + (area.height - h).max(0) / 2;
        self.space.map_element(window.window.clone(), (x, y), false);
    }


    /// The area windows can be tiled into for this output, in **global**
    /// (`Space`) coordinates: the layer-shell reserved zone (panels, bars)
    /// shifted by wherever this output actually sits in the layout (see
    /// `Space::map_output`/`output_position` in config.lua). `LayerMap`'s
    /// non-exclusive zone is always reported relative to the output's own
    /// origin, so on any output that isn't sitting at global (0, 0) this
    /// offset must be added back in — otherwise every output's windows get
    /// tiled into the same region near the space's origin instead of onto
    /// the output they actually belong to.
    fn usable_area(&mut self, output: &Output) -> Rect {
        let map = layer_map_for_output(output);
        let zone = map.non_exclusive_zone();
        drop(map);

        let output_loc = self.space.output_geometry(output)
            .map(|geo| geo.loc)
            .unwrap_or_default();

        Rect {
            x: zone.loc.x + output_loc.x,
            y: zone.loc.y + output_loc.y,
            width: zone.size.w,
            height: zone.size.h,
        }
    }

    fn try_full_screen(&mut self, area: Rect, windows: &[WindowId]) -> bool {
        let mut first_fullscreen = None;

        for id in windows.iter().rev() {
            let Some(window) = self.window_registry.get(id) else {
                continue;
            };
            if window.fullscreen {
                first_fullscreen = Some(*id);
                break;
            }
        }

        let Some(window) = first_fullscreen else {
            return false;
        };

        for id in windows {
            if *id == window {
                self.apply_rects(window, area);
                continue
            }
            let Some(window) = self.window_registry.get(id) else {
                continue;
            };
            self.space.unmap_elem(&window.window);
        }

        true
    }

    /// Focus the nearest output in `direction` relative to the currently
    /// focused output, based on each output's position in global (layout)
    /// space (i.e. wherever `Space::map_output` placed it).
    pub fn focus_output_direction(&mut self, direction: crate::output::Direction) -> Option<()> {
        let id = self.select_output_direction(direction)?;
        self.focus_output(id);
        Some(())
    }

    pub fn select_output_direction(&self, direction: crate::output::Direction) -> Option<OutputId> {
        use crate::output::Direction;

        let focused = self.outputs.get_focused();
        let current_id = focused.id;
        let current_geo = self.space.output_geometry(&focused.output)?;

        // Track the best candidate as (id, primary distance along the travel
        // axis, secondary distance on the cross axis). Smaller is better on
        // both, primary first: this prefers the closest output in that
        // direction, breaking ties by how well it lines up with the current
        // output (e.g. going "up" prefers an output directly above over one
        // that's up-and-far-to-the-side).
        let mut best: Option<(crate::output::OutputId, i32, i32)> = None;

        for info in self.outputs.iter() {
            if info.id == current_id {
                continue;
            }
            let Some(geo) = self.space.output_geometry(&info.output) else {
                continue;
            };

            let (is_candidate, primary_dist) = match direction {
                Direction::Left => (
                    geo.loc.x + geo.size.w <= current_geo.loc.x,
                    current_geo.loc.x - (geo.loc.x + geo.size.w),
                ),
                Direction::Right => (
                    geo.loc.x >= current_geo.loc.x + current_geo.size.w,
                    geo.loc.x - (current_geo.loc.x + current_geo.size.w),
                ),
                Direction::Up => (
                    geo.loc.y + geo.size.h <= current_geo.loc.y,
                    current_geo.loc.y - (geo.loc.y + geo.size.h),
                ),
                Direction::Down => (
                    geo.loc.y >= current_geo.loc.y + current_geo.size.h,
                    geo.loc.y - (current_geo.loc.y + current_geo.size.h),
                ),
            };

            if !is_candidate {
                continue;
            }

            let secondary_dist = match direction {
                Direction::Left | Direction::Right => {
                    axis_gap(current_geo.loc.y, current_geo.size.h, geo.loc.y, geo.size.h)
                }
                Direction::Up | Direction::Down => {
                    axis_gap(current_geo.loc.x, current_geo.size.w, geo.loc.x, geo.size.w)
                }
            };

            let is_better = match best {
                None => true,
                Some((_, best_primary, best_secondary)) => {
                    (primary_dist, secondary_dist) < (best_primary, best_secondary)
                }
            };

            if is_better {
                best = Some((info.id, primary_dist, secondary_dist));
            }
        }

        let (id, ..) = best?;
        Some(id)
    }

    /// Switch input focus to a specific output: updates which output is
    /// "focused" for tag/layout purposes, warps the pointer onto it, and
    /// hands keyboard focus to whichever window is focused on that output's
    /// active tag (or clears keyboard focus if the output has no windows).
    ///
    /// The pointer warp matters here, not just for feel: `new_toplevel`
    /// places new windows on whatever output the pointer is over, and
    /// pointer motion re-focuses whatever window is under the cursor. Without
    /// moving the pointer, changing `outputs.focused_output` alone is barely
    /// observable — the very next mouse move or spawned window would just
    /// snap back to the output the cursor is still sitting on.
    pub fn focus_output(&mut self, id: crate::output::OutputId) {
        if self.outputs.get_focused().id == id {
            return;
        }
        self.outputs.change_focus(id);

        let output = self.outputs.get_id(id).output.clone();
        if let Some(geo) = self.space.output_geometry(&output) {
            let center = Point::<f64, Logical>::new(
                (geo.loc.x + geo.size.w / 2) as f64,
                (geo.loc.y + geo.size.h / 2) as f64,
            );

            let under = self.surface_under(center);
            let element_under = self.space.element_under(center);
            if let Some((window, _)) = element_under {
                self.focus_window(window.clone());
            }

            let serial = SERIAL_COUNTER.next_serial();
            let time = self.start_time.elapsed().as_millis() as u32;
            let pointer = self.seat.get_pointer().unwrap();
            pointer.motion(self, under, &smithay::input::pointer::MotionEvent {
                location: center,
                serial,
                time,
            });
            pointer.frame(self);
        }

        let focused_window = self.outputs.get_focused_tag(id)
            .and_then(|tag| self.window_registry.get_stack_mut(&LayoutScope { output: id, tag }))
            .and_then(|stack| stack.focused())
            .and_then(|wid| self.window_registry.get(&wid).map(|info| (wid, info.window.clone())));

        match focused_window {
            Some((wid, window)) => {
                self.change_focus(wid, window);
            }
            None => {
                let keyboard = self.seat.get_keyboard().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Option::<WlSurface>::None, serial);
            }
        }
    }

    pub fn undo_all_fullscreen(&mut self) -> Option<()> {
        let tag = self.outputs.current_focused_tag()?;
        let output = self.outputs.get_focused().id;
        let scope = LayoutScope {
            output,
            tag,
        };

        let windows = self.window_registry.filter(&scope).collect::<Vec<_>>();
        for window in windows {
            let Some(info) = self.window_registry.get_mut(&window) else {
                continue;
            };
            info.fullscreen = false;
        }
        Some(())
    }

    pub fn spawn(&self, command: &str) {
        let socket_name = self.socket_name.clone();
        std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env("WAYLAND_DISPLAY", socket_name)
            .spawn()
            .ok();
    }

    pub fn get_window(&self, surface: &WlSurface) -> Option<(WindowId, Window)> {
        let window = self.space.elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface()) == Some(surface))
            .cloned()?;

        self.window_registry.find(window.clone())
            .map(|id| (id, window))
    }

    pub fn remove_window(&mut self, surface: &WlSurface) {
        let Some((id, window)) = self.get_window(surface) else {
            return;
        };

        let was_focused = self.window_registry.focused_window() == Some(id);
        let scope_info = self.window_registry.get(&id).map(|i| (i.output, i.tag));

        self.window_registry.remove(id);
        self.space.unmap_elem(&window);

        if !was_focused {
            return;
        }

        // Try to hand focus to whatever's next on the same output/tag.
        if let Some((output, tag)) = scope_info {
            let next = self.window_registry
                .get_stack_mut(&LayoutScope { output, tag })
                .and_then(|s| s.focused());

            if let Some(next_id) = next {
                if let Some(next_window) = self.window_registry.get(&next_id).map(|i| i.window.clone()) {
                    self.change_focus(next_id, next_window);
                    return;
                }
            }
        }

        // Nothing left to focus on this scope — explicitly release keyboard focus
        // instead of leaving it dangling on the surface we just destroyed.
        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
    }

    pub fn focus_window(&mut self, window: Window) {
        let Some(id) = self.window_registry.find(window.clone()) else {
            return;
        };
        self.change_focus(id, window.clone());
    }

    pub fn change_focus(&mut self, id: WindowId, window: Window) {
        let Some(info) = self.window_registry.get(&id) else {
            return;
        };
        let fullscreen = info.fullscreen;
        let output = info.output;
        let tag = info.tag;

        let Some(stack) = self.window_registry.get_stack_mut(&LayoutScope {
            output,
            tag,
        }) else {
            return;
        };

        if fullscreen {
            stack.move_down();
        }
        stack.change_focus(id);
        self.window_registry.change_focus(Some(id));

        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(
            self,
            window.toplevel().map(|surface| surface.wl_surface().clone()),
            serial
        );
        if fullscreen {
            self.relayout(Some(LayoutScope {
                output,
                tag
            }));
        }

    }

    pub fn focus_up(&mut self) -> Option<()> {
        let info = self.window_registry.get_focused()?;
        let window = info.window.clone();
        let stack = self.window_registry.get_stack_mut(&LayoutScope {
            output: info.output,
            tag: info.tag,
        })?;

        stack.focus_up();
        let id = stack.focused()?;
        self.change_focus(id, window);
        Some(())
    }

    pub fn focus_down(&mut self) -> Option<()> {
        let info = self.window_registry.get_focused()?;
        let window = info.window.clone();
        let stack = self.window_registry.get_stack_mut(&LayoutScope {
            output: info.output,
            tag: info.tag,
        })?;

        stack.focus_down();
        let id = stack.focused()?;
        self.change_focus(id, window);
        Some(())
    }

    pub fn move_up(&mut self) -> Option<()> {
        let info = self.window_registry.get_focused()?;
        let window = info.window.clone();
        let output = info.output;
        let tag = info.tag;
        let stack = self.window_registry.get_stack_mut(&LayoutScope {
            output: info.output,
            tag: info.tag,
        })?;

        stack.move_up();
        let id = stack.focused()?;
        self.change_focus(id, window);
        self.relayout(Some(LayoutScope {
            output,
            tag,
        }));
        Some(())
    }

    pub fn move_down(&mut self) -> Option<()> {
        let info = self.window_registry.get_focused()?;
        let window = info.window.clone();
        let output = info.output;
        let tag = info.tag;
        let stack = self.window_registry.get_stack_mut(&LayoutScope {
            output: info.output,
            tag: info.tag,
        })?;

        stack.move_down();
        let id = stack.focused()?;
        self.change_focus(id, window);
        self.relayout(Some(LayoutScope {
            output,
            tag,
        }));
        Some(())
    }

    pub fn change_tag(&mut self, tag: TagId) -> Option<()> {
        let old_tag = self.outputs.current_focused_tag()?;
        let output = self.outputs.get_focused().id;

        for id in self.window_registry.filter(&LayoutScope { output, tag: old_tag }) {
            if let Some(window) = self.window_registry.get(&id) {
                self.space.unmap_elem(&window.window);
            }
        }
        for id in self.window_registry.filter(&LayoutScope { output, tag }) {
            if let Some(window) = self.window_registry.get(&id) {
                self.space.map_element(window.window.clone(), (0, 0), false);
            }
        }
        self.outputs.change_tag(tag);

        let new_focus = self.window_registry
            .get_stack_mut(&LayoutScope { output, tag })
            .and_then(|s| s.focused());
        self.window_registry.change_focus(new_focus);

        self.relayout(Some(LayoutScope { output, tag }));
        Some(())
    }

    pub fn move_to_output(&mut self, direction: crate::output::Direction) -> Option<()> {
        let new_output_id = self.select_output_direction(direction)?;
        let info = self.window_registry.get_focused()?;
        let window = info.window.clone();
        let old_output = info.output;
        let old_tag = info.tag;
        self.space.unmap_elem(&window);
        let id = self.window_registry.find(window)?;
        let stack = self.window_registry.get_stack_mut(&LayoutScope {
            output: old_output,
            tag: old_tag,
        })?;

        stack.remove_window(id);

        let tag = self.outputs.get_focused_tag(new_output_id).unwrap_or(TagId(0));

        self.window_registry.stack_entry(LayoutScope { output: new_output_id, tag })
            .and_modify(|stack| { stack.push(id); })
            .or_insert(LayoutInfo::new(vec![id]));

        // Keep the window's own metadata in sync with which output/stack it
        // now lives in. Previously only `tag` was updated here, leaving
        // `window_info.output` pointing at the output the window just left
        // while the layout stacks already thought it belonged to the new
        // one — that split state, combined with never re-mapping the window
        // below, is what let it linger half-associated with both outputs.
        if let Some(window_info) = self.window_registry.get_mut(&id) {
            window_info.output = new_output_id;
            window_info.tag = tag;
        }

        self.focus_output(new_output_id);

        // Reflow the output the window left, so the remaining windows there
        // fill the gap, and the output it landed on, so it actually gets
        // mapped back into the space at its new position. Without this the
        // window stayed unmapped (from the `unmap_elem` above) until some
        // unrelated event happened to trigger a relayout touching one of
        // these scopes, which is what produced the flicker/"on two
        // displays" symptom.
        self.relayout(Some(LayoutScope { output: old_output, tag: old_tag }));
        self.relayout(Some(LayoutScope { output: new_output_id, tag }));
        Some(())
    }

    pub fn move_to_tag(&mut self, tag: TagId) -> Option<()> {
        let info = self.window_registry.get_focused()?;
        let window = info.window.clone();
        self.space.unmap_elem(&window);
        let id = self.window_registry.find(window)?;
        let output = info.output;
        let current_tag = info.tag;
        let stack = self.window_registry.get_stack_mut(&LayoutScope {
            output: info.output,
            tag: info.tag,
        })?;

        stack.remove_window(id);
        self.window_registry.stack_entry(LayoutScope { output, tag })
            .and_modify(|stack| { stack.push(id); })
            .or_insert(LayoutInfo::new(vec![id]));

        // Keep the window's own metadata in sync with which stack it now lives in.
        if let Some(window_info) = self.window_registry.get_mut(&id) {
            window_info.tag = tag;
        }

        self.change_tag(tag)?;
        Some(())
    }

    fn focus_next_tag(&mut self) -> Option<()> {
        let mut tag = self.outputs.current_focused_tag()?;
        if tag.0 != 8 {
            tag.0 += 1;
            self.change_tag(tag);
        }
        Some(())
    }

    fn focus_prevous_tag(&mut self) -> Option<()> {
        let mut tag = self.outputs.current_focused_tag()?;
        let new_tag = tag.0.saturating_sub(1);
        if tag.0 != new_tag {
            self.change_tag(TagId(new_tag));
        }
        Some(())
    }

    fn move_next_tag(&mut self) -> Option<()> {
        let mut tag = self.outputs.current_focused_tag()?;
        if tag.0 != 8 {
            tag.0 += 1;
            self.move_to_tag(tag);
        }
        Some(())
    }

    fn move_prevous_tag(&mut self) -> Option<()> {
        let mut tag = self.outputs.current_focused_tag()?;
        let new_tag = tag.0.saturating_sub(1);
        if tag.0 != new_tag {
            self.move_to_tag(TagId(new_tag));
        }
        Some(())
    }

    fn change_layout(&mut self, name: &str) -> Option<()> {
        let info = self.outputs.get_focused();
        let tag = self.outputs.get_focused_tag(info.id)?;

        let scope = LayoutScope {
            output: info.id,
            tag,
        };

        self.layout_registry.set_active(scope, name);
        Some(())
    }

    pub fn toggle_fullscreen(&mut self, id: WindowId) -> Option<()> {
        let window = self.window_registry.get_mut(&id)?;
        window.fullscreen = !window.fullscreen;
        let output = window.output;
        let tag = window.tag;

        self.relayout(Some(LayoutScope {
            output,
            tag,
        }));
        Some(())
    }

    /// Returns `true` if the keypress was handled
    pub fn try_handle_keypress(&mut self, mods: &ModifiersState, sym: Keysym) -> bool {
        let keypress = KeyPress::from((mods, sym));

        if let Some(action) = self.config.get_keypress(&keypress) {
            let action = action.clone();
            self.handle_action(action);
            true
        } else {
            false
        }
    }
    /// Re-applies the currently configured keyboard layout (`self.config`'s
    /// `KeyboardLayout`) to the live seat keyboard. Called after a config
    /// reload so `keyboard_layout(...)` changes take effect without
    /// restarting the compositor.
    fn apply_keyboard_layout(&mut self) {
        // Clone the layout out of `self.config` first so the `XkbConfig` we
        // build below borrows from this local instead of from `self`,
        // letting us still pass `&mut self` to `set_xkb_config`.
        let layout = self.config.keyboard_layout().clone();
        let xkb_config = layout.as_xkb_config();

        if let Some(keyboard) = self.seat.get_keyboard() {
            if let Err(err) = keyboard.set_xkb_config(self, xkb_config) {
                eprintln!("Failed to apply keyboard layout: {err}");
            }
        }
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::Quit => std::process::exit(0),
            Action::ReloadConfig => {
                self.config = BackendData::make_config();
                self.apply_keyboard_layout();
            }
            Action::Close => {
                let Some(info) = self.window_registry.get_focused() else {
                    return;
                };
                //,eprintln!("Close: closing window on tag={}", info.tag.0);
                if let Some(toplevel) = info.window.toplevel() {
                    toplevel.send_close();
                }
                let ids = self.window_registry.filter(&LayoutScope {
                    output: info.output,
                    tag: info.tag,
                }).collect::<Vec<_>>();
                for id in ids {
                    if Some(id) != self.window_registry.focused_window() {
                        self.window_registry.change_focus(Some(id));
                        break;
                    }
                }
            }
            Action::FullScreen => {
                self.window_registry.focused_window()
                    .and_then(|id| {
                        self.toggle_fullscreen(id)
                    });
            }
            Action::Spawn(command) => {
                self.spawn(&command);
            }
            Action::FocusTag(id) => {
                self.change_tag(id);
            }
            Action::MoveToTag(id) => {
                self.move_to_tag(id);
            }
            Action::FocusNextTag => {
                self.focus_next_tag();
            }
            Action::FocusPreviousTag => {
                self.focus_prevous_tag();
            }
            Action::MoveNextTag => {
                self.move_next_tag();
            }
            Action::MovePreviousTag => {
                self.move_prevous_tag();
            }
            Action::SetLayout(layout) => {
                _ = self.change_layout(&layout);
            }
            Action::FocusDownStack => {
                _ = self.focus_down();
            }
            Action::FocusUpStack => {
                _ = self.focus_up();
            }
            Action::MoveDownStack => {
                _ = self.move_down();
            }
            Action::MoveUpStack => {
                _ = self.move_up();
            }
            Action::FocusOutputLeft => {
                self.focus_output_direction(crate::output::Direction::Left);
            }
            Action::FocusOutputRight => {
                self.focus_output_direction(crate::output::Direction::Right);
            }
            Action::FocusOutputUp => {
                self.focus_output_direction(crate::output::Direction::Up);
            }
            Action::FocusOutputDown => {
                self.focus_output_direction(crate::output::Direction::Down);
            }
            Action::MoveOutputLeft => {
                self.move_to_output(crate::output::Direction::Left);
            }
            Action::MoveOutputRight => {
                self.move_to_output(crate::output::Direction::Right);
            }
            Action::MoveOutputUp => {
                self.move_to_output(crate::output::Direction::Up);
            }
            Action::MoveOutputDown => {
                self.move_to_output(crate::output::Direction::Down);
            }
        }
    }

    pub fn do_autostart_if_needed(&mut self) {
        if !self.done_autostart {
            for command in self.config.autostarts() {
                self.spawn(command);
            }
            self.done_autostart = true;
        }
    }
}

/// Returns 0 if the two 1D ranges `[a0, a0+al)` and `[b0, b0+bl)` overlap,
/// otherwise the gap between them. Used to prefer outputs that line up with
/// the current one on the axis perpendicular to the travel direction.
fn axis_gap(a0: i32, al: i32, b0: i32, bl: i32) -> i32 {
    let a1 = a0 + al;
    let b1 = b0 + bl;
    if a0 < b1 && b0 < a1 {
        0
    } else if b0 >= a1 {
        b0 - a1
    } else {
        a0 - b1
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        //,eprintln!("client {:?} initialized", client_id);
    }
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        //,eprintln!("client {:?} disconnected: {:?}", client_id, reason);
    }
}
