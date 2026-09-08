pub mod backend;

use std::{collections::{HashMap, HashSet}, ffi::OsString, sync::Arc, time::{Duration, Instant}};

use smithay::{
    backend::renderer::{Renderer, ImportAll, element::{AsRenderElements, RenderElement, surface::WaylandSurfaceRenderElement}}, desktop::{PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output, space::space_render_elements}, input::{Seat, SeatState, keyboard::{Keysym, ModifiersState}}, output::Output, reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic}, wayland_protocols::xdg::shell::server::xdg_toplevel, wayland_server::{
            Display, DisplayHandle, Resource, backend::{ClientData, ClientId, DisconnectReason, ObjectId}, protocol::{wl_output::WlOutput, wl_surface::WlSurface}
        }
    }, utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER}, wayland::{
        compositor::{CompositorClientState, CompositorState}, fractional_scale::FractionalScaleManagerState, output::OutputManagerState, selection::data_device::DataDeviceState, session_lock::{LockSurface, SessionLockManagerState, SessionLocker}, shell::{wlr_layer::{self, WlrLayerShellState}, xdg::XdgShellState}, shm::ShmState, socket::ListeningSocketSource, viewporter::ViewporterState
    }
};

use crate::{CalloopData, animation::{Animation, MorphFinish, ScaledElement, TagSlideAnimation, WindowMorph, morph_render_elements, shrink_target}, config::{Action, Config, KeyPress, execute_lua_config}, gesture::{ActiveGesture, GestureDirection, Resolved, ResolvedKind, COMMIT_THRESHOLD, DEAD_ZONE, GESTURE_DISTANCE, RELEASE_DURATION_MS}, handlers::workspace::WorkspaceManagerState, layer::LayerRegistry, layout::Rect, output::{LayoutRegistry, LayoutScope, OutputId, OutputInfo, Outputs, TagId}, state::backend::{Backend, udev::UdevData, winit::WinitData}, window::{LayoutInfo, WindowId, WindowRegistry}};

