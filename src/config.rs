use std::collections::HashMap;

use mlua::{FromLua, FromLuaMulti, Lua, MetaMethod, Table, UserData};
use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::output::TagId;



#[derive(Clone)]
pub enum Action {
    Quit,
    ReloadConfig,
    Close,
    FullScreen,
    Spawn(String),
    FocusTag(TagId),
    MoveToTag(TagId),
    FocusNextTag,
    FocusPreviousTag,
    MoveNextTag,
    MovePreviousTag,
    SetLayout(String),
    FocusDownStack,
    FocusUpStack,
    MoveDownStack,
    MoveUpStack,
}

impl UserData for Action {
}

impl FromLua for Action {
    fn from_lua(value: mlua::prelude::LuaValue, lua: &Lua) -> mlua::prelude::LuaResult<Self> {
        match value {
            mlua::Value::UserData(data) => {
                data.user_value::<Self>()
            }
            _ => Err(mlua::Error::runtime("Not an Action")),
        }
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Hash, PartialEq, Eq)]
    pub struct ModMask: u8 {
        const Super = 0b1;
        const Ctrl = 0b10;
        const Alt = 0b100;
        const Shift = 0b1000;
    }
}

impl UserData for ModMask {
    fn add_methods<M: mlua::prelude::LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::BOr, |_, this, other: ModMask| {
            Ok(*this | other)
        });
    }
}

impl FromLua for ModMask {
    fn from_lua(value: mlua::prelude::LuaValue, lua: &Lua) -> mlua::prelude::LuaResult<Self> {
        match value {
            mlua::Value::UserData(data) => {
                data.user_value::<Self>()
            }
            _ => Err(mlua::Error::runtime("Not a modifier")),
        }
    }
}


#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub struct KeyPress {
    pub modifiers: ModMask,
    pub keysym: Keysym,
}

impl From<(&ModifiersState, Keysym)> for KeyPress {
    fn from((mods, sym): (&ModifiersState, Keysym)) -> Self {
        let mut modifiers = ModMask::empty();
        if mods.logo {
            modifiers |= ModMask::Super;
        }
        if mods.ctrl {
            modifiers |= ModMask::Ctrl;
        }
        if mods.alt {
            modifiers |= ModMask::Alt;
        }
        if mods.shift {
            modifiers |= ModMask::Shift;
        }

        KeyPress {
            modifiers,
            keysym: sym,
        }
    }
}

impl UserData for KeyPress {

}

pub struct Config {
    map: HashMap<KeyPress, Action>,
}

impl Config {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn get_keypress(&self, press: &KeyPress) -> Option<&Action> {
        self.map.get(press)
    }

    pub fn insert_keypress(&mut self, press: KeyPress, action: Action) {
        self.map.insert(press, action);
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut map = HashMap::new();
        let main_mod = ModMask::Alt;

        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('j'),
        }, Action::FocusDownStack);
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('k'),
        }, Action::FocusUpStack);
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('u'),
        }, Action::FocusPreviousTag);
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('i'),
        }, Action::FocusNextTag);
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('q'),
        }, Action::Close);
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('r'),
        }, Action::ReloadConfig);

        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('j'),
        }, Action::MoveDownStack);
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('k'),
        }, Action::MoveUpStack);
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('u'),
        }, Action::MovePreviousTag);
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('i'),
        }, Action::MoveNextTag);
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('q'),
        }, Action::Quit);

        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('1'),
        }, Action::FocusTag(TagId(0)));
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('2'),
        }, Action::FocusTag(TagId(1)));
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('3'),
        }, Action::FocusTag(TagId(2)));
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('4'),
        }, Action::FocusTag(TagId(3)));
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('5'),
        }, Action::FocusTag(TagId(4)));
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('6'),
        }, Action::FocusTag(TagId(5)));
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('7'),
        }, Action::FocusTag(TagId(6)));
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('8'),
        }, Action::FocusTag(TagId(7)));
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('9'),
        }, Action::FocusTag(TagId(8)));


        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('1'),
        }, Action::MoveToTag(TagId(0)));
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('2'),
        }, Action::MoveToTag(TagId(1)));
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('3'),
        }, Action::MoveToTag(TagId(2)));
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('4'),
        }, Action::MoveToTag(TagId(3)));
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('5'),
        }, Action::MoveToTag(TagId(4)));
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('6'),
        }, Action::MoveToTag(TagId(5)));
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('7'),
        }, Action::MoveToTag(TagId(6)));
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('8'),
        }, Action::MoveToTag(TagId(7)));
        map.insert(KeyPress {
            modifiers: main_mod | ModMask::Shift,
            keysym: Keysym::from_char('9'),
        }, Action::MoveToTag(TagId(8)));

        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('t'),
        }, Action::SetLayout(String::from("MasterStack")));

        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::from_char('f'),
        }, Action::FullScreen);

        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::Return,
        }, Action::Spawn(String::from("alacritty")));


        Self {
            map
        }
    }
}

