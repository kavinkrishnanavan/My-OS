//! The desktop shell: a taskbar (Home/logo button plus one button per
//! app) drawn across the bottom of the screen, and a simple desktop
//! background above it. `run()` is the top-level loop spawned from
//! `main` in place of the old "just fetch one hardcoded URL fullscreen"
//! demo — it waits for a taskbar click and hands the whole screen over
//! to that app's own event loop (`apps::files`/`browser`/`calculator`/
//! `game`) until the app itself reports the user clicked Home, then
//! redraws the desktop and waits again.
//!
//! Every app is handed the Home button's rect (`home_rect`) and is
//! expected to check clicks against it itself — same reason every app
//! also owns its own mouse-position tracking (`mouse.rs` only reports
//! relative deltas; only the current screen's bounds are known to the
//! loop actually running).

use crate::apps;
use crate::gfx::{self, Color, FontSize};
use crate::mouse::MouseEvent;
use crate::net::stack::net_tick;

pub const TASKBAR_HEIGHT: usize = 44;

const DESKTOP_BG: Color = Color(0x1c, 0x3c, 0x5a);
const TASKBAR_BG: Color = Color(0x20, 0x20, 0x24);
const LOGO_BG: Color = Color(0x2f, 0x6f, 0xd8);
const BUTTON_BG: Color = Color(0x38, 0x38, 0x40);
const ACTIVE_BG: Color = Color(0x50, 0x50, 0x5c);
const TEXT_COLOR: Color = Color(0xf0, 0xf0, 0xf0);

/// An axis-aligned screen-space rectangle, `[x0, x1) x [y0, y1)` — shared
/// by the desktop's own taskbar hit-testing and every app's click
/// handling (including `net::http`'s Browser engine, which checks a
/// click against the Home rect the same way it checks its own link
/// regions).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Rect {
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AppId {
    Files,
    Browser,
    Calculator,
    Game,
}

const APP_LIST: [(AppId, &str); 4] = [
    (AppId::Files, "Files"),
    (AppId::Browser, "Browser"),
    (AppId::Calculator, "Calc"),
    (AppId::Game, "Game"),
];

const HOME_WIDTH: usize = 84;
const BUTTON_WIDTH: usize = 96;
const BUTTON_GAP: usize = 10;

/// The taskbar's Home/logo button — clicking it always means "go back to
/// the desktop", whether that click happened while an app is running
/// (every app checks this itself) or, harmlessly, while already on the
/// desktop.
pub fn home_rect(_width: usize, height: usize) -> Rect {
    Rect { x0: 8, y0: height - TASKBAR_HEIGHT + 6, x1: 8 + HOME_WIDTH, y1: height - 6 }
}

fn app_rect(index: usize, height: usize) -> Rect {
    let start_x = 8 + HOME_WIDTH + 24;
    let x0 = start_x + index * (BUTTON_WIDTH + BUTTON_GAP);
    Rect { x0, y0: height - TASKBAR_HEIGHT + 6, x1: x0 + BUTTON_WIDTH, y1: height - 6 }
}

/// Draws the taskbar strip (Home button + one button per app,
/// highlighting `active` if it names one of them) across the full width
/// of the screen. Every app redraws this on its own every frame it
/// redraws anything, both so the active button stays highlighted and
/// because nothing else here keeps the taskbar's pixels from being
/// touched by an app's own full-content redraws.
pub fn draw_taskbar(fb: &mut gfx::Framebuffer, width: usize, height: usize, active: Option<AppId>) {
    let bar_y = height - TASKBAR_HEIGHT;
    fb.fill_rect(0, bar_y, width, TASKBAR_HEIGHT, TASKBAR_BG);

    let home = home_rect(width, height);
    fb.fill_rect(home.x0, home.y0, home.x1 - home.x0, home.y1 - home.y0, LOGO_BG);
    fb.draw_wrapped(
        "MyOS",
        home.x0 + 10,
        home.y0 + (home.y1 - home.y0).saturating_sub(16) / 2,
        home.x1 - 4,
        TEXT_COLOR,
        LOGO_BG,
        FontSize::Size16,
    );

    for (i, (id, label)) in APP_LIST.iter().enumerate() {
        let r = app_rect(i, height);
        let button_bg = if active == Some(*id) { ACTIVE_BG } else { BUTTON_BG };
        fb.fill_rect(r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0, button_bg);
        fb.draw_wrapped(
            label,
            r.x0 + 10,
            r.y0 + (r.y1 - r.y0).saturating_sub(16) / 2,
            r.x1 - 4,
            TEXT_COLOR,
            button_bg,
            FontSize::Size16,
        );
    }
}

