use crate::window::WindowId;



#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy)]
pub struct TilingConfig {
    pub master_ratio: f64,
    pub right_handed: bool,
    pub tablet_hide_minimized: bool,
}

impl TilingConfig {
    pub fn new() -> Self {
        Self {
            master_ratio: 0.5,
            right_handed: true,
            tablet_hide_minimized: false,
        }
    }
}

pub trait Layout {
    fn name(&self) -> &'static str;
    fn arrange_horizontal(&self, area: Rect, windows: &[WindowId], gap_size: i32, config: TilingConfig) -> Vec<Rect>;
    fn arrange_vertical(&self, area: Rect, windows: &[WindowId], gap_size: i32, config: TilingConfig) -> Vec<Rect>;
}


pub struct MasterStack;

impl Layout for MasterStack {
    fn name(&self) -> &'static str {
        "MasterStack"
    }

    fn arrange_horizontal(&self, area: Rect, windows: &[WindowId], gap_size: i32, config: TilingConfig) -> Vec<Rect> {
        if windows.is_empty() {
            return Vec::new();
        }
        if windows.len() == 1 {
            return vec![area];
        }

        let main_area = (area.width as f64 * config.master_ratio).round() as i32;
        let main_rect = Rect {
            x: area.x,
            y: area.y,
            width: main_area.saturating_sub(gap_size),
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
            stack[i].y = stack[i - 1].y + stack[i - 1].height + gap_size;
            stack[i].height = stack[i].height.saturating_sub(gap_size);
        }
        out.extend(stack);

        out
    }

    fn arrange_vertical(&self, area: Rect, windows: &[WindowId], gap_size: i32, config: TilingConfig) -> Vec<Rect> {
        if windows.is_empty() {
            return Vec::new();
        }
        if windows.len() == 1 {
            return vec![area];
        }
        let main_area = (area.height as f64 * config.master_ratio).round() as i32;
        let main_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: main_area.saturating_sub(gap_size),
        };
        let remaining_rect = Rect {
            x: area.x,
            y: main_rect.height + main_rect.y + gap_size,
            width: area.width,
            height: area.height.saturating_sub(main_rect.height + gap_size),
        };
        let mut out = Vec::with_capacity(windows.len());
        out.push(main_rect);

        let part_size = area.width / (windows.len() - 1) as i32; // Skipping first window
        let stack_rect = Rect {
            x: remaining_rect.x,
            y: remaining_rect.y,
            width: part_size,
            height: remaining_rect.height,
        };
        let mut stack = vec![stack_rect; windows.len() - 1];

        for i in 1..stack.len() {
            stack[i].x = stack[i - 1].x + stack[i - 1].width + gap_size;
            stack[i].width = stack[i].width.saturating_sub(gap_size);
        }
        out.extend(stack);

        out
    }
}

pub struct Fibonacci;

impl Layout for Fibonacci {
    fn name(&self) -> &'static str {
        "Fibonacci"
    }

    fn arrange_horizontal(&self, area: Rect, windows: &[WindowId], gap_size: i32, config: TilingConfig) -> Vec<Rect> {
        let mut out = Vec::with_capacity(windows.len());
        fib(Phase::Right, area, windows.len(), gap_size, &mut out);
        out
    }

    fn arrange_vertical(&self, area: Rect, windows: &[WindowId], gap_size: i32, config: TilingConfig) -> Vec<Rect> {
        let mut out = Vec::with_capacity(windows.len());
        fib(Phase::Down, area, windows.len(), gap_size, &mut out);
        out
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Up,
    Down,
    Left,
    Right,
}
impl Phase {
    fn next(self) -> Self {
        match self {
            Self::Right=> Self::Down,
            Self::Down => Self::Left,
            Self::Left => Self::Up,
            Self::Up => Self::Right,
        }
    }
}


