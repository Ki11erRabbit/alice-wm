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

use crate::{CalloopData, config::{Action, Config, KeyPress, execute_lua_config}, layer::LayerRegistry, layout::Rect, output::{LayoutRegistry, LayoutScope, OutputInfo, Outputs, TagId}, state::backend::Backend, window::{LayoutInfo, WindowId, WindowRegistry}};

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

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Alice<BackendData>>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,

    pub seat: Seat<Self>,
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

        // Get the loop signal, used to stop the event loop
        let loop_signal = event_loop.get_signal();

        let config = match execute_lua_config() {
            Ok(config) => config,
            Err(err) => {
                eprintln!("Error while loading config: {err}");
                Config::default()
            }
        };

        let layer_shell_state = WlrLayerShellState::new::<Alice<BackendData>>(&dh);

        Self {
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

            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            popups,
            seat,
        }
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
                eprintln!("accepting new client connection");
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
                    eprintln!("dispatch_clients firing");
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

    pub fn layer_under(&self, pos: Point<f64, Logical>) -> Option<smithay::desktop::LayerSurface> {
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
        let point = pos - output_loc;

        layers
            .layer_under(wlr_layer::Layer::Overlay, point)
            .or_else(|| layers.layer_under(wlr_layer::Layer::Top, point))
            .or_else(|| layers.layer_under(wlr_layer::Layer::Bottom, point))
            .or_else(|| layers.layer_under(wlr_layer::Layer::Background, point))
            .cloned()
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
        let mut windows = self.window_registry.filter(&scope)
            .collect::<Vec<_>>();

        if self.try_full_screen(area, &windows) {
            BackendData::schedule_render(self);
            return;
        }

        let layout = self.layout_registry.get_layout(&scope);
        let rects = layout.arrange(area, &windows);
        eprintln!("relayout_single: area={:?} windows={} rects={:?}", area, windows.len(), rects);

        for (id, rect) in windows.iter().zip(rects) {
            self.apply_rects(*id, rect);
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
        eprintln!("apply_rects: window {:?} -> {:?}", id, rect);
    }


    fn usable_area(&mut self, output: &Output) -> Rect {
        let map = layer_map_for_output(output);
        let zone = map.non_exclusive_zone();

        Rect {
            x: zone.loc.x,
            y: zone.loc.y,
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

        self.window_registry.remove(id);

        self.space.unmap_elem(&window);
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

        for id in self.window_registry.filter(&LayoutScope {
            output: self.outputs.get_focused().id,
            tag: old_tag,
        }) {
            let window = self.window_registry.get(&id)?;
            self.space.unmap_elem(&window.window);
        }
        let mut no_windows = true;
        for id in self.window_registry.filter(&LayoutScope {
            output: self.outputs.get_focused().id,
            tag,
        }) {
            no_windows = false;
            let window = self.window_registry.get(&id)?;
            self.space.map_element(window.window.clone(), (0,0), false);
        }
        self.outputs.change_tag(tag);
        if no_windows == true {
            self.window_registry.change_focus(None);
        }
        self.relayout(Some(LayoutScope {
            output: self.outputs.get_focused().id,
            tag,
        }));
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

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::Quit => std::process::exit(0),
            Action::ReloadConfig => {
                match execute_lua_config() {
                    Ok(config) => self.config = config,
                    Err(err) => {
                        eprintln!("Error reloading config: {err}");
                    }
                }
            }
            Action::Close => {
                let Some(info) = self.window_registry.get_focused() else {
                    return;
                };
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
                _ = self.focus_down();
            }
            Action::MoveUpStack => {
                _ = self.focus_up();
            }


        }

    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        eprintln!("client {:?} initialized", client_id);
    }
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        eprintln!("client {:?} disconnected: {:?}", client_id, reason);
    }
}
