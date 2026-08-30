use std::collections::HashMap;

use mlua::{FromLua, Lua, MetaMethod, Table, UserData};
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::utils::Transform;

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
    FocusOutputLeft,
    FocusOutputRight,
    FocusOutputUp,
    FocusOutputDown,
}

impl UserData for Action {
}

impl FromLua for Action {
    fn from_lua(value: mlua::prelude::LuaValue, _: &Lua) -> mlua::prelude::LuaResult<Self> {
        match value {
            mlua::Value::UserData(data) => {
                Ok(data.borrow::<Self>()?.clone())
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
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            let mut string = String::new();
            if this.contains(ModMask::Super) {
                string.push_str("S");
            }
            if this.contains(ModMask::Ctrl) {
                string.push_str("C");
            }
            if this.contains(ModMask::Alt) {
                string.push_str("A");
            }
            if this.contains(ModMask::Shift) {
                string.push_str("Sh");
            }

            Ok(string)
        });
    }
}

impl FromLua for ModMask {
    fn from_lua(value: mlua::prelude::LuaValue, _: &Lua) -> mlua::prelude::LuaResult<Self> {
        match value {
            mlua::Value::UserData(data) => {
                Ok(*data.borrow::<Self>()?)
            }
            x => Err(mlua::Error::runtime(format!("Not a modifier: `{}`", x.type_name()))),
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

impl FromLua for KeyPress {
    fn from_lua(value: mlua::prelude::LuaValue, _: &Lua) -> mlua::prelude::LuaResult<Self> {
        match value {
            mlua::Value::UserData(data) => {
                Ok(*data.borrow::<Self>()?)
            }
            _ => Err(mlua::Error::runtime("Not a keypress")),
        }

    }
}

/// A user-configured position for a named output (connector), e.g. `"DP-1"`
/// or `"HDMI-A-1"`, in the compositor's global (layout) coordinate space,
/// along with optional transform/scale/refresh preferences for that output.
#[derive(Clone, Copy, Debug)]
pub struct OutputPosition {
    pub x: i32,
    pub y: i32,
    /// Rotation and/or flip to apply to the output. Defaults to
    /// `Transform::Normal` (no rotation, not flipped).
    pub transform: Transform,
    /// Fractional scale factor (e.g. `1.5`, `2.0`). `None` leaves the
    /// backend's default scale (integer 1) in place.
    pub scale: Option<f64>,
    /// Desired refresh rate in Hz (e.g. `144.0`). `None` picks the
    /// connector's preferred mode as before; the udev backend otherwise
    /// picks whichever available mode at the target resolution has the
    /// closest refresh rate.
    pub refresh: Option<f64>,
}

impl OutputPosition {
    fn new(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            transform: Transform::Normal,
            scale: None,
            refresh: None,
        }
    }
}

pub struct Config {
    map: HashMap<KeyPress, Action>,
    output_positions: HashMap<String, OutputPosition>,
}

impl Config {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            output_positions: HashMap::new(),
        }
    }

    pub fn get_keypress(&self, press: &KeyPress) -> Option<&Action> {
        self.map.get(press)
    }

    pub fn insert_keypress(&mut self, press: KeyPress, action: Action) {
        self.map.insert(press, action);
    }

    /// Look up the configured position (and transform/scale/refresh) for an
    /// output by its connector name (e.g. `"DP-1"`). Returns `None` if the
    /// user hasn't configured that output, in which case the backend should
    /// fall back to automatic placement and defaults.
    pub fn get_output_position(&self, name: &str) -> Option<OutputPosition> {
        self.output_positions.get(name).copied()
    }

