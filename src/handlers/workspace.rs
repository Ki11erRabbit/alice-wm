//! `ext-workspace-v1` — exposes our tag system to workspace-switcher bars
//! (waybar's `ext/workspaces` module, noctalia, and anything else that
//! speaks this protocol).
//!
//! Mapping: one `ext_workspace_group_handle_v1` per [`Output`], containing
//! one `ext_workspace_handle_v1` per [`TagId`]. There are a fixed 9 tags
//! (see the `FocusTag`/`MoveToTag` bindings in `config.rs`), so unlike a
//! dynamic-workspace compositor we never create or destroy workspace
//! objects except when an output itself appears/disappears.
//!
//! Since each output here focuses exactly one tag at a time (see
//! `Outputs::focused_tag`), this is a much simpler mapping than a
//! multi-tag-view system like river or awesomewm: `active` just tracks
//! `Outputs::get_focused_tag`, one workspace at a time per group.
//! `hidden` doubles as an occupancy signal (see `workspace_state`) since
//! the protocol has no dedicated bit for that — set for any inactive,
//! window-less tag, so bars that key off it (waybar's `ignore-hidden`,
//! noctalia's `empty_color`) can tell empty tags apart from occupied
//! ones, while all 9 tags stay individually selectable regardless.
//! `urgent` isn't wired up yet since this compositor has no notion of
//! window urgency/attention requests at all; if that's added later, set
//! it here too.

use std::collections::HashMap;

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
            ext_workspace_handle_v1::{
                self, ExtWorkspaceHandleV1, State as WsState, WorkspaceCapabilities,
            },
            ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
        },
        wayland_server::{
            backend::{ClientId, GlobalId, ObjectId},
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
        },
    },
};

use crate::{
    output::{LayoutScope, OutputId, Outputs, TagId},
    state::backend::Backend,
    window::WindowRegistry,
    Alice,
};

const MANAGER_VERSION: u32 = 1;
/// Keep in sync with the `FocusTag`/`MoveToTag` bindings in `config.rs`.
const NUM_TAGS: u32 = 9;

#[derive(Default)]
pub struct ManagerGlobalData;
#[derive(Default)]
pub struct ManagerUserData;

pub struct GroupUserData {
    #[allow(dead_code)]
    output: OutputId,
}

pub struct WorkspaceUserData {
    output: OutputId,
    tag: TagId,
}

struct BoundInstance {
    manager: ExtWorkspaceManagerV1,
    groups: HashMap<OutputId, GroupEntry>,
    workspaces: HashMap<(OutputId, TagId), ExtWorkspaceHandleV1>,
}

struct GroupEntry {
    handle: ExtWorkspaceGroupHandleV1,
    /// Whether `output_enter` has actually been sent for this group yet.
    /// A client is free to bind `wl_output` for this output *after*
    /// binding `ext_workspace_manager_v1` (there's no ordering guarantee
    /// between two independent globals), so we can't assume
    /// `Output::client_outputs` will find anything at group-creation time
    /// — this tracks "still owed an output_enter" so we can retry later
    /// once the client catches up.
    entered_output: bool,
}

/// Retry `output_enter` for any group of this instance that hasn't gotten
/// one yet. Cheap no-op once every group has caught up. Call this
/// anywhere we're already about to talk to the client (broadcasts, the
/// manager's `commit` request), since we have no direct hook for "client
/// just bound a new wl_output" to react to immediately.
fn sync_group_outputs(outputs: &Outputs, instance: &mut BoundInstance) {
    let Some(client) = instance.manager.client() else { return };
    for (&output_id, group) in &mut instance.groups {
        if group.entered_output {
            continue;
        }
        let output = &outputs.get_id(output_id).output;
        if let Some(wl_output) = output.client_outputs(&client).next() {
            group.handle.output_enter(&wl_output);
            group.entered_output = true;
        }
    }
}

pub struct WorkspaceManagerState {
    #[allow(dead_code)]
    global: GlobalId,
    instances: Vec<BoundInstance>,
}

impl WorkspaceManagerState {
    /// Advertise the `ext_workspace_manager_v1` global. Call once at
    /// startup, same as the other `*State::new::<Alice<BackendData>>(&dh)`
    /// calls in `Alice::new`.
    pub fn new<BackendData: Backend + 'static>(dh: &DisplayHandle) -> Self {
        let global = dh.create_global::<Alice<BackendData>, ExtWorkspaceManagerV1, _>(
            MANAGER_VERSION,
            ManagerGlobalData,
        );
        Self { global, instances: Vec::new() }
    }
}

