//! The Files app: lists the root directory of the on-disk FAT filesystem
//! (`fs::list_root`) and, on clicking a file, reads it (`fs::read`) and
//! shows its contents as text. No subdirectories, no editing — just
//! enough to prove the filesystem is browsable from the desktop, not
//! only from kernel boot-time diagnostics or a userland ELF program.

use crate::desktop::{self, Rect};
use crate::fs;
use crate::gfx::{self, Color, FontSize};
use crate::mouse::MouseEvent;
use crate::net::stack::net_tick;
use alloc::string::String;
use alloc::vec::Vec;

const MARGIN: usize = 24;
const ROW_HEIGHT: usize = 28;
const LIST_TOP: usize = 70;
const BG: Color = Color(0x18, 0x18, 0x1c);
const TEXT: Color = Color(0xe8, 0xe8, 0xe8);
const ROW_HOVER: Color = Color(0x28, 0x28, 0x30);

enum View {
    List,
    Content { name: String, text: String },
}

fn back_rect(width: usize) -> Rect {
    Rect { x0: width - 24 - 90, y0: 24, x1: width - 24, y1: 24 + 28 }
}

fn row_rect(index: usize, width: usize) -> Rect {
    let y0 = LIST_TOP + index * ROW_HEIGHT;
    Rect { x0: MARGIN, y0, x1: width - MARGIN, y1: y0 + ROW_HEIGHT }
}

fn draw(view: &View, entries: &[String], width: usize, height: usize) {
    let mut guard = gfx::SCREEN.lock();
    let Some(fb) = guard.as_mut() else { return };
    let viewport_h = height - desktop::TASKBAR_HEIGHT;
    fb.fill_rect(0, 0, width, viewport_h, BG);

    match view {
        View::List => {
            fb.draw_wrapped("Files", MARGIN, 24, width - MARGIN, TEXT, BG, FontSize::Size24);
            if entries.is_empty() {
                fb.draw_wrapped("(root directory is empty)", MARGIN, LIST_TOP, width - MARGIN, gfx::GRAY, BG, FontSize::Size16);
            }
            for (i, name) in entries.iter().enumerate() {
                let r = row_rect(i, width);
                if r.y1 > viewport_h {
                    break;
                }
                fb.fill_rect(r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0, ROW_HOVER);
                fb.draw_wrapped(name, r.x0 + 12, r.y0 + 6, r.x1 - 8, TEXT, ROW_HOVER, FontSize::Size16);
            }
        }
        View::Content { name, text } => {
            fb.draw_wrapped(name, MARGIN, 24, width - MARGIN - 100, TEXT, BG, FontSize::Size24);
            let back = back_rect(width);
            fb.fill_rect(back.x0, back.y0, back.x1 - back.x0, back.y1 - back.y0, ROW_HOVER);
            fb.draw_wrapped("< Back", back.x0 + 10, back.y0 + 6, back.x1 - 4, TEXT, ROW_HOVER, FontSize::Size16);
            let shown = if text.len() > 4000 { &text[..4000] } else { text.as_str() };
            fb.draw_wrapped(shown, MARGIN, LIST_TOP, width - MARGIN, gfx::GRAY, BG, FontSize::Size16);
        }
    }

    desktop::draw_taskbar(fb, width, height, Some(desktop::AppId::Files));
}

pub async fn run(width: usize, height: usize) {
    let entries: Vec<String> = fs::list_root().unwrap_or_default();
    let mut view = View::List;
    let home = desktop::home_rect(width, height);

    let mut mouse_x: i32 = (width / 2) as i32;
    let mut mouse_y: i32 = (height / 2) as i32;

    draw(&view, &entries, width, height);
    if let Some(fb) = gfx::SCREEN.lock().as_mut() {
        fb.draw_cursor(mouse_x as usize, mouse_y as usize);
    }

    loop {
        let mut redraw = false;
        match crate::mouse::poll_event() {
            Some(MouseEvent::Move { dx, dy }) => {
                mouse_x = (mouse_x + dx).clamp(0, width as i32 - 1);
                mouse_y = (mouse_y + dy).clamp(0, height as i32 - 1);
                redraw = true;
            }
            Some(MouseEvent::LeftDown) => {
                let (mx, my) = (mouse_x as usize, mouse_y as usize);
                if home.contains(mx, my) {
                    return;
                }
                match &view {
                    View::List => {
                        if let Some((i, _)) = entries.iter().enumerate().find(|(i, _)| row_rect(*i, width).contains(mx, my)) {
                            let name = entries[i].clone();
                            let text = match fs::read(&name) {
                                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                                Err(e) => alloc::format!("(failed to read {name}: {e})"),
                            };
                            view = View::Content { name, text };
                            redraw = true;
                        }
                    }
                    View::Content { .. } => {
                        if back_rect(width).contains(mx, my) {
                            view = View::List;
                            redraw = true;
                        }
                    }
                }
            }
            Some(MouseEvent::LeftUp) | Some(MouseEvent::ScrollUp) | Some(MouseEvent::ScrollDown) | None => {}
        }

        if !redraw {
            net_tick().await;
            continue;
        }
        draw(&view, &entries, width, height);
        if let Some(fb) = gfx::SCREEN.lock().as_mut() {
            fb.draw_cursor(mouse_x as usize, mouse_y as usize);
        }
    }
}
