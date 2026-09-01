use crate::window::WindowId;



#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}


pub trait Layout {
    fn name(&self) -> &'static str;
    fn arrange(&self, area: Rect, windows: &[WindowId], gap_size: i32) -> Vec<Rect>;
}


pub struct MasterStack;

impl Layout for MasterStack {
    fn name(&self) -> &'static str {
        "MasterStack"
    }

    fn arrange(&self, area: Rect, windows: &[WindowId], gap_size: i32) -> Vec<Rect> {
        if windows.is_empty() {
            return Vec::new();
        }
        if windows.len() == 1 {
            return vec![area];
        }
        let main_rect = Rect {
            x: area.x,
            y: area.y,
            width: (area.width / 2).saturating_sub(gap_size),
            height: area.height
        };
        let remaining_rect = Rect {
            x: main_rect.width + main_rect.x + gap_size,
            y: area.y,
            width: area.width.saturating_sub(main_rect.width + gap_size),
            height: area.height,
        };
        let mut out = Vec::with_capacity(windows.len());
        out.push(main_rect);

        let part_size = area.height / (windows.len() - 1) as i32; // Skipping first window
        let stack_rect = Rect {
            x: remaining_rect.x,
            y: remaining_rect.y,
            width: remaining_rect.width,
            height: part_size,
        };
        let mut stack = vec![stack_rect; windows.len() - 1];

        for i in 1..stack.len() {
            stack[i].y = stack[i - 1].y + stack[i - 1].height;
            if stack.len() > 1 {
                stack[i].height = stack[i].height.saturating_sub(gap_size);
            }
            if i != 1 {
                stack[i].y += gap_size;
            }
        }
        out.extend(stack);

        out
    }
}