    pub fn set_output_position(&mut self, name: String, position: OutputPosition) {
        self.output_positions.insert(name, position);
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

        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::Left,
        }, Action::FocusOutputLeft);
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::Right,
        }, Action::FocusOutputRight);
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::Up,
        }, Action::FocusOutputUp);
        map.insert(KeyPress {
            modifiers: main_mod,
            keysym: Keysym::Down,
        }, Action::FocusOutputDown);


        Self {
            map,
            output_positions: HashMap::new(),
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
    action_table.set("close", lua.create_function(|_, _: ()| {
        Ok(Action::Close)
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
    action_table.set("focus_output_left", lua.create_function(|_, _: ()| {
        Ok(Action::FocusOutputLeft)
    })?)?;
    action_table.set("focus_output_right", lua.create_function(|_, _: ()| {
        Ok(Action::FocusOutputRight)
    })?)?;
    action_table.set("focus_output_up", lua.create_function(|_, _: ()| {
        Ok(Action::FocusOutputUp)
    })?)?;
    action_table.set("focus_output_down", lua.create_function(|_, _: ()| {
        Ok(Action::FocusOutputDown)
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
    match key {
        "Return" => Ok(Keysym::Return),
        "Left" => Ok(Keysym::Left),
        "Right" => Ok(Keysym::Right),
        "Up" => Ok(Keysym::Up),
        "Down" => Ok(Keysym::Down),
        x => todo!("handle additional keysyms: {x}"),
    }
}


fn load_config(file_text: &str) -> mlua::Result<Config> {
    use std::rc::Rc;
    use std::cell::RefCell;
    let lua = create_lua()?;
    let config = Rc::new(RefCell::new(Config::new()));

    let config_clone = config.clone();

    lua.globals().set("bind", lua.create_function_mut(move |_, (keypress, action): (KeyPress, Action)| {
        let config = config_clone.clone();
        let mut guard = config.borrow_mut();
        guard.insert_keypress(keypress, action);
        Ok(())
    })?)?;

    let config_clone = config.clone();

    // output_position(name, x, y, opts?) — pins an output (by connector name,
    // e.g. "DP-1" or "HDMI-A-1") to an explicit spot in the global layout
    // space. Outputs without a configured position fall back to automatic
    // left-to-right placement. `opts` is an optional table:
    //   output_position("HDMI-A-1", 1920, 0, {
    //       rotate = 90,      -- 0, 90, 180, or 270 (degrees, clockwise)
    //       flipped = true,   -- mirror the output vertically
    //       scale = 1.5,      -- fractional scale factor
    //       refresh = 144,    -- desired refresh rate in Hz
    //   })
    lua.globals().set("output_position", lua.create_function_mut(move |_, (name, x, y, opts): (String, i32, i32, Option<Table>)| {
        let mut position = OutputPosition::new(x, y);

        if let Some(opts) = opts {
            let rotate: Option<i32> = opts.get("rotate")?;
            let flipped: Option<bool> = opts.get("flipped")?;
            position.transform = match (rotate.unwrap_or(0), flipped.unwrap_or(false)) {
                (0, false) => Transform::Normal,
                (90, false) => Transform::_90,
                (180, false) => Transform::_180,
                (270, false) => Transform::_270,
                (0, true) => Transform::Flipped,
                (90, true) => Transform::Flipped90,
                (180, true) => Transform::Flipped180,
                (270, true) => Transform::Flipped270,
                (other, _) => return Err(mlua::Error::runtime(
                    format!("output_position: rotate must be 0, 90, 180, or 270 (got {other})")
                )),
            };
            position.scale = opts.get("scale")?;
            position.refresh = opts.get("refresh")?;
        }

        let config = config_clone.clone();
        let mut guard = config.borrow_mut();
        guard.set_output_position(name, position);
        Ok(())
    })?)?;

    lua.load(file_text).exec()?;


    Ok(config.take())
}


pub fn execute_lua_config() -> anyhow::Result<Config> {
    let xdg_dirs = xdg::BaseDirectories::with_prefix("alice-wm");
    xdg_dirs.create_config_directory("")?;
    let config_path = match xdg_dirs.find_config_file("config.lua") {
        Some(path) => path,
        None => return Ok(Config::default()),
    };
    let string  = std::fs::read_to_string(config_path)?;
    let config = load_config(&string)?;

    Ok(config)
}