/// Which way a tag switch between `from` and `to` should slide, for
/// callers that only have two tag numbers and no other sense of
/// direction (a direct "go to tag N", rather than an explicit
/// next/previous key). Tags lower than where we are now slide as if
/// "next"; higher ones slide as if "previous" — see `Action::FocusTag`/
/// `Action::MoveToTag` for where this applies. `focus_next_tag` and
/// friends don't use this: they already know which way they're going.
fn tag_direction(from: TagId, to: TagId) -> i32 {
    if to.0 < from.0 { -1 } else { 1 }
}

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

    /// The non-exclusive zone (the tiling area left over after every
    /// layer-shell surface's exclusive-zone reservation, per
    /// `LayerMap::non_exclusive_zone`) last seen for each output — see
    /// `wlr_shell::handle_commit`, which is called on *every* commit from
    /// *any* layer-shell surface (a status bar's clock tick, a battery
    /// percentage updating, a systray icon redrawing, ...), the overwhelming
    /// majority of which don't actually change how much of the output is
    /// reserved. Without this cache, every one of those commits triggered a
    /// full `relayout` unconditionally regardless of whether the tiling
    /// area actually changed — and on the (rarer, but far from negligible)
    /// commits where a text-based panel's own committed width genuinely
    /// does shift by a pixel or two (its clock's digit count changing, a
    /// battery readout going from 2 digits to 3, ...), that unconditional
    /// relayout was a real, legitimate few-pixel reflow of every tiled
    /// window on that output — recurring roughly as often as the panel
    /// updates. This cache lets `handle_commit` skip the relayout
    /// entirely when the zone it just recomputed matches what's already
    /// stored here.
    pub layer_zone_cache: HashMap<OutputId, smithay::utils::Rectangle<i32, Logical>>,

    /// One in-flight tag-switch slide animation per output that currently
    /// has one running (see `slide_tag`/`advance_tag_animations` and
    /// `animation.rs`). An output with no key means no animation is
    /// running there — tag switches on it are instant.
    pub tag_animations: HashMap<OutputId, TagSlideAnimation>,

    /// One in-flight per-window box animation (open/close/reorder/reflow)
    /// per window that currently has one running — keyed by the window's
    /// own `wl_surface` id rather than our recycled `WindowId`, since a
    /// closing window's `WindowId` slot can be handed to a brand new
    /// window before its close animation finishes (see
    /// `start_window_close_morph`). See `animation.rs`'s `WindowMorph` and
    /// `morph_elements_for_output` for how these are actually drawn.
    pub window_morphs: HashMap<ObjectId, WindowMorph>,

    /// Exposes our tag system to `ext-workspace-v1` clients (waybar's
    /// `ext/workspaces` module, noctalia, etc). See `handlers/workspace.rs`.
    pub workspace_manager: WorkspaceManagerState,

    pub config: Config,
    pub done_autostart: bool,

    pub session_lock_manager_state: SessionLockManagerState,
    pub locked: bool,
    pub pending_locker: Option<SessionLocker>,
    pub lock_surfaces: HashMap<Output, LockSurface>,
    pub blanked_outputs: HashSet<Output>,
    pub lock_focus_output: Option<Output>,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Alice<BackendData>>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,

    /// `wp_viewporter` — lets clients (and our own fractional-scale
    /// handling below) present a surface at a destination size/region
    /// that differs from its buffer size, without the buffer itself
    /// needing to be an exact multiple of the output scale. This is a
    /// prerequisite for artifact-free fractional scaling: without it, a
    /// client scaling e.g. a 100x100 logical surface by 1.5 has to submit
    /// a 150x150 buffer, which then needs viewporter to be displayed at
    /// its intended logical size on outputs with a *different* scale.
    pub viewporter_state: ViewporterState,
    /// `wp_fractional_scale_v1` — tells supporting clients (e.g. Firefox)
    /// the output's *exact* fractional scale (1.5, 1.25, ...) instead of
    /// making them infer it from the integer `wl_output` scale, which is
    /// always rounded **up** (see `Scale::Fractional`'s docs). Without
    /// this, a client at a 1.5x output sees an advertised integer scale
    /// of 2, renders its buffer assuming 2x, and the compositor then
    /// composites that buffer back down using the real 1.5x scale — the
    /// buffer and the destination logical size no longer agree, so the
    /// surface renders oversized and spills past the output's edges.
    pub fractional_scale_manager_state: FractionalScaleManagerState,

    pub seat: Seat<Self>,

    /// What the pointer cursor should currently look like, as last reported
    /// by the seat (default arrow, hidden, or a client-provided surface).
    pub cursor_status: smithay::input::pointer::CursorImageStatus,

    /// The touchpad swipe currently in progress, if any — from
    /// `GestureSwipeBegin` until its matching `GestureSwipeEnd`. See
    /// `gesture.rs` and `gesture_begin`/`gesture_update`/`gesture_end`
    /// below.
    pub gesture: Option<ActiveGesture>,
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
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);

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

        let session_lock_manager_state = SessionLockManagerState::new::<Alice<BackendData>, _>(&dh, |_client| true);
        let workspace_manager = WorkspaceManagerState::new::<BackendData>(&dh);

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
            layer_zone_cache: HashMap::new(),
            tag_animations: HashMap::new(),
            window_morphs: HashMap::new(),
            workspace_manager,

            config,
            done_autostart: false,

            session_lock_manager_state,
            locked: false,
            pending_locker: None,
            lock_surfaces: HashMap::new(),
            blanked_outputs: HashSet::new(),
            lock_focus_output: None,

            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            popups,
            viewporter_state,
            fractional_scale_manager_state,
            seat,

            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),

            gesture: None,
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
                        if let Err(err) = display.get_mut().dispatch_clients(&mut state.state) {
                            // A single client's protocol violation or a
                            // socket error (e.g. it just segfaulted mid-write)
                            // must not bring down every other client's
                            // session. wayland-server already disconnects
                            // the offending client in this case; just log it
                            // and keep the loop running for everyone else.
                            eprintln!("dispatch_clients error (client disconnected): {}", err);
                        }
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

    /// Tells every mapped window's and layer-surface's `wp_fractional_scale`
    /// object (if the client created one) what `output`'s *exact*
    /// fractional scale currently is. Called once per rendered frame per
    /// output (see the `send_frame` loops in `render_surface`/`Redraw`),
    /// mirroring how those same call sites already keep frame callbacks
    /// current — the fractional scale needs the same continuous upkeep,
    /// since it can change whenever the output's configured scale changes
    /// or a window/layer moves onto a different output.
    ///
    /// This is a simplified stand-in for Smithay's `primary_scanout_output`
    /// tracking (see `anvil`'s `post_repaint`): rather than tracking which
    /// output most recently scanned out each surface, it just re-derives
    /// "is this window on `output` right now" from `Space` on every call
    /// via `outputs_for_element`, and only touches windows for which that's
    /// true. Cheaper alternatives (e.g. unconditionally updating every
    /// window regardless of `output`) cause every window's preferred scale
    /// to thrash between whichever outputs' scales differ, once per output
    /// per frame — see the filter below for what that did to Firefox.
    pub fn refresh_fractional_scale_for_output(&self, output: &Output) {
        let scale = output.current_scale().fractional_scale();

        // Which output "owns" this window — from our own tiling
        // assignment (`WindowInfo::output`), not from `Space`'s geometric
        // bbox overlap (`outputs_for_element`/`Space::refresh`). Those
        // agree almost always, but `Space`'s overlap is computed from
        // `bbox_with_popups()`, which includes a client's own CSD shadow
        // margin — often larger than this compositor's tiling gap. A
        // window tiled flush against the seam between two adjacent
        // outputs then has its (invisible) shadow bleed a few dozen
        // pixels into the neighboring output, and `Space` — correctly,
        // given that input — considers it present on both. That made
        // this function ping-pong the affected window's preferred scale
        // between both outputs' values every frame (see the git history
        // here for the diagnostic that caught it: a window's logged bbox
        // was consistently ~44px larger on every side than its geometry,
        // matching a shadow margin bigger than the 15px tiling gap).
        // `WindowInfo::output` has no such ambiguity — our own layout
        // assigned this window to exactly one output, full stop.
        let Some(output_id) = self.outputs.get(&output.name()).map(|info| info.id) else {
            return;
        };

        self.window_registry.iter()
            .filter(|info| info.output == output_id)
            .for_each(|info| {
                info.window.with_surfaces(|_, states| {
                    smithay::wayland::fractional_scale::with_fractional_scale(states, |fractional_scale| {
                        fractional_scale.set_preferred_scale(scale);
                    });
                });
            });

        if let Some(id) = self.outputs.get(&output.name()).map(|info| info.id) {
            if let Some(layers) = self.layer_surfaces.get(&id) {
                for layer in layers {
                    layer.surface.with_surfaces(|_, states| {
                        smithay::wayland::fractional_scale::with_fractional_scale(states, |fractional_scale| {
                            fractional_scale.set_preferred_scale(scale);
                        });
                    });
                }
            }
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

        if self.locked && let Some(surface) = self.lock_surfaces.get(&output) {
            return Some((surface.wl_surface().clone(), output_loc))
        }

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
        self.relayout_impl(scope, true);
    }

    /// Same as `relayout`, but skips the automatic reflow/grow/shrink
    /// animation this normally plays for every window whose rect
    /// changes (see `apply_rects`). Used only by `slide_tag_impl`, which
    /// is already animating the very same windows itself — as a whole
    /// tag sliding across the output — and would otherwise fight with a
    /// per-window reflow morph trying to animate those same windows to
    /// those same rects on the same frames.
    fn relayout_unanimated(&mut self, scope: Option<LayoutScope>) {
        self.relayout_impl(scope, false);
    }

    fn relayout_impl(&mut self, scope: Option<LayoutScope>, animate: bool) {
        if let Some(scope) = scope {
            let output = self.outputs.get_id(scope.output).clone();
            self.relayout_single(output, animate);
            return;
        }

        let outputs = self.outputs.iter()
            .cloned()
            .collect::<Vec<_>>();

        for output in outputs {
            self.relayout_single(output, animate);
        }
    }

    fn relayout_single(&mut self, output: OutputInfo, animate: bool) {
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

        if self.try_full_screen(&output.output, &windows) {
            for id in &floating {
                self.apply_floating(*id, area);
            }
            BackendData::schedule_render(self);
            return;
        }

        let layout = self.layout_registry.get_layout(&scope);
        let rects = if area.width >= area.height {
            layout.arrange_horizontal(area, &windows, self.config.gap_size(), self.config.tiling_config.clone())
        } else {
            layout.arrange_vertical(area, &windows, self.config.gap_size(), self.config.tiling_config.clone())
        };
        //eprintln!("[{:?}] relayout_single: output={:?} area={:?} windows={} rects={:?}", self.start_time.elapsed(), output.id.0, area, windows.len(), rects);

        for (id, rect) in windows.iter().zip(rects) {
            self.apply_rects(*id, rect, animate);
        }
        for id in &floating {
            self.apply_floating(*id, area);
        }

        BackendData::schedule_render(self);
    }

    fn apply_rects(&mut self, id: WindowId, rect: Rect, animate: bool) {
        let Some(window) = self.window_registry.get(&id) else {
            return;
        };

        // `relayout` re-derives and re-applies rects for every window in a
        // scope on all sorts of triggers that don't actually change any
        // individual window's target geometry (another window's commit,
        // a sibling being (re)mapped, a layer-shell surface adjusting its
        // exclusive zone, etc.). Unconditionally sending a fresh
        // `xdg_toplevel.configure` here regardless — as this used to —
        // means every one of those no-op relayouts hands out a brand new
        // configure serial to every window in the scope, whether or not
        // its size/state actually changed.
        //
        // Normally that's just wasteful. But when something drives
        // `relayout` at high frequency — video playback in one window
        // was observed doing this, likely via a layer-shell panel
        // reacting to media state on roughly every frame — it floods
        // *every other window sharing that scope* with new serials fast
        // enough that a client can fall behind acking them. When that
        // happens, Smithay's own xdg_shell validation sees an
        // `ack_configure` for a serial it no longer considers current and
        // posts a fatal `xdg_wm_base` "wrong configure serial" protocol
        // error — which is exactly what killed Zen here (see the
        // WAYLAND_DEBUG trace this was diagnosed from). The fix isn't to
        // relax that validation (it's correct per-spec); it's to stop
        // manufacturing configures nothing asked for. Skip sending one at
        // all when the target (rect, fullscreen) hasn't actually changed
        // since the last one we sent.
        //
        // That same `last_configured` bookkeeping doubles as exactly the
        // "where was this window before" this needs to decide whether
        // (and how) to animate — captured *before* it gets overwritten
        // below.
        let target = (rect, window.fullscreen);
        let previous_rect = window.last_configured.map(|(r, _)| r);
        if window.last_configured != Some(target) {
            window.window.toplevel().unwrap().with_pending_state(|state| {
                state.size = Some((rect.width, rect.height).into());
                if window.fullscreen {
                    state.states.set(xdg_toplevel::State::Fullscreen)
                } else {
                    state.states.unset(xdg_toplevel::State::Fullscreen)
                }
            });
            window.window.toplevel().unwrap().send_configure();
            if let Some(window) = self.window_registry.get_mut(&id) {
                window.last_configured = Some(target);
            }
        }

        let Some(window) = self.window_registry.get(&id) else {
            return;
        };
        let window_obj = window.window.clone();
        let output = window.output;

        if !animate {
            self.space.map_element(window_obj, (rect.x, rect.y), false);
            return;
        }

        // A window whose position is currently owned by some other
        // in-flight animation — a `WindowMorph` (open/close/reflow/
        // reorder, see `start_window_morph_impl`), or a `TagSlideAnimation`
        // sliding the tag it's on in or out (see `slide_tag_impl`) — has
        // deliberately been taken out of this function's control for the
        // duration; `advance_tag_animations`/`morph_elements_for_output`
        // are what's actually moving it, frame to frame, right now.
        //
        // As the big comment above explains, `relayout` — and so this
        // function — gets re-run constantly for reasons that don't
        // change any given window's target rect at all. Ordinarily
        // that's harmless: `previous_rect == Some(rect)` below just
        // falls through to a no-op re-map at the same spot. But for a
        // window one of those animations currently owns, "the same
        // spot" means its *resting* position — which is nowhere near
        // wherever the animation currently has it, and possibly not
        // even where the animation is instantaneously supposed to be at
        // all (rest is the value it's animating *toward*, not a
        // snapshot of "now"). Mapping it there directly would yank it
        // out from under the animation and snap it to rest immediately,
        // which is exactly what was making gesture-driven stack moves
        // (and, less often, tag slides) glitchy: any unrelated relayout
        // firing while a gesture was still being held — which, per the
        // comment above, happens all the time — would make the dragged
        // window instantly jump to its final position mid-drag. So:
        // skip the direct re-map entirely here and leave it to whichever
        // animation already owns it; it'll get mapped back into `Space`
        // itself once that animation actually finishes.
        let has_live_morph = window_obj.toplevel()
            .map(|toplevel| self.window_morphs.contains_key(&toplevel.wl_surface().id()))
            .unwrap_or(false);
        let tag_sliding = self.tag_animations.contains_key(&output);

        match previous_rect {
            // Brand new window (never positioned before): play the "grow"
            // animation in from a small, centered placeholder instead of
            // treating "nothing" -> "its tile" as an ordinary reflow —
            // but only if it's actually got a committed buffer to scale.
            // Before the client's first commit, `geometry()` is 0x0 (see
            // `apply_floating`'s comment on the same thing); animating
            // from a zero-sized reference produces no visible content at
            // all for the whole animation, rather than just skipping it.
            None if window_obj.geometry().size.w > 0 && window_obj.geometry().size.h > 0 => {
                self.start_window_morph(window_obj, output, shrink_target(rect), rect)
            }
            None => self.space.map_element(window_obj, (rect.x, rect.y), false),
            // An existing window whose rect actually changed — a sibling
            // opened/closed/moved, or this window itself got reordered in
            // the stack: reflow from wherever it was to wherever it's
            // going.
            Some(old) if old != rect => self.start_window_morph(window_obj, output, old, rect),
            // Rect didn't actually change, but something else already
            // owns this window's position for now (see the comment
            // above) — leave it alone rather than snapping it to rest.
            _ if has_live_morph || tag_sliding => {}
            // Rect didn't actually change, and nothing else has a claim
            // on this window's position either — nothing to animate.
            _ => self.space.map_element(window_obj, (rect.x, rect.y), false),
        }
        //eprintln!("[{:?}] apply_rects: window {:?} -> {:?} (animate={})", self.start_time.elapsed(), id, rect, animate);
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

        // Same dedup as `apply_rects` (see its comment for why this
        // matters): a floating window's configure never actually varies —
        // size is always "client's choice" (`None`) and fullscreen is
        // always unset here — so there's nothing to compare against a
        // *changing* target. This sentinel just marks "have we ever sent
        // this window its (only ever needed) floating configure", so
        // repeated `relayout` calls (e.g. every time this window is
        // recentered by a sibling mapping/closing) don't keep manufacturing
        // new serials for a configure whose content never differs from the
        // last one.
        const FLOATING_SENTINEL: Rect = Rect { x: 0, y: 0, width: 0, height: 0 };
        if window.last_configured != Some((FLOATING_SENTINEL, false)) {
            window.window.toplevel().unwrap().with_pending_state(|state| {
                state.size = None;
                state.states.unset(xdg_toplevel::State::Fullscreen);
            });
            window.window.toplevel().unwrap().send_configure();
            if let Some(window) = self.window_registry.get_mut(&id) {
                window.last_configured = Some((FLOATING_SENTINEL, false));
            }
        }

        let x = area.x + (area.width - w).max(0) / 2;
        let y = area.y + (area.height - h).max(0) / 2;
        let Some(window) = self.window_registry.get(&id) else {
            return;
        };
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

        let gap_size = self.config.gap_size();

        Rect {
            x: zone.loc.x + output_loc.x + gap_size,
            y: zone.loc.y + output_loc.y + gap_size,
            width: zone.size.w - gap_size * 2,
            height: zone.size.h - gap_size * 2,
        }
    }

    fn try_full_screen(&mut self, output: &Output, windows: &[WindowId]) -> bool {
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

        let Some(area) = self.space.output_geometry(output) else {
            return false;
        };

        let area = Rect {
            x: area.loc.x,
            y: area.loc.y,
            width: area.size.w,
            height: area.size.h,
        };

        for id in windows {
            if *id == window {
                // Entering/exiting fullscreen is a big, deliberate size
                // change of its own, not the kind of incidental reflow
                // `apply_rects`'s grow/reflow morph is meant for — keep it
                // instant rather than stretching a giant scale change
                // through the same 180ms animation.
                self.apply_rects(window, area, false);
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

    /// Removes the window backing `surface` and returns the
    /// `LayoutScope` (output + tag) it was on, so the caller can relayout
    /// just that scope instead of every output — see `toplevel_destroyed`,
    /// which used to relayout unconditionally on every close regardless of
    /// which output the closed window was even on.
    pub fn remove_window(&mut self, surface: &WlSurface) -> Option<LayoutScope> {
        let Some(id) = self.window_registry.find_by_surface(surface) else {
            return None;
        };
        let Some(info) = self.window_registry.get(&id) else {
            return None
        };
        let window = info.window.clone();
        let output = info.output;
        let tag = info.tag;
        let last_rect = info.last_configured.map(|(rect, _)| rect);

        let was_focused = self.window_registry.focused_window() == Some(id);
        let scope_info = self.window_registry.get(&id).map(|i| (i.output, i.tag));

        self.window_registry.remove(id);

        // Play the "new window" grow animation in reverse: shrink toward
        // the same small, centered placeholder it would have grown out of
        // if it were opening right now, and only actually unmap it from
        // `Space` once that finishes (`start_window_close_morph` handles
        // both — see `animation.rs`'s `WindowMorph`). It's already gone
        // from `window_registry` above, so layout for everyone else
        // (about to be triggered by the `relayout` call after this
        // returns, in `toplevel_destroyed`) reflows around the gap
        // immediately, concurrently with this window shrinking on top of
        // it. A window we never actually got a real rect for (closed
        // before ever being laid out) has nothing meaningful to shrink
        // from, so it just unmaps immediately instead.
        match last_rect {
            Some(rect) => self.start_window_close_morph(window, output, rect),
            None => self.space.unmap_elem(&window),
        }

        // Occupancy just changed for whichever tag this window was on —
        // update before any of the focus-handling paths below return early.
        self.broadcast_workspace_state();

        if !was_focused {
            return Some(LayoutScope { output, tag });
        }

        // Try to hand focus to whatever's next on the same output/tag.
        if let Some((output, tag)) = scope_info {
            let next = self.window_registry
                .get_stack_mut(&LayoutScope { output, tag })
                .and_then(|s| s.focused());

            if let Some(next_id) = next {
                if let Some(next_window) = self.window_registry.get(&next_id).map(|i| i.window.clone()) {
                    self.change_focus(next_id, next_window);
                    return Some(LayoutScope { output, tag });
                }
            }
        }

        // Nothing left to focus on this scope — explicitly release keyboard focus
        // instead of leaving it dangling on the surface we just destroyed.
        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        Some(LayoutScope { output, tag })
    }

    pub fn focus_window(&mut self, window: Window) {
        let Some(id) = self.window_registry.find(window.clone()) else {
            return;
        };
        // Focus-follows-mouse (see `input.rs`'s `PointerMotion` /
        // `PointerMotionAbsolute` handlers) calls this on *every* pointer
        // motion sample whose hit-test lands on a window — not just the
        // sample where the hovered window actually changes. Motion events
        // arrive far more often than that (a 1000Hz mouse, or just normal
        // movement across a large window, produces dozens of samples that
        // all resolve to the same already-focused window). Without this
        // check, every one of those redundantly re-ran `change_focus`
        // below: a new keyboard focus serial, a stack reshuffle, and —
        // worst case — if the hovered window is fullscreen, a full
        // `relayout` (which unconditionally ends in `schedule_render`,
        // forcing a whole extra composite/present). That last part means
        // simply moving the mouse over a fullscreen video or game was
        // enough to trigger a full relayout-and-redraw on every single
        // motion sample, which is a lot of avoidable work fighting for
        // the same frame budget the real rendering needs — worse the
        // higher the output's refresh rate, since there's less budget
        // per frame to begin with.
        if self.window_registry.focused_id() == Some(id) {
            return;
        }
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

    /// Tablet layout's smaller tiles double as a master-select: clicking
    /// one of them promotes it straight to the master slot instead of
    /// merely focusing it in place, demoting whatever was master into
    /// the slot the clicked window just vacated — the same "zoom to
    /// master" a lot of dynamic tiling WMs bind separately, just done on
    /// click since Tablet's whole point is a touch-friendly one-handed
    /// layout. See the `PointerButton` press handler in `input.rs` —
    /// it calls this first and only falls back to an ordinary focus
    /// click when this returns `false`.
    ///
    /// Returns `false` (having done nothing) if `window` isn't tiled
    /// here, isn't on a Tablet-layout scope, or is already master —
    /// callers should fall back to an ordinary focus click in that case.
    /// Otherwise this reorders the stack, focuses `window`, and
    /// relayouts — the same reflow-morph animation `move_up`/`move_down`
    /// get from `relayout` plays here automatically, just swapping the
    /// clicked tile with the master tile instead of adjacent slots.
    pub fn swap_to_master(&mut self, window: Window) -> bool {
        let Some(id) = self.window_registry.find(window.clone()) else {
            return false;
        };
        let Some(info) = self.window_registry.get(&id) else {
            return false;
        };
        if info.floating {
            // Dialogs sit outside the tiling grid entirely — clicking
            // one is an ordinary focus click, never a master-swap.
            return false;
        }
        let output = info.output;
        let tag = info.tag;
        let scope = LayoutScope { output, tag };

        if self.layout_registry.get_layout(&scope).name() != "Tablet" {
            return false;
        }

        let Some(master_id) = self.window_registry.master(&scope) else {
            return false;
        };

        if master_id == id {
            // Clicked the master tile itself — nothing to swap.
            return false;
        }

        let Some(stack) = self.window_registry.get_stack_mut(&scope) else {
            return false;
        };
        stack.swap_ids(id, master_id);
        self.change_focus(id, window);
        self.relayout(Some(scope));
        true
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

        self.apply_tag_focus(output, tag);
        self.relayout(Some(LayoutScope { output, tag }));
        self.broadcast_workspace_state();
        Some(())
    }

    /// Hands keyboard focus to whichever window is focused on `tag` (or
    /// clears focus if `tag` has no windows) — the bookkeeping half of a
    /// tag switch, shared by `change_tag` and `slide_tag`.
    ///
    /// Deliberately does *not* call `relayout` or `broadcast_workspace_state`
    /// itself: `change_tag` and `slide_tag` each need those at a different
    /// point relative to their own extra steps (`slide_tag` in particular
    /// must not relayout again after this, or it would snap the incoming
    /// windows straight to their resting position and skip the animation
    /// entirely — see the comment in `slide_tag`), so each calls them once,
    /// itself, in whichever order it needs.
    fn apply_tag_focus(&mut self, output: OutputId, tag: TagId) {
        let new_focus = self.window_registry
            .get_stack_mut(&LayoutScope { output, tag })
            .and_then(|s| s.focused())
            .and_then(|wid| self.window_registry.get(&wid).map(|info| (wid, info.window.clone())));

        match new_focus {
            Some((wid, window)) => {
                // Actually hand keyboard focus to the window, not just update
                // the registry's bookkeeping (see `Alice::change_focus` vs
                // `WindowRegistry::change_focus` — the latter only sets an
                // internal id and never touches the seat).
                self.change_focus(wid, window);
            }
            None => {
                self.window_registry.change_focus(None);
                let keyboard = self.seat.get_keyboard().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Option::<WlSurface>::None, serial);
            }
        }
    }

    /// Like `change_tag`, but animates the switch: instead of an instant
    /// unmap-old/map-new swap, the outgoing tag's windows and the incoming
    /// tag's windows slide across the output together, as one continuous
    /// strip, before the outgoing ones are finally unmapped.
    ///
    /// `direction` is `1` for "next tag" (the incoming tag slides in from
    /// the right) and `-1` for "previous tag" (from the left) — see
    /// `TagSlideAnimation::offset_at` for exactly how that's used. Callers
    /// that already know which way they're moving (`focus_next_tag`,
    /// `focus_prevous_tag`) pass it straight through; anything that only
    /// has a target `TagId` with no inherent direction should keep calling
    /// plain `change_tag` instead.
    pub fn slide_tag(&mut self, tag: TagId, direction: i32) -> Option<()> {
        self.slide_tag_impl(tag, direction, None)
    }

    /// The real implementation behind `slide_tag`. `excluded`, when set, is
    /// a window that's about to change tags (see `move_to_tag`) — it's
    /// left out of both the outgoing and incoming snapshots below, since
    /// it doesn't slide off/on with everyone else; the caller morphs it
    /// into place separately instead.
    fn slide_tag_impl(&mut self, tag: TagId, direction: i32, excluded: Option<WindowId>) -> Option<()> {
        let old_tag = self.outputs.current_focused_tag()?;
        if old_tag == tag {
            return Some(());
        }
        let output = self.outputs.get_focused().clone();

        // If a slide is already mid-flight on this output — e.g. the user
        // pressed the next-tag key twice in quick succession — snap it to
        // completion first. Starting a second animation on top of an
        // unfinished one would leave the first one's outgoing windows
        // mapped and sliding forever, never unmapped.
        self.finish_tag_animation(output.id);

        // Snapshot the outgoing tag's windows at wherever they currently
        // sit. Unlike `change_tag`, we deliberately do NOT unmap them here
        // — they need to stay visible, just sliding away, until the
        // animation finishes.
        let mut outgoing = Vec::new();
        for id in self.window_registry.filter(&LayoutScope { output: output.id, tag: old_tag }) {
            if Some(id) == excluded { continue; }
            let Some(info) = self.window_registry.get(&id) else { continue };
            let window = info.window.clone();
            let Some(loc) = self.space.element_location(&window) else { continue };
            outgoing.push((window, loc));
        }

        // Register the tag switch itself *before* laying out — `relayout`
        // always arranges whatever tag is currently focused, so this needs
        // to happen first or it would just re-arrange the tag we're
        // leaving.
        self.outputs.change_tag(tag);

        // Map the incoming windows at a (0, 0) placeholder — same first
        // step `change_tag` takes — then let the normal layout engine give
        // them their real, correct tiled positions.
        for id in self.window_registry.filter(&LayoutScope { output: output.id, tag }) {
            if let Some(info) = self.window_registry.get(&id) {
                self.space.map_element(info.window.clone(), (0, 0), false);
            }
        }
        // Unanimated: this whole tag switch is already one continuous
        // animation of its own (the off-screen-start override further
        // down), so the ordinary per-window grow/reflow morph
        // `apply_rects` would otherwise trigger here needs to stay out of
        // the way — see the comment there, and on `relayout_unanimated`.
        self.relayout_unanimated(Some(LayoutScope { output: output.id, tag }));

        // Read back the positions `relayout` just computed — this is each
        // incoming window's final, resting position for the animation to
        // slide *to*.
        let mut incoming = Vec::new();
        for id in self.window_registry.filter(&LayoutScope { output: output.id, tag }) {
            if Some(id) == excluded { continue; }
            let Some(info) = self.window_registry.get(&id) else { continue };
            let window = info.window.clone();
            let Some(loc) = self.space.element_location(&window) else { continue };
            incoming.push((window, loc));
        }

        self.apply_tag_focus(output.id, tag);
        // No `self.relayout(...)` here — see the comment on `apply_tag_focus`.
        // We already laid out `tag` above, before overriding positions
        // below; doing it again now would immediately snap the incoming
        // windows to their resting position and there'd be nothing left to
        // animate.
        self.broadcast_workspace_state();

        // Distance to slide: the output's own full width, not just the
        // usable/tiled area, so a window is always fully off-screen before
        // it's considered "arrived" regardless of panels/bars.
        let distance = self.space.output_geometry(&output.output)
            .map(|geo| geo.size.w)
            .unwrap_or(0);

        if distance == 0 {
            // Pathological case (output has no geometry yet): fall back to
            // an instant switch rather than animating nothing.
            for (window, _) in &outgoing {
                self.space.unmap_elem(window);
            }
            BackendData::schedule_render(self);
            return Some(());
        }

        // Move every incoming window off-screen, on the side it should
        // appear to slide in from, by offsetting the resting position we
        // just read back. This is exactly `TagSlideAnimation::offset_at`'s
        // formula at progress 0.0 — see its doc comment.
        for (window, final_pos) in &incoming {
            self.space.map_element(
                window.clone(),
                (final_pos.x + direction * distance, final_pos.y),
                false,
            );
        }

        self.tag_animations.insert(output.id, TagSlideAnimation {
            direction,
            distance,
            animation: Animation::new(Duration::from_millis(250)),
            old_tag,
            new_tag: tag,
            outgoing,
            incoming,
        });

        // Kick off the first render; see `advance_tag_animations` for how
        // this keeps itself rendering every subsequent frame until the
        // animation ends.
        BackendData::schedule_render(self);
        Some(())
    }

    /// Immediately stops whatever tag-switch animation is running on
    /// `output`, if any, and finalizes it — direction-aware, via
    /// `finalize_tag_animation` below. Used when a new slide interrupts
    /// an old one, so the old one doesn't linger half-finished forever.
    fn finish_tag_animation(&mut self, output: OutputId) {
        let Some(anim) = self.tag_animations.remove(&output) else {
            return;
        };
        let now = Instant::now();
        self.finalize_tag_animation(output, anim, now);
    }

    /// The actual "this animation is over, make the final state stick"
    /// logic, shared by `finish_tag_animation` (interrupted early) and
    /// `advance_tag_animations` (ran to its natural end) — both need
    /// exactly the same direction-aware finalize, and having two copies
    /// is exactly how this went wrong before: `finish_tag_animation`
    /// used to always assume "arrived at `new_tag`", which is only true
    /// for an ordinary switch. A gesture that got released back toward
    /// `old_tag` (see `Alice::gesture_end`'s `Tag` arm) is still
    /// *finishing* — just in the other direction — and interrupting it
    /// (by immediately swiping again, say) has to respect that, or the
    /// interrupted switch gets snapped to the wrong tag before the new
    /// one even starts, which is what produced the "plays in reverse"
    /// symptom.
    fn finalize_tag_animation(&mut self, output: OutputId, anim: TagSlideAnimation, now: Instant) {
        if anim.completed(now) {
            // Arrived at `new_tag`: outgoing tag unmapped for good,
            // incoming tag left at its resting position.
            for (window, _) in &anim.outgoing {
                self.space.unmap_elem(window);
            }
            for (window, final_pos) in &anim.incoming {
                self.space.map_element(window.clone(), *final_pos, false);
            }
        } else {
            // Never actually got there (or was on its way back): the
            // "incoming" tag never happened, so unmap it, put the
            // "outgoing" tag's windows back at the position they never
            // actually left, and restore the bookkeeping `slide_tag_impl`
            // changed up front (`outputs.change_tag`, focus) back to
            // `old_tag`.
            for (window, _) in &anim.incoming {
                self.space.unmap_elem(window);
            }
            for (window, base) in &anim.outgoing {
                self.space.map_element(window.clone(), *base, false);
            }
            self.outputs.change_tag(anim.old_tag);
            self.apply_tag_focus(output, anim.old_tag);
            self.broadcast_workspace_state();
        }
    }

    /// Advances every in-flight tag-switch animation by one frame: moves
    /// each animating window to its current position for `Instant::now()`,
    /// or — once an animation's duration has elapsed — finalizes it
    /// (unmap outgoing, snap incoming to rest) and drops it.
    ///
    /// This is the piece that turns a one-off `Space::map_element` call
    /// into something that looks animated at all: it needs to run once per
    /// rendered frame for as long as any animation is active. See the
    /// call sites in `winit.rs`/`udev.rs`'s render paths for how each
    /// backend arranges to keep calling this — the short version is that
    /// updating a window's position here produces damage, and damage is
    /// what makes both backends' existing render loops keep rendering on
    /// their own, so no separate animation timer is needed.
    pub fn advance_tag_animations(&mut self) {
        if self.tag_animations.is_empty() {
            return;
        }

        let now = Instant::now();
        // Taken out and reinserted rather than iterated in place: the loop
        // body needs `&mut self.space` (to move windows) at the same time
        // as read access to the animation being advanced, and those can't
        // both be field-projections of a `self` that's also mutably
        // borrowed by `self.tag_animations.iter_mut()`.
        let animations = std::mem::take(&mut self.tag_animations);

        for (output, anim) in animations {
            if anim.is_finished(now) {
                self.finalize_tag_animation(output, anim, now);
                // Not reinserted: this animation is done.
                continue;
            }

            let offset = anim.offset_at(now);
            let shift = anim.direction * anim.distance;
            for (window, base) in &anim.outgoing {
                self.space.map_element(window.clone(), (base.x + offset - shift, base.y), false);
            }
            for (window, final_pos) in &anim.incoming {
                self.space.map_element(window.clone(), (final_pos.x + offset, final_pos.y), false);
            }

            self.tag_animations.insert(output, anim);
        }
    }

    /// Starts (or smoothly redirects) a `WindowMorph` for `window`, animating
    /// its on-screen box from `from` to `to`. Used for the ordinary cases:
    /// growing in on open, and the reflow when a sibling
    /// opens/closes/reorders (see `apply_rects`). The close animation
    /// itself uses `start_window_close_morph` below instead, since it needs
    /// different finishing behavior.
    fn start_window_morph(&mut self, window: Window, output: OutputId, from: Rect, to: Rect) {
        self.start_window_morph_impl(window, output, from, to, MorphFinish::Remap);
    }

    /// Starts the reverse of the open animation: shrinks `window` from
    /// `from` toward the same small, centered placeholder it would have
    /// grown out of, then unmaps it for good — rather than remapping it —
    /// once that finishes. Called from `remove_window`, *before* the
    /// window is actually torn down, so there's still a real, on-screen
    /// box to shrink from.
    fn start_window_close_morph(&mut self, window: Window, output: OutputId, from: Rect) {
        let to = shrink_target(from);
        self.start_window_morph_impl(window, output, from, to, MorphFinish::Unmap);
    }

    fn start_window_morph_impl(
        &mut self,
        window: Window,
        output: OutputId,
        mut from: Rect,
        to: Rect,
        on_finish: MorphFinish,
    ) {
        // Keyed by the surface's own id, not our `WindowId` — see the doc
        // comment on `Alice::window_morphs` for why (a closing window's
        // `WindowId` can be recycled onto a brand new window before this
        // finishes).
        let Some(key) = window.toplevel().map(|t| t.wl_surface().id()) else {
            apply_morph_finish(&mut self.space, &window, to, on_finish);
            return;
        };

        let now = Instant::now();
        // Frozen once, here, rather than read fresh every render frame —
        // see the doc comment on `WindowMorph::base` for why re-reading
        // `window.geometry()` live was the actual jitter bug: a fast
        // client (Alacritty, Firefox) can commit an already-`to`-sized
        // buffer before this animation finishes, and using that as the
        // live reference would snap the scale factor mid-flight. If a
        // morph is already running for this window, keep whatever base
        // it already captured rather than re-reading `window.geometry()`
        // now — by this point the client may already have committed
        // toward the *previous* `to`, so the window's current geometry
        // may no longer reflect the size the in-flight animation has
        // actually been stretching from.
        let base = match self.window_morphs.get(&key) {
            Some(existing) => existing.base,
            None => window.geometry().size,
        };
        if let Some(existing) = self.window_morphs.get(&key) {
            // Already mid-animation — e.g. the stack got reordered again,
            // or a second close arrived, before the last one finished.
            // Continue smoothly from wherever it currently is rather than
            // snapping back to `from`.
            from = existing.current_rect(now);
        }

        if from == to {
            self.window_morphs.remove(&key);
            apply_morph_finish(&mut self.space, &window, to, on_finish);
            return;
        }

        // Excluded from `Space`'s normal per-element rendering for the
        // duration — see `morph_elements_for_output`, which draws this
        // window itself, scaled, instead.
        self.space.unmap_elem(&window);

        // `Space::unmap_elem` (see Smithay's implementation) synchronously
        // fires a real `wl_surface.leave` for every output this window
        // was on — and nothing sends the matching `enter` back until this
        // animation finishes and the window is remapped, up to 180ms
        // later. The window never actually left `output`: it's still
        // being drawn there the entire time, just via this scaled morph
        // path instead of `Space`'s own. But the client doesn't know
        // that — it just saw its window leave a monitor. Firefox in
        // particular treats an output-leave as a real monitor change
        // (different scale, different color profile, different vsync)
        // and tears down/renegotiates its rendering surface in response,
        // which is exactly the flicker/resize-loop that shows up on
        // every sibling open/close/reorder once more than one window is
        // tiled together.
        //
        // `Output::enter`/`leave` are idempotent (Smithay tracks a
        // per-output set of already-entered surfaces and only actually
        // sends the protocol event on a real state change), so
        // re-asserting "still here" immediately below — using the same
        // whole-bbox-on-one-output overlap `Space::refresh` itself would
        // compute for a window that doesn't straddle two outputs, which
        // covers every ordinary tiled reflow this path handles — collapses
        // into a same-batch leave+enter pair the client never gets a
        // chance to act on in between, instead of a real ~180ms absence.
        if let Some(output_obj) = self.outputs.iter().find(|info| info.id == output).map(|info| info.output.clone()) {
            let overlap = Rectangle::new((0, 0).into(), window.bbox_with_popups().size);
            smithay::desktop::space::SpaceElement::output_enter(&window, &output_obj, overlap);
        }

        self.window_morphs.insert(key, WindowMorph {
            window,
            output,
            animation: Animation::new(Duration::from_millis(180)),
            from,
            to,
            on_finish,
            base,
        });
        BackendData::schedule_render(self);
    }

    /// Sends the focused window to the neighboring output in `direction`
    /// and follows it there. Animates the same way `move_up`/`move_down`
    /// do: this just reorders which stack the window belongs to and
    /// calls `relayout` on both the output it left and the one it landed
    /// on, and `apply_rects`/`start_window_morph` pick up from there —
    /// the window's `last_configured` rect (still holding its *old*,
    /// pre-move position, since nothing here clears it) differs from the
    /// freshly tiled rect the new output's layout just computed for it,
    /// which is exactly the "reflow: from wherever it was to wherever
    /// it's going" case `apply_rects` already handles for an ordinary
    /// stack reorder. The only difference is the box it flies across
    /// this time spans two outputs' worth of distance instead of one —
    /// and since a `WindowMorph` belongs to (and is only drawn on) a
    /// single output (see `morph_elements_for_output`), that plays out
    /// as the window sliding in from off-screen on the *destination*
    /// output's edge nearest the source, rather than visibly crossing
    /// the physical gap between the two monitors. Bound to a gesture
    /// (see `gesture.rs`'s `ResolvedKind::Output`), the same swipe that
    /// triggers this also drives that slide live, 1:1 with the finger.
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

    /// Moves the focused window to `tag` and follows it there. `direction`
    /// picks which way everyone *else* on the two tags slides (see
    /// `slide_tag`/`tag_direction`) — the moved window itself doesn't
    /// slide with them; see the comment below.
    pub fn move_to_tag(&mut self, tag: TagId, direction: i32) -> Option<()> {
        let info = self.window_registry.get_focused()?;
        let window = info.window.clone();
        let output = info.output;
        let old_tag = info.tag;
        if old_tag == tag {
            return Some(());
        }
        let old_rect = info.last_configured.map(|(r, _)| r);

        let id = self.window_registry.find(window.clone())?;
        let stack = self.window_registry.get_stack_mut(&LayoutScope {
            output,
            tag: old_tag,
        })?;

        stack.remove_window(id);
        self.window_registry.stack_entry(LayoutScope { output, tag })
            .and_modify(|stack| { stack.push(id); })
            .or_insert(LayoutInfo::new(vec![id]));

        // Keep the window's own metadata in sync with which stack it now lives in.
        if let Some(window_info) = self.window_registry.get_mut(&id) {
            window_info.tag = tag;
        }

        // Everyone else on the old and new tags slides off/on exactly like
        // an ordinary next/previous tag switch. This window is excluded
        // from that slide (`Some(id)`, below) — it doesn't leave the
        // screen and come back, it just changes shape — and is carried
        // separately right after instead.
        self.slide_tag_impl(tag, direction, Some(id))?;

        // Read back the rect the relayout inside `slide_tag_impl` just
        // computed for this window on its new tag, and reshape it into
        // that spot from wherever it was before.
        let new_rect = self.window_registry.get(&id).and_then(|i| i.last_configured).map(|(r, _)| r);
        if let (Some(old_rect), Some(new_rect)) = (old_rect, new_rect) {
            self.start_window_morph(window, output, old_rect, new_rect);
        }

        Some(())
    }

    fn focus_next_tag(&mut self) -> Option<()> {
        let mut tag = self.outputs.current_focused_tag()?;
        if tag.0 != 8 {
            tag.0 += 1;
            self.slide_tag(tag, 1);
        }
        Some(())
    }

    fn focus_prevous_tag(&mut self) -> Option<()> {
        let mut tag = self.outputs.current_focused_tag()?;
        let new_tag = tag.0.saturating_sub(1);
        if tag.0 != new_tag {
            self.slide_tag(TagId(new_tag), -1);
        }
        Some(())
    }

    fn move_next_tag(&mut self) -> Option<()> {
        let mut tag = self.outputs.current_focused_tag()?;
        if tag.0 != 8 {
            tag.0 += 1;
            self.move_to_tag(tag, 1);
        }
        Some(())
    }

    fn move_prevous_tag(&mut self) -> Option<()> {
        let mut tag = self.outputs.current_focused_tag()?;
        let new_tag = tag.0.saturating_sub(1);
        if tag.0 != new_tag {
            self.move_to_tag(TagId(new_tag), -1);
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

        const VT_SWITCH_1: u32 = Keysym::XF86_Switch_VT_1.raw();
        const VT_SWITCH_12: u32 = Keysym::XF86_Switch_VT_12.raw();
        if sym.raw() >= VT_SWITCH_1 && sym.raw() <= VT_SWITCH_12 {
            let vt = (sym.raw() - VT_SWITCH_1 + 1) as i32;
            self.backend_data.change_vt(vt);
            return true;
        }
        let keypress = KeyPress::from((mods, sym));

        if self.locked && let Some(action) = self.config.get_lock_keypress(&keypress) {
            let action = action.clone();
            self.handle_action(action);
            true

        } else if !self.locked && let Some(action) = self.config.get_keypress(&keypress) {
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
                self.execute_commands();
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
                // "Go to tag N" has no inherent direction the way
                // next/previous does, so it borrows one from where `id`
                // sits relative to the tag we're currently on — see
                // `tag_direction`.
                let direction = self.outputs.current_focused_tag()
                    .map(|current| tag_direction(current, id))
                    .unwrap_or(1);
                self.slide_tag(id, direction);
            }
            Action::MoveToTag(id) => {
                let direction = self.outputs.current_focused_tag()
                    .map(|current| tag_direction(current, id))
                    .unwrap_or(1);
                self.move_to_tag(id, direction);
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
            Action::IncrementMasterRatio(amount) => {
                let new_value = self.config.tiling_config.master_ratio + amount;
                if new_value < 0.95 {
                    self.config.tiling_config.master_ratio = new_value;
                }
                self.relayout(None);
            }
            Action::DecrementMasterRatio(amount) => {
                let new_value = self.config.tiling_config.master_ratio - amount;
                if new_value > 0.05 {
                    self.config.tiling_config.master_ratio = new_value;
                }
                self.relayout(None);
            }
            Action::HideTabletWindows => {
                self.config.tiling_config.tablet_hide_minimized = true;
                self.relayout(None);
            }
            Action::ShowTabletWindows => {
                self.config.tiling_config.tablet_hide_minimized = false;
                self.relayout(None);
            }
            Action::ToggleTabletWindows => {
                self.config.tiling_config.tablet_hide_minimized = !self.config.tiling_config.tablet_hide_minimized;
                self.relayout(None);
            }
            Action::MakeRightHanded => {
                self.config.tiling_config.right_handed = true;
                self.relayout(None);
            }
            Action::MakeLeftHanded => {
                self.config.tiling_config.right_handed = false;
                self.relayout(None);
            }
            Action::FlipHandedness => {
                self.config.tiling_config.right_handed = !self.config.tiling_config.right_handed;
                self.relayout(None);
            }
        }
    }

    // -------------------------------------------------------------
    // Touchpad gestures — see `gesture.rs` for the data types these
    // operate on, and its module doc for the overall design.
    // -------------------------------------------------------------

    /// Starts tracking a new swipe — called from `GestureSwipeBegin`.
    /// Nothing actually happens until it clears the dead zone and
    /// resolves a direction (see `gesture_update`); a swipe that never
    /// does is just dropped, harmlessly, by `gesture_end`.
    pub fn gesture_begin(&mut self, fingers: u32) {
        self.gesture = Some(ActiveGesture::new(fingers));
    }

    /// Feeds one `GestureSwipeUpdate`'s delta into whatever gesture is
    /// currently in progress. Once accumulated movement clears
    /// `DEAD_ZONE`, resolves a direction, looks it up in the config, and
    /// fires the bound action immediately — for the animatable ones,
    /// hijacking the animation it started into `Manual` mode (see
    /// `start_resolved_gesture`). Every update after that just updates
    /// that animation's progress to match total distance travelled along
    /// the resolved axis.
    pub fn gesture_update(&mut self, dx: f64, dy: f64) {
        // Taken out of `self` rather than borrowed in place: resolving a
        // direction needs to fire an action, which needs `&mut self` —
        // impossible while still holding a live borrow of
        // `self.gesture`. Working on an owned local and putting it back
        // at the end sidesteps that entirely.
        let Some(mut gesture) = self.gesture.take() else { return };
        gesture.total.0 += dx;
        gesture.total.1 += dy;

        if gesture.resolved.is_none() {
            if gesture.total.0.abs() < DEAD_ZONE && gesture.total.1.abs() < DEAD_ZONE {
                self.gesture = Some(gesture);
                return;
            }

            let direction = if gesture.total.0.abs() > gesture.total.1.abs() {
                if gesture.total.0 < 0.0 { GestureDirection::Left } else { GestureDirection::Right }
            } else if gesture.total.1 < 0.0 {
                GestureDirection::Up
            } else {
                GestureDirection::Down
            };

            let action = self.config.get_gesture(gesture.fingers, direction).cloned();
            let kind = match action {
                Some(action) => self.start_resolved_gesture(action),
                None => ResolvedKind::Discrete { action: None },
            };
            gesture.resolved = Some(Resolved { direction, kind, progress: 0.0 });
        }

        if let Some(resolved) = gesture.resolved.as_mut() {
            let magnitude = match resolved.direction {
                GestureDirection::Up | GestureDirection::Down => gesture.total.1.abs(),
                GestureDirection::Left | GestureDirection::Right => gesture.total.0.abs(),
            };
            let progress = (magnitude / GESTURE_DISTANCE).clamp(0.0, 1.0);
            resolved.progress = progress;

            match &resolved.kind {
                ResolvedKind::Tag { output } => {
                    if let Some(anim) = self.tag_animations.get_mut(output) {
                        // Reassigned outright rather than mutated
                        // in-place: if some unrelated relayout raced in
                        // and replaced this with a fresh `Timed`
                        // animation since our last update, mutating the
                        // old `Animation` in place wouldn't touch the
                        // new one at all, and this gesture would quietly
                        // stop tracking the finger. An outright
                        // reassignment can't have that problem.
                        anim.animation = Animation::manual(progress);
                    }
                }
                ResolvedKind::Stack { keys, .. } | ResolvedKind::Output { keys, .. } => {
                    for key in keys {
                        if let Some(morph) = self.window_morphs.get_mut(key) {
                            morph.animation = Animation::manual(progress);
                        }
                    }
                }
                ResolvedKind::Discrete { .. } => {}
            }
        }

        self.gesture = Some(gesture);
        Backend::schedule_render(self);
    }

    /// Ends whatever gesture is in progress — called from
    /// `GestureSwipeEnd`. A swipe that never resolved a direction is
    /// just dropped. One that did either commits (if not `cancelled` and
    /// it travelled far enough — see `COMMIT_THRESHOLD`) or cancels; in
    /// both cases handing off to a short eased settle rather than
    /// snapping straight to the result.
    pub fn gesture_end(&mut self, cancelled: bool) {
        let Some(gesture) = self.gesture.take() else { return };
        let Some(resolved) = gesture.resolved else { return };
        let commit = !cancelled && resolved.progress >= COMMIT_THRESHOLD;
        let now = Instant::now();
        let release_duration = Duration::from_millis(RELEASE_DURATION_MS);

        match resolved.kind {
            ResolvedKind::Tag { output } => {
                if let Some(anim) = self.tag_animations.get_mut(&output) {
                    let target = if commit { 1.0 } else { 0.0 };
                    anim.animation = anim.animation.release_to(now, target, release_duration);
                }
            }
            ResolvedKind::Stack { keys, undo } | ResolvedKind::Output { keys, undo } => {
                if commit {
                    for key in &keys {
                        if let Some(morph) = self.window_morphs.get_mut(key) {
                            morph.animation = morph.animation.release_to(now, 1.0, release_duration);
                        }
                    }
                } else {
                    // Re-fire the opposite action: this both reverts the
                    // stack reorder (or, for `Output`, the output move)
                    // itself and — since `start_window_morph_impl`
                    // continues smoothly from a morph's *current* rect
                    // when one's already mid-animation for that window —
                    // eases back from wherever the drag currently sits
                    // rather than snapping to the start first.
                    self.handle_action(undo);
                }
            }
            ResolvedKind::Discrete { action } => {
                if commit && let Some(action) = action {
                    self.handle_action(action);
                }
            }
        }

        Backend::schedule_render(self);
    }

    /// Shared by every gesture whose bound action reorders/relocates
    /// windows and lets `relayout` create the animation via the ordinary
    /// `WindowMorph` reflow path (`MoveUpStack`/`MoveDownStack`'s stack
    /// reorder, and `MoveOutputLeft`/`MoveOutputRight`/`MoveOutputUp`/
    /// `MoveOutputDown`'s cross-output move): fires `action`, diffs
    /// `window_morphs` before/after to find every morph it just created
    /// or redirected, snaps each straight back to progress 0 (the finger
    /// hasn't moved past the dead zone yet — `gesture_update` drives it
    /// forward from here), and hands back the keys for the caller to
    /// wrap in whichever `ResolvedKind` fits.
    ///
    /// Snapshotting each entry's `to` rect before, rather than just which
    /// keys exist, is what catches a *replaced* morph too — a key that
    /// already existed but now targets a different rect got a brand new
    /// `Timed` animation from `start_window_morph_impl`, and needs
    /// hijacking into `Manual` exactly the same as one that didn't exist
    /// at all before. Missing that was leaving some windows animating on
    /// their own 180ms clock instead of tracking the new gesture, which
    /// is what made rapid repeated swipes look glitchy.
    fn start_diffing_gesture(&mut self, action: Action) -> Vec<ObjectId> {
        let mut before: std::collections::HashMap<ObjectId, Rect> = std::collections::HashMap::new();
        for (key, morph) in self.window_morphs.iter() {
            before.insert(key.clone(), morph.to);
        }
        self.handle_action(action);
        let mut keys: Vec<ObjectId> = Vec::new();
        for (key, morph) in self.window_morphs.iter() {
            if before.get(key) != Some(&morph.to) {
                keys.push(key.clone());
            }
        }

        for key in &keys {
            if let Some(morph) = self.window_morphs.get_mut(key) {
                morph.animation = Animation::manual(0.0);
            }
        }

        keys
    }

    /// Fires `action` immediately — exactly as if it were bound to a key
    /// — and, if it started an animatable transition, hijacks that
    /// animation into `Manual` mode so subsequent `gesture_update` calls
    /// can drive it directly instead of it running out on its own timer.
    /// Called once, the instant a swipe resolves a direction.
    fn start_resolved_gesture(&mut self, action: Action) -> ResolvedKind {
        match action {
            Action::MoveUpStack | Action::MoveDownStack => {
                let keys = self.start_diffing_gesture(action.clone());

                if keys.is_empty() {
                    // Nothing actually moved (e.g. only one window on
                    // this tag) — nothing to drive.
                    return ResolvedKind::Discrete { action: None };
                }

                let undo = match action {
                    Action::MoveUpStack => Action::MoveDownStack,
                    Action::MoveDownStack => Action::MoveUpStack,
                    _ => unreachable!(),
                };
                ResolvedKind::Stack { keys, undo }
            }
            Action::MoveOutputLeft | Action::MoveOutputRight | Action::MoveOutputUp | Action::MoveOutputDown => {
                let keys = self.start_diffing_gesture(action.clone());

                if keys.is_empty() {
                    // No output in that direction to send the window to
                    // (or nothing focused to move) — nothing to drive.
                    return ResolvedKind::Discrete { action: None };
                }

                let undo = match action {
                    Action::MoveOutputLeft => Action::MoveOutputRight,
                    Action::MoveOutputRight => Action::MoveOutputLeft,
                    Action::MoveOutputUp => Action::MoveOutputDown,
                    Action::MoveOutputDown => Action::MoveOutputUp,
                    _ => unreachable!(),
                };
                ResolvedKind::Output { keys, undo }
            }
            Action::FocusNextTag | Action::FocusPreviousTag => {
                let output = self.outputs.get_focused().id;
                let tag_before = self.outputs.get_focused_tag(output);
                self.handle_action(action.clone());
                let tag_after = self.outputs.get_focused_tag(output);

                if tag_before == tag_after {
                    // Already at the first/last tag — `focus_next_tag`/
                    // `focus_prevous_tag` no-op there, so there's no
                    // animation to have hijacked.
                    return ResolvedKind::Discrete { action: None };
                }

                if let Some(anim) = self.tag_animations.get_mut(&output) {
                    // Same idea as the stack case above: back to
                    // progress 0 (fully off-screen incoming tag, fully
                    // in-place outgoing tag) since the finger hasn't
                    // moved past the dead zone yet.
                    anim.animation = Animation::manual(0.0);
                }
                ResolvedKind::Tag { output }
            }
            other => ResolvedKind::Discrete { action: Some(other) },
        }
    }

    fn execute_commands(&mut self) {
        let commands = self.config.execute_actions();
        for command in commands {
            self.handle_action(command);
        }
    }

    pub fn do_autostart_if_needed(&mut self) {
        if !self.done_autostart {
            for command in self.config.autostarts() {
                self.spawn(command);
            }
            self.done_autostart = true;
            self.execute_commands();
        }
    }

    pub fn try_lock(&mut self) {
        if !self.locked {
            return;
        }

        let all_blanked = self.space.outputs()
            .all(|o| self.blanked_outputs.contains(o));

        if all_blanked {
            eprintln!("try_lock: confirming");
            let Some(confirmation) = self.pending_locker.take() else {
                return;
            };
            confirmation.lock();
        }
    }

    pub fn try_unlock(&mut self) {
        if !self.locked {
            return;
        }

        self.locked = false;
        self.blanked_outputs.clear();
        self.lock_surfaces.clear();;
        self.lock_focus_output = None;

        let keyboard = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();

        let focus_target = self
            .window_registry
            .focused_window()
            .and_then(|id| self.window_registry.get(&id))
            .and_then(|info| info.window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());

        keyboard.set_focus(self, focus_target, serial);
        BackendData::schedule_render(self);
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

fn apply_morph_finish(space: &mut Space<Window>, window: &Window, rect: Rect, on_finish: MorphFinish) {
    match on_finish {
        MorphFinish::Remap => space.map_element(window.clone(), (rect.x, rect.y), false),
        MorphFinish::Unmap => space.unmap_elem(window),
    }
}

/// Builds this frame's render elements for every in-flight `WindowMorph`
/// that belongs to `output_id`, advancing (and, once finished,
/// finalizing) each one along the way. Called once per rendered frame per
/// output from each backend's render path, right alongside
/// `Alice::advance_tag_animations`.
///
/// A free function taking `space`/`window_morphs` directly, rather than an
/// `Alice` method, very deliberately — both backends call this with a
/// `renderer` already borrowed *from* `Alice` (its GPU/backend state), so
/// a method needing all of `&mut Alice` here as well would conflict with
/// that live borrow the same way the plain `&mut alice` call in
/// `advance_tag_animations` briefly did before it was moved earlier in
/// `render_surface`. Borrowing just these two fields, spelled out
/// explicitly at each call site (`&mut alice.window_morphs, &mut
/// alice.space`), is what lets this coexist with a `renderer` still
/// borrowed from a different field of the same `Alice`.
///
/// `output_origin` is that output's own position in `Space` — needed to
/// convert each morph's rect (in `Space`-global logical coordinates) down
/// to that output's own physical pixels; see `morph_render_elements`.
pub fn morph_elements_for_output<R>(
    renderer: &mut R,
    window_morphs: &mut HashMap<ObjectId, WindowMorph>,
    space: &mut Space<Window>,
    output_id: OutputId,
    output_origin: Point<i32, Logical>,
    scale: f64,
) -> Vec<ScaledElement<WaylandSurfaceRenderElement<R>>>
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    if window_morphs.is_empty() {
        return Vec::new();
    }

    let now = Instant::now();
    // Taken out and reinserted rather than iterated in place, same reason
    // as `Alice::advance_tag_animations`: finalizing needs `&mut space` at
    // the same time as read access to the morph being finalized.
    let morphs = std::mem::take(window_morphs);
    let mut elements = Vec::new();

    for (key, morph) in morphs {
        if morph.output != output_id {
            // Not this output's frame to advance — leave it untouched and
            // let that output's own render call handle it.
            window_morphs.insert(key, morph);
            continue;
        }

        // A window whose underlying surface died mid-animation (the
        // client crashed, or otherwise tore things down faster than a
        // ~180ms close animation) has nothing left to safely render —
        // finish immediately rather than risk drawing a dead surface.
        if !morph.window.alive() || morph.is_finished(now) {
            apply_morph_finish(space, &morph.window, morph.to, morph.on_finish);
            continue;
        }

        let rect = morph.current_rect(now);
        let produced = morph_render_elements(renderer, &morph.window, rect, morph.base, output_origin, scale, 1.0);
        elements.extend(produced);
        window_morphs.insert(key, morph);
    }

    elements
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