/// Temporary diagnostic: prints how many raw bytes IRQ1/IRQ12 have ever
/// delivered, in the top-right corner — see `mouse::irq_byte_count`'s doc
/// comment. Answers "is any interrupt reaching the keyboard/mouse driver
/// at all" by just looking at the screen, no serial log needed. Worth
/// removing once real mouse movement is confirmed working end to end.
fn draw_irq_diagnostic(fb: &mut gfx::Framebuffer, width: usize) {
    let text = alloc::format!(
        "kbd irq bytes: {}  mouse irq bytes: {}",
        crate::keyboard::irq_byte_count(),
        crate::mouse::irq_byte_count(),
    );
    const BOX_W: usize = 340;
    let x0 = width.saturating_sub(BOX_W);
    fb.fill_rect(x0, 20, BOX_W, 20, TASKBAR_BG);
    fb.draw_wrapped(&text, x0 + 6, 22, width - 4, gfx::GRAY, TASKBAR_BG, FontSize::Size16);
}

fn draw_desktop(fb: &mut gfx::Framebuffer, width: usize, height: usize) {
    fb.fill_rect(0, 0, width, height - TASKBAR_HEIGHT, DESKTOP_BG);
    fb.draw_wrapped("MyOS", 24, 24, width - 24, TEXT_COLOR, DESKTOP_BG, FontSize::Size32);
    fb.draw_wrapped(
        "Click an app in the taskbar to open it.",
        24,
        70,
        width - 24,
        gfx::GRAY,
        DESKTOP_BG,
        FontSize::Size16,
    );
    draw_irq_diagnostic(fb, width);
    draw_taskbar(fb, width, height, None);
}

/// The desktop's own main loop: draw the desktop, wait for a taskbar app
/// click, run that app until it returns (clicked Home), then loop back to
/// drawing the desktop. Never returns — this replaces the old boot-time
/// "fetch one hardcoded page fullscreen" demo as the thing `main.rs`
/// spawns onto the executor.
pub async fn run() -> ! {
    loop {
        let (width, height) = loop {
            let mut guard = gfx::SCREEN.lock();
            if let Some(fb) = guard.as_mut() {
                let (w, h) = (fb.width(), fb.height());
                draw_desktop(fb, w, h);
                break (w, h);
            }
            drop(guard);
            net_tick().await;
        };

        let mut mouse_x: i32 = (width / 2) as i32;
        let mut mouse_y: i32 = (height - TASKBAR_HEIGHT / 2) as i32;
        if let Some(fb) = gfx::SCREEN.lock().as_mut() {
            fb.draw_cursor(mouse_x as usize, mouse_y as usize);
        }

        // Forces a redraw every ~50 wakeups (roughly 500ms, since
        // `net_tick` wakes on every ~10ms PIT tick) purely so the IRQ
        // diagnostic counters in the corner visibly refresh even when
        // zero real mouse/keyboard events are arriving — otherwise a
        // stuck-at-zero counter would never repaint and this whole
        // diagnostic would be useless for telling "truly stuck at zero"
        // apart from "just hasn't redrawn recently".
        let mut idle_ticks: u32 = 0;

        let clicked = loop {
            let mut redraw = false;
            match crate::mouse::poll_event() {
                Some(MouseEvent::Move { dx, dy }) => {
                    mouse_x = (mouse_x + dx).clamp(0, width as i32 - 1);
                    mouse_y = (mouse_y + dy).clamp(0, height as i32 - 1);
                    redraw = true;
                }
                Some(MouseEvent::LeftDown) => {
                    let (mx, my) = (mouse_x as usize, mouse_y as usize);
                    if let Some((id, _)) = APP_LIST.iter().enumerate().find_map(|(i, (id, _))| {
                        app_rect(i, height).contains(mx, my).then(|| (*id, i))
                    }) {
                        break id;
                    }
                }
                Some(MouseEvent::LeftUp) | Some(MouseEvent::ScrollUp) | Some(MouseEvent::ScrollDown) | None => {}
            }

            idle_ticks += 1;
            if idle_ticks >= 50 {
                idle_ticks = 0;
                redraw = true;
            }

            if redraw {
                if let Some(fb) = gfx::SCREEN.lock().as_mut() {
                    draw_desktop(fb, width, height);
                    fb.draw_cursor(mouse_x as usize, mouse_y as usize);
                }
            }
            net_tick().await;
        };

        match clicked {
            AppId::Files => apps::files::run(width, height).await,
            AppId::Browser => apps::browser::run(width, height).await,
            AppId::Calculator => apps::calculator::run(width, height).await,
            AppId::Game => apps::game::run(width, height).await,
        }
    }
}