/// Creates a lua interpreter with all need modules
fn create_lua() -> mlua::Result<Lua> {
    let lua = Lua::new();

    let modifier_table: Table = lua.create_table()?;

    modifier_table.set("super", lua.create_function(|_, _: ()| {
        Ok(ModMask::Super)
    })?)?;
    modifier_table.set("ctrl", lua.create_function(|_, _: ()| {
        Ok(ModMask::Ctrl)
    })?)?;
    modifier_table.set("alt", lua.create_function(|_, _: ()| {
        Ok(ModMask::Alt)
    })?)?;
    modifier_table.set("shift", lua.create_function(|_, _: ()| {
        Ok(ModMask::Shift)
    })?)?;
    modifier_table.set("none", lua.create_function(|_, _: ()| {
        Ok(ModMask::empty())
    })?)?;

    lua.globals().set("Modifiers", modifier_table)?;

    let action_table: Table = lua.create_table()?;

    action_table.set("quit", lua.create_function(|_, _: ()| {
        Ok(Action::Quit)
    })?)?;
    action_table.set("reload_config", lua.create_function(|_, _: ()| {
        Ok(Action::ReloadConfig)
    })?)?;
    action_table.set("full_screen", lua.create_function(|_, _: ()| {
        Ok(Action::FullScreen)
    })?)?;
    action_table.set("spawn", lua.create_function(|_, cmd: String| {
        Ok(Action::Spawn(cmd))
    })?)?;
    action_table.set("focus_tag", lua.create_function(|_, tag_id: i64| {
        if tag_id <= 0 {
            return Err(mlua::Error::runtime("Tags cannot be 0 or negative"));
        }
        let tag = (tag_id - 1) as u32;
        Ok(Action::FocusTag(TagId(tag)))
    })?)?;
    action_table.set("move_to_tag", lua.create_function(|_, tag_id: i64| {
        if tag_id <= 0 {
            return Err(mlua::Error::runtime("Tags cannot be 0 or negative"));
        }
        let tag = (tag_id - 1) as u32;
        Ok(Action::MoveToTag(TagId(tag)))
    })?)?;
    action_table.set("focus_next_tag", lua.create_function(|_, _: ()| {
        Ok(Action::FocusNextTag)
    })?)?;
    action_table.set("focus_previous_tag", lua.create_function(|_, _: ()| {
        Ok(Action::FocusPreviousTag)
    })?)?;
    action_table.set("move_next_tag", lua.create_function(|_, _: ()| {
        Ok(Action::MoveNextTag)
    })?)?;
    action_table.set("move_previous_tag", lua.create_function(|_, _: ()| {
        Ok(Action::MovePreviousTag)
    })?)?;
    action_table.set("set_layout", lua.create_function(|_, layout: String| {
        Ok(Action::SetLayout(layout))
    })?)?;
    action_table.set("focus_down_stack", lua.create_function(|_, _: ()| {
        Ok(Action::FocusDownStack)
    })?)?;
    action_table.set("focus_up_stack", lua.create_function(|_, _: ()| {
        Ok(Action::FocusUpStack)
    })?)?;
    action_table.set("move_down_stack", lua.create_function(|_, _: ()| {
        Ok(Action::MoveDownStack)
    })?)?;
    action_table.set("move_up_stack", lua.create_function(|_, _: ()| {
        Ok(Action::MoveUpStack)
    })?)?;

    lua.globals().set("Action", action_table)?;

    let key_press_table: Table = lua.create_table()?;

    key_press_table.set("press", lua.create_function(|_, (mask, key): (ModMask, String)| {
        let keysym = parse_keystring(&key)?;

        Ok(KeyPress {
            modifiers: mask,
            keysym,
        })
    })?)?;

    lua.globals().set("Key", key_press_table)?;


    Ok(lua)
}


fn parse_keystring(key: &str) -> mlua::Result<Keysym> {
    if key.chars().count() == 1 {
        return match key.chars().next() {
            Some(c) => Ok(Keysym::from_char(c)),
            None => unreachable!("we already checked for at least one character"),
        };
    }
    todo!("handle additional keysyms")
}