/// `active` mirrors `Outputs::get_focused_tag` — one workspace at a time
/// per group, since this compositor only ever views one tag per output.
/// `hidden` doubles as "occupied" here: the protocol has no dedicated
/// occupancy bit, but `hidden` is the conventional signal bars key off for
/// exactly this (e.g. waybar's `ignore-hidden`, noctalia's
/// `empty_color`/`occupied_color`) — so we set it for any tag that's both
/// not currently active *and* has no windows on this output. An active
/// tag is never marked hidden even if it happens to be empty, since it's
/// inherently in view regardless of occupancy.
fn workspace_state(outputs: &Outputs, window_registry: &WindowRegistry, output: OutputId, tag: TagId) -> WsState {
    let active = outputs.get_focused_tag(output) == Some(tag);
    let occupied = window_registry.filter(&LayoutScope { output, tag }).next().is_some();

    let mut state = WsState::empty();
    if active {
        state |= WsState::Active;
    }
    if !active && !occupied {
        state |= WsState::Hidden;
    }
    state
}

/// Push every group/workspace this `manager` instance should know about for
/// `output`, and register them in `instance`. Shared by the initial bind
/// (all known outputs) and by `Alice::workspace_output_added` (one new
/// output, for every already-bound client).
/// Generic over `BackendData` purely so `Client::create_resource::<I, U, D>`
/// below has a concrete `D = Alice<BackendData>` to infer — nothing here
/// otherwise touches backend-specific state.
fn announce_output<BackendData: Backend + 'static>(
    dh: &DisplayHandle,
    client: &Client,
    manager: &ExtWorkspaceManagerV1,
    instance: &mut BoundInstance,
    outputs: &Outputs,
    window_registry: &WindowRegistry,
    output_id: OutputId,
    output: &Output,
) {

    let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, Alice<BackendData>>(
        dh,
        manager.version(),
        GroupUserData { output: output_id },
    ) else {
        return;
    };
    manager.workspace_group(&group);
    group.capabilities(GroupCapabilities::empty());

    // Attach the group to a `wl_output` if this client already happens to
    // have one bound for it — but don't gate *creating the group and its
    // workspaces* on that. If it's not available yet, `sync_group_outputs`
    // will retry later; see `GroupEntry::entered_output`.
    let entered_output = if let Some(wl_output) = output.client_outputs(client).next() {
        group.output_enter(&wl_output);
        true
    } else {
        false
    };

    for i in 0..NUM_TAGS {
        let tag = TagId(i);
        let Ok(ws) = client.create_resource::<ExtWorkspaceHandleV1, _, Alice<BackendData>>(
            dh,
            manager.version(),
            WorkspaceUserData { output: output_id, tag },
        ) else {
            continue;
        };
        manager.workspace(&ws);
        ws.name((i + 1).to_string());
        ws.capabilities(WorkspaceCapabilities::Activate);
        ws.state(workspace_state(outputs, window_registry, output_id, tag));
        group.workspace_enter(&ws);
        instance.workspaces.insert((output_id, tag), ws);
    }

    instance.groups.insert(output_id, GroupEntry { handle: group, entered_output });
}

impl<BackendData: Backend + 'static> Alice<BackendData> {
    /// Re-sends `state()` for every workspace on every bound client, then
    /// `done()`s each manager instance. Call this after anything that
    /// changes which tag is focused on an output, or which tags have
    /// windows (once `urgent`/occupancy gets wired in).
    pub fn broadcast_workspace_state(&mut self) {
        for instance in &mut self.workspace_manager.instances {
            sync_group_outputs(&self.outputs, instance);
            for (&(output, tag), ws) in &instance.workspaces {
                ws.state(workspace_state(&self.outputs, &self.window_registry, output, tag));
            }
            instance.manager.done();
        }
    }

    /// Call right after `self.outputs.insert(output.clone())` in the
    /// winit/udev backends, so already-bound clients learn about the new
    /// output's workspace group without having to rebind the manager.
    pub fn workspace_output_added(&mut self, output_id: OutputId) {
        let dh = self.display_handle.clone();
        let output = self.outputs.get_id(output_id).output.clone();

        for i in 0..self.workspace_manager.instances.len() {
            let manager = self.workspace_manager.instances[i].manager.clone();
            let Some(client) = manager.client() else { continue };
            announce_output::<BackendData>(
                &dh,
                &client,
                &manager,
                &mut self.workspace_manager.instances[i],
                &self.outputs,
                &self.window_registry,
                output_id,
                &output,
            );
            manager.done();
        }
    }

    /// Call from `connector_disconnected`/`device_removed` (alongside
    /// `self.outputs.deactivate(...)`), so bars drop the group for an
    /// output that just went away instead of showing a dead entry.
    pub fn workspace_output_removed(&mut self, output_id: OutputId) {
        for instance in &mut self.workspace_manager.instances {
            let Some(group) = instance.groups.remove(&output_id) else { continue };

            let stale: Vec<_> = instance
                .workspaces
                .keys()
                .filter(|(o, _)| *o == output_id)
                .copied()
                .collect();
            for key in stale {
                if let Some(ws) = instance.workspaces.remove(&key) {
                    group.handle.workspace_leave(&ws);
                    ws.removed();
                }
            }
            group.handle.removed();
            instance.manager.done();
        }
    }
}

