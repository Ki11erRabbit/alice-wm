use smithay::{output::Output, reexports::{wayland_protocols::ext::workspace::v1::server::{ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1, ext_workspace_handle_v1::ExtWorkspaceHandleV1}, wayland_server::backend::GlobalId}};

use crate::output::TagId;



pub struct WorkspaceManagerState {
    pub manager: GlobalId,
    pub groups: Vec<WorkspaceGroup>,
}

pub struct WorkspaceGroup {
    pub handle: ExtWorkspaceGroupHandleV1,
    pub output: Output,
    pub workspace: Vec<WorkspaceEntry>,
}

pub struct WorkspaceEntry {
    handle: ExtWorkspaceHandleV1,
    tag_id: TagId,
    name: String,
}

#[derive(Default)]
pub struct ManagerGlobalData;
