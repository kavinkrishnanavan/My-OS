//! The Calculator app: a button grid building up an expression string,
//! evaluated left-to-right with standard `*`/`/` before `+`/`-`
//! precedence (no parentheses — out of scope for a four-function
//! calculator). Mouse-only; no keyboard numeric entry.

use crate::desktop::{self, Rect};
use crate::gfx::{self, Color, FontSize};
use crate::mouse::MouseEvent;
use crate::net::stack::net_tick;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const BG: Color = Color(0x18, 0x18, 0x1c);
const DISPLAY_BG: Color = Color(0x10, 0x10, 0x14);
const TEXT: Color = Color(0xe8, 0xe8, 0xe8);
const BUTTON_BG: Color = Color(0x30, 0x30, 0x38);
const OP_BUTTON_BG: Color = Color(0x40, 0x50, 0x70);

const GRID: [[&str; 4]; 5] = [
    ["C", "DEL", "/", "*"],
    ["7", "8", "9", "-"],
    ["4", "5", "6", "+"],
    ["1", "2", "3", "="],
    ["0", ".", "=", ""],
];

const GRID_TOP: usize = 110;
const CELL_W: usize = 90;
const CELL_H: usize = 56;
const CELL_GAP: usize = 8;
const GRID_LEFT: usize = 24;

fn cell_rect(row: usize, col: usize) -> Rect {
    let x0 = GRID_LEFT + col * (CELL_W + CELL_GAP);
    let y0 = GRID_TOP + row * (CELL_H + CELL_GAP);
    Rect { x0, y0, x1: x0 + CELL_W, y1: y0 + CELL_H }
}

/// Tokenizes `expr` into numbers and `+ - * /` operators, then evaluates
/// with `*`/`/` before `+`/`-` — a plain two-pass precedence evaluator,
/// no parentheses or unary minus support.
fn evaluate(expr: &str) -> Option<f64> {
    let mut nums: Vec<f64> = Vec::new();
    let mut ops: Vec<u8> = Vec::new();
    let mut cur = String::new();

    for c in expr.chars() {
        if c.is_ascii_digit() || c == '.' {
            cur.push(c);
        } else if c == '+' || c == '-' || c == '*' || c == '/' {
            if cur.is_empty() {
                return None;
            }
            nums.push(cur.parse().ok()?);
            cur.clear();
            ops.push(c as u8);
        } else {
            return None;
        }
    }
    if cur.is_empty() {
        return None;
    }
    nums.push(cur.parse().ok()?);

    // Pass 1: fold `*` and `/`, left to right.
    let mut nums2: Vec<f64> = Vec::with_capacity(nums.len());
    let mut ops2: Vec<u8> = Vec::with_capacity(ops.len());
    nums2.push(nums[0]);
    for (i, &op) in ops.iter().enumerate() {
        let rhs = nums[i + 1];
        match op {
            b'*' => {
                let lhs = nums2.pop().unwrap();
                nums2.push(lhs * rhs);
            }
            b'/' => {
                if rhs == 0.0 {
                    return None;
                }
                let lhs = nums2.pop().unwrap();
                nums2.push(lhs / rhs);
            }
            _ => {
                ops2.push(op);
                nums2.push(rhs);
            }
        }
    }

    // Pass 2: fold the remaining `+`/`-`, left to right.
    let mut result = nums2[0];
    for (i, &op) in ops2.iter().enumerate() {
        let rhs = nums2[i + 1];
        if op == b'+' {
            result += rhs;
        } else {
            result -= rhs;
        }
    }
    Some(result)
}

/// `no_std` has no `f64::fract`/`abs` without pulling in `libm` — an
/// integer round-trip comparison does the same "is this a whole number"
/// check without it.
fn format_result(value: f64) -> String {
    let rounded = value as i64;
    if rounded as f64 == value {
        format!("{rounded}")
    } else {
        format!("{value}")
    }
}

fn draw(expr: &str, width: usize, height: usize) {
    let mut guard = gfx::SCREEN.lock();
    let Some(fb) = guard.as_mut() else { return };
    let viewport_h = height - desktop::TASKBAR_HEIGHT;
    fb.fill_rect(0, 0, width, viewport_h, BG);

    fb.fill_rect(GRID_LEFT, 24, width - 2 * GRID_LEFT, 70, DISPLAY_BG);
    let shown = if expr.is_empty() { "0" } else { expr };
    fb.draw_wrapped(shown, GRID_LEFT + 12, 24 + 22, width - GRID_LEFT - 12, TEXT, DISPLAY_BG, FontSize::Size24);

    for (row, cells) in GRID.iter().enumerate() {
        for (col, label) in cells.iter().enumerate() {
            if label.is_empty() {
                continue;
            }
            let r = cell_rect(row, col);
            let is_op = matches!(*label, "/" | "*" | "-" | "+" | "=" | "C" | "DEL");
            let bg = if is_op { OP_BUTTON_BG } else { BUTTON_BG };
            fb.fill_rect(r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0, bg);
            fb.draw_wrapped(label, r.x0 + 12, r.y0 + 18, r.x1 - 4, TEXT, bg, FontSize::Size24);
        }
    }

    desktop::draw_taskbar(fb, width, height, Some(desktop::AppId::Calculator));
}

pub async fn run(width: usize, height: usize) {
    let mut expr = String::new();
    let home = desktop::home_rect(width, height);

    let mut mouse_x: i32 = (width / 2) as i32;
    let mut mouse_y: i32 = (height / 2) as i32;

    draw(&expr, width, height);
    if let Some(fb) = gfx::SCREEN.lock().as_mut() {
        fb.draw_cursor(mouse_x as usize, mouse_y as usize);
        fb.present();
    }

    loop {
        let mut redraw = false;
        // Drains every currently-queued mouse event before redrawing —
        // see desktop.rs's `run` for why this matters for responsiveness
        // under real, fast mouse motion.
        while let Some(event) = crate::mouse::poll_event() {
            match event {
                MouseEvent::Move { dx, dy } => {
                    mouse_x = (mouse_x + dx).clamp(0, width as i32 - 1);
                    mouse_y = (mouse_y + dy).clamp(0, height as i32 - 1);
                    redraw = true;
                }
                MouseEvent::LeftDown => {
                    let (mx, my) = (mouse_x as usize, mouse_y as usize);
                    if home.contains(mx, my) {
                        return;
                    }
                    'hit: for (row, cells) in GRID.iter().enumerate() {
                        for (col, label) in cells.iter().enumerate() {
                            if label.is_empty() || !cell_rect(row, col).contains(mx, my) {
                                continue;
                            }
                            match *label {
                                "C" => expr.clear(),
                                "DEL" => {
                                    expr.pop();
                                }
                                "=" => {
                                    expr = match evaluate(&expr) {
                                        Some(v) => format_result(v),
                                        None => String::from("Error"),
                                    };
                                }
                                token => expr.push_str(token),
                            }
                            redraw = true;
                            break 'hit;
                        }
                    }
                }
                MouseEvent::LeftUp | MouseEvent::ScrollUp | MouseEvent::ScrollDown => {}
            }
        }

        if !redraw {
            net_tick().await;
            continue;
        }
        draw(&expr, width, height);
        if let Some(fb) = gfx::SCREEN.lock().as_mut() {
            fb.draw_cursor(mouse_x as usize, mouse_y as usize);
            fb.present();
        }
    }
}