impl<BackendData: Backend + 'static>
    GlobalDispatch<ExtWorkspaceManagerV1, ManagerGlobalData, Alice<BackendData>> for Alice<BackendData>
{
    fn bind(
        state: &mut Alice<BackendData>,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &ManagerGlobalData,
        data_init: &mut DataInit<'_, Alice<BackendData>>,
    ) {
        let manager = data_init.init(resource, ManagerUserData);

        let mut instance = BoundInstance {
            manager: manager.clone(),
            groups: HashMap::new(),
            workspaces: HashMap::new(),
        };

        let outputs: Vec<(OutputId, Output)> = state
            .outputs
            .iter()
            .map(|info| (info.id, info.output.clone()))
            .collect();

        for (output_id, output) in &outputs {
            announce_output::<BackendData>(
                dh,
                client,
                &manager,
                &mut instance,
                &state.outputs,
                &state.window_registry,
                *output_id,
                output,
            );
        }

        manager.done();
        state.workspace_manager.instances.push(instance);
    }
}

impl<BackendData: Backend + 'static> Dispatch<ExtWorkspaceManagerV1, ManagerUserData, Alice<BackendData>>
    for Alice<BackendData>
{
    fn request(
        state: &mut Alice<BackendData>,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &ManagerUserData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Alice<BackendData>>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                // Some clients send `commit` once they've processed our
                // initial batch, which by then typically means they've
                // also finished binding whatever else they wanted
                // (including wl_output) — a convenient point to retry any
                // output_enter we couldn't send earlier. Harmless no-op if
                // there's nothing left to catch up on.
                if let Some(instance) = state
                    .workspace_manager
                    .instances
                    .iter_mut()
                    .find(|i| i.manager.id() == manager.id())
                {
                    sync_group_outputs(&state.outputs, instance);
                    instance.manager.done();
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
            }
            _ => {}
        }
    }

    fn destroyed(
            state: &mut Alice<BackendData>,
            _client: ClientId,
            resource: &ExtWorkspaceManagerV1,
            _data: &ManagerUserData,
        ) {
        state.workspace_manager.instances.retain(|i| i.manager.id() != resource.id());

    }

}

impl<BackendData: Backend + 'static>
    Dispatch<ExtWorkspaceGroupHandleV1, GroupUserData, Alice<BackendData>> for Alice<BackendData>
{
    fn request(
        _state: &mut Alice<BackendData>,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &GroupUserData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Alice<BackendData>>,
    ) {
        match request {
            // We never advertise the `create_workspace` group capability
            // (our tag set is fixed) — ignore it per-spec if it arrives anyway.
            ext_workspace_group_handle_v1::Request::CreateWorkspace { .. } => {}
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl<BackendData: Backend + 'static>
    Dispatch<ExtWorkspaceHandleV1, WorkspaceUserData, Alice<BackendData>> for Alice<BackendData>
{
    fn request(
        state: &mut Alice<BackendData>,
        _client: &Client,
        _ws: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &WorkspaceUserData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Alice<BackendData>>,
    ) {
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                // Clicking a tag button for an output other than the
                // currently-focused one should focus that output first —
                // `change_tag` below always applies to whichever output is
                // currently focused. `change_tag` itself calls
                // `broadcast_workspace_state` at the end, so every path
                // that switches tags (this one and the keybindings in
                // `config.rs`) stays in sync without duplicating that call.
                state.focus_output(data.output);
                state.change_tag(data.tag);
            }
            // We only advertise `activate` — `deactivate`/`remove`/`assign`
            // are ignored per-spec since we didn't advertise them.
            _ => {}
        }
    }
    fn destroyed(
        state: &mut Alice<BackendData>,
        _client: ClientId,
        resource: &ExtWorkspaceHandleV1,
        _data: &WorkspaceUserData,
    ) {
        for instance in &mut state.workspace_manager.instances {
            // Must fully-qualify: `ws.id()`/`resource.id()` would resolve to
            // the protocol's own `id(id: String)` event-sender method
            // (inherent methods shadow trait methods of the same name),
            // not `Resource::id() -> ObjectId`.
            instance.workspaces.retain(|_, ws| Resource::id(ws) != Resource::id(resource));
        }
    }

}

// No delegate_dispatch!/delegate_global_dispatch! needed here: those macros
// forward Alice's obligations to a *different* helper type (that's what
// delegate_seat!/delegate_output! do, forwarding to smithay's own
// SeatState/OutputManagerState). Since ext-workspace-v1 has no such
// smithay-provided helper, the four impls above are implemented directly
// on `Alice<BackendData>`, and that's already the complete story — adding
// a delegate_dispatch! on top would try to forward Alice to itself.
