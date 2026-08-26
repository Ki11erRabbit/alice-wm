use std::collections::HashMap;

use smithay::output::Output;

use crate::layout::{Layout, MasterStack};



pub struct Outputs {
    outputs: Vec<OutputInfo>,
    map: HashMap<String, OutputId>,
    focused_tag: HashMap<OutputId, TagId>,
    focused_output: OutputId,
}

impl Outputs {
    pub fn new() -> Self {
        Self {
            outputs: Vec::with_capacity(3),
            map: HashMap::with_capacity(3),
            focused_tag: HashMap::with_capacity(3),
            focused_output: OutputId(0),
        }
    }

    pub fn insert(&mut self, output: Output) -> OutputId {
        if let Some(id) = self.map.get(&output.name()) {
            self.outputs[id.0 as usize] = OutputInfo::new(output, *id);
            return *id
        }
        let index = self.outputs.len() as u32;
        if self.map.is_empty() {
            self.focused_output = OutputId(index);
        }
        self.map.insert(output.name(), OutputId(index));
        self.outputs.push(OutputInfo::new(output, OutputId(index)));
        self.focused_tag.insert(OutputId(index), TagId(0));
        OutputId(index)
    }

    pub fn get(&self, name: &str) -> Option<&OutputInfo> {
        self.map.get(name)
            .and_then(|id| self.outputs.get(id.0 as usize))
    }

    pub fn get_id(&self, id: OutputId) -> &OutputInfo {
        &self.outputs[id.0 as usize]
    }

    pub fn get_focused(&self) -> &OutputInfo {
        &self.outputs[self.focused_output.0 as usize]
    }

    pub fn change_focus(&mut self, id: OutputId) {
        self.focused_output = id;
    }

    pub fn change_tag(&mut self, tag: TagId) {
        self.focused_tag.insert(self.focused_output, tag);
    }

    pub fn current_focused_tag(&self) -> Option<TagId> {
        self.focused_tag.get(&self.focused_output).cloned()
    }

    pub fn get_focused_tag(&self, id: OutputId) -> Option<TagId> {
        self.focused_tag.get(&id).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = &OutputInfo> {
        self.outputs.iter()
            .filter_map(|item| {
                if item.active {
                    Some(item)
                } else {
                    None
                }
            })
    }

}

#[derive(Clone)]
pub struct OutputInfo {
    pub output: Output,
    pub id: OutputId,
    pub active: bool,
}

impl OutputInfo {
    pub fn new(output: Output, id: OutputId) -> Self {
        Self {
            output,
            id,
            active: true,
        }
    }
}

#[derive(Clone, Copy,PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TagId(pub u32);
#[derive(Clone, Copy,PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputId(pub u32);

impl OutputId {
    pub fn next(&mut self) -> Self {
        let val = *self;
        self.0 += 1;
        val
    }
}

pub struct LayoutRegistry {
    available: HashMap<&'static str, Box<dyn Layout>>,
    active: HashMap<LayoutScope, &'static str>,
    default_layout: String,
}

impl LayoutRegistry {
    pub fn new() -> Self {
        let mut available = HashMap::new();
        let master_stack = MasterStack;
        let default_layout = master_stack.name().to_string();
        available.insert(master_stack.name(), Box::new(master_stack) as Box<dyn Layout>);

        Self {
            available,
            active: HashMap::new(),
            default_layout,
        }
    }

    pub fn get_layout(&self, scope: &LayoutScope) -> &dyn Layout {
        let name = self.active.get(scope)
            .map(|string| *string)
            .unwrap_or(self.default_layout.as_str());
        match self.available.get(name) {
            Some(layout) => layout.as_ref(),
            None => unreachable!("the only layouts are ones that are known at compile time"),
        }
    }

    pub fn set_active(&mut self, scope: LayoutScope, name: &str) {
        match self.available.get(name) {
            Some(layout) => self.active.insert(scope, layout.name()),
            None => unreachable!("you provided a bad layout name at compile time"),
        };
    }
}

#[derive(Clone, Copy,PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayoutScope {
    pub output: OutputId,
    pub tag: TagId,
}