fn fib(
    phase: Phase,
    area: Rect,
    n: usize,
    gap_size: i32,
    out: &mut Vec<Rect>,
) {
    if n == 0 {
        return;
    } else if n == 1 {
        out.push(area);
        return;
    }
    const RATIO: f64 = 0.618;

    let (placed, rest) = match phase {
        Phase::Right => {
            let first_width = (area.width as f64 * RATIO).round() as i32 - gap_size;
            let second_width = area.width - first_width - gap_size;

            let first_rect = Rect {
                x: area.x,
                y: area.y,
                width: first_width,
                height: area.height,
            };

            let rest_rect = Rect {
                x: area.x + first_rect.width + gap_size,
                y: area.y,
                width: second_width,
                height: area.height,
            };

            (first_rect, rest_rect)
        }
        Phase::Down => {
            let first_height = (area.height as f64 * RATIO).round() as i32 - gap_size;
            let second_height = area.height - first_height - gap_size;

            let first_rect = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: first_height,
            };

            let rest_rect = Rect {
                x: area.x,
                y: area.y + first_rect.height + gap_size,
                width: area.width,
                height: second_height,
            };

            (first_rect, rest_rect)
        }
        Phase::Left => {
            let first_width = (area.width as f64 * RATIO).round() as i32 - gap_size;
            let second_width = area.width - first_width - gap_size;

            let first_rect = Rect {
                x: area.x,
                y: area.y,
                width: first_width,
                height: area.height,
            };

            let rest_rect = Rect {
                x: area.x + first_rect.width + gap_size,
                y: area.y,
                width: second_width,
                height: area.height,
            };

            (rest_rect, first_rect)
        }
        Phase::Up => {
            let first_height = (area.height as f64 * RATIO).round() as i32 - gap_size;
            let second_height = area.height - first_height - gap_size;

            let first_rect = Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: first_height,
            };

            let rest_rect = Rect {
                x: area.x,
                y: area.y + first_rect.height + gap_size,
                width: area.width,
                height: second_height,
            };
            (rest_rect, first_rect)
        }
    };
    out.push(placed);

    fib(phase.next(), rest, n - 1, gap_size, out);
}


pub struct Tablet;

impl Layout for Tablet {
    fn name(&self) -> &'static str {
        "Tablet"
    }

    fn arrange_horizontal(&self, area: Rect, windows: &[WindowId], gap_size: i32, config: TilingConfig) -> Vec<Rect> {
        if windows.is_empty() {
             return Vec::new();
        }
        if windows.len() == 1 || config.tablet_hide_minimized {
            return vec![area];
        }

        let segment_size = (area.width / 4);

        let left_factor = if config.right_handed {
            1
        } else {
            3
        };

        let main_rect = Rect {
            x: area.x,
            y: area.y,
            width: (segment_size * left_factor).saturating_sub(gap_size),
            height: area.height
        };
        let remaining_rect = Rect {
            x: main_rect.width + main_rect.x + gap_size,
            y: area.y,
            width: area.width.saturating_sub(main_rect.width + gap_size),
            height: area.height,
        };
        let mut out = Vec::with_capacity(windows.len());

        let (main_rect, rest_rect) = if config.right_handed {
            (remaining_rect, main_rect)
        } else {
            (main_rect, remaining_rect)
        };
        out.push(main_rect);

        let part_size = area.height / (windows.len() - 1) as i32; // Skipping first window
        let stack_rect = Rect {
            x: rest_rect.x,
            y: rest_rect.y,
            width: rest_rect.width,
            height: part_size,
        };
        let mut stack = vec![stack_rect; windows.len() - 1];

        for i in 1..stack.len() {
            stack[i].y = stack[i - 1].y + stack[i - 1].height + gap_size;
            stack[i].height = stack[i].height.saturating_sub(gap_size);
        }
        out.extend(stack);

        out
    }

    fn arrange_vertical(&self, area: Rect, windows: &[WindowId], gap_size: i32, config: TilingConfig) -> Vec<Rect> {
        if windows.is_empty() {
            return Vec::new();
        }
        if windows.len() == 1 || config.tablet_hide_minimized {
            return vec![area];
        }

        let segment_size = (area.height / 4);

        let main_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: (segment_size * 3).saturating_sub(gap_size),
        };
        let remaining_rect = Rect {
            x: area.x,
            y: main_rect.height + main_rect.y + gap_size,
            width: area.width,
            height: area.height.saturating_sub(main_rect.height + gap_size),
        };
        let mut out = Vec::with_capacity(windows.len());
        out.push(main_rect);

        let part_size = area.width / (windows.len() - 1) as i32; // Skipping first window
        let stack_rect = Rect {
            x: remaining_rect.x,
            y: remaining_rect.y,
            width: part_size,
            height: remaining_rect.height,
        };
        let mut stack = vec![stack_rect; windows.len() - 1];

        for i in 1..stack.len() {
            stack[i].x = stack[i - 1].x + stack[i - 1].width + gap_size;
            stack[i].width = stack[i].width.saturating_sub(gap_size);
        }
        out.extend(stack);

        out
    }
}
