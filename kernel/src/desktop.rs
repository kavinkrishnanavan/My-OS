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
const BUTTON_WIDTH: usize = 108;
const BUTTON_GAP: usize = 10;
const ICON_RADIUS: usize = 8;

/// Draws the "MyOS" logo — concentric circles, the same simple vector-
/// shape approach `draw_app_icon` uses for the per-app icons below (no
/// image assets in this kernel, just basic shapes) — centered at
/// `(cx, cy)`.
fn draw_logo_icon(fb: &mut gfx::Framebuffer, cx: usize, cy: usize) {
    fb.fill_circle(cx, cy, ICON_RADIUS, TEXT_COLOR, LOGO_BG);
    fb.fill_circle(cx, cy, ICON_RADIUS - 3, LOGO_BG, TEXT_COLOR);
}

/// Draws a small, simple vector icon for `id` centered at `(cx, cy)` —
/// beside each taskbar button's text label, the way a real desktop's
/// taskbar pairs an icon with every app's name. Deliberately plain
/// geometric shapes (no image assets/decoder needed for the desktop
/// shell itself): a folder for Files, a globe for the Browser, a
/// keypad for the Calculator, a gamepad for the Game.
fn draw_app_icon(fb: &mut gfx::Framebuffer, id: AppId, cx: usize, cy: usize, bg: Color) {
    match id {
        AppId::Files => {
            const FOLDER: Color = Color(0xe8, 0xb8, 0x3c);
            fb.fill_rect(cx - 8, cy - 5, 16, 11, FOLDER);
            fb.fill_rect(cx - 8, cy - 8, 8, 4, FOLDER);
        }
        AppId::Browser => {
            const GLOBE: Color = Color(0x4a, 0x9c, 0xe8);
            fb.fill_circle(cx, cy, ICON_RADIUS, GLOBE, bg);
            fb.fill_rect(cx - ICON_RADIUS, cy - 1, ICON_RADIUS * 2, 2, TASKBAR_BG);
            fb.fill_rect(cx - 2, cy - ICON_RADIUS, 3, ICON_RADIUS * 2, TASKBAR_BG);
        }
        AppId::Calculator => {
            const BODY: Color = Color(0xc4, 0xc4, 0xcc);
            const KEY: Color = Color(0x30, 0x30, 0x38);
            fb.fill_rect(cx - 8, cy - 8, 16, 16, BODY);
            fb.fill_rect(cx - 6, cy - 6, 12, 4, KEY);
            for (kx, ky) in [(-6, 1), (-1, 1), (4, 1), (-6, 5), (-1, 5), (4, 5)] {
                fb.fill_rect((cx as isize + kx) as usize, (cy as isize + ky) as usize, 3, 3, KEY);
            }
        }
        AppId::Game => {
            const PAD: Color = Color(0x8a, 0x50, 0xd8);
            const BUTTON: Color = Color(0xf0, 0xf0, 0xf0);
            fb.fill_rect(cx - 9, cy - 5, 18, 10, PAD);
            fb.fill_circle(cx - 4, cy, 2, BUTTON, PAD);
            fb.fill_circle(cx + 4, cy, 2, BUTTON, PAD);
        }
    }
}

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
    let home_mid_y = home.y0 + (home.y1 - home.y0) / 2;
    fb.fill_rect(home.x0, home.y0, home.x1 - home.x0, home.y1 - home.y0, LOGO_BG);
    draw_logo_icon(fb, home.x0 + 8 + ICON_RADIUS, home_mid_y);
    fb.draw_wrapped(
        "MyOS",
        home.x0 + 20 + ICON_RADIUS,
        home_mid_y.saturating_sub(8),
        home.x1 - 4,
        TEXT_COLOR,
        LOGO_BG,
        FontSize::Size16,
    );

    for (i, (id, label)) in APP_LIST.iter().enumerate() {
        let r = app_rect(i, height);
        let mid_y = r.y0 + (r.y1 - r.y0) / 2;
        let button_bg = if active == Some(*id) { ACTIVE_BG } else { BUTTON_BG };
        fb.fill_rect(r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0, button_bg);
        draw_app_icon(fb, *id, r.x0 + 8 + ICON_RADIUS, mid_y, button_bg);
        fb.draw_wrapped(
            label,
            r.x0 + 20 + ICON_RADIUS,
            mid_y.saturating_sub(8),
            r.x1 - 4,
            TEXT_COLOR,
            button_bg,
            FontSize::Size16,
        );
    }
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
            // Nothing in this loop below ever changes the desktop's own
            // content (a click either launches an app — leaving this loop
            // entirely — or does nothing visible), only the cursor
            // position, so the content is only ever drawn once here and
            // snapshotted; every redraw after this is just a cheap
            // restore_content + cursor draw, not a full desktop re-blit.
            fb.save_content();
            fb.draw_cursor(mouse_x as usize, mouse_y as usize);
            fb.present();
        }

        let clicked = 'wait: loop {
            let mut redraw = false;
            // Drains every currently-queued event before redrawing even
            // once, instead of "pop one, redraw, repeat" — redrawing is a
            // comparatively expensive full-screen software blit, and a
            // real PS/2 mouse queues many small-delta Move packets per
            // screen refresh. Redrawing per-event throttled the whole
            // pipeline down to roughly one screen update per packet,
            // which is what made the cursor feel like it needed a lot of
            // physical motion to move a little on screen.
            while let Some(event) = crate::mouse::poll_event() {
                match event {
                    MouseEvent::Move { dx, dy } => {
                        mouse_x = (mouse_x + dx).clamp(0, width as i32 - 1);
                        mouse_y = (mouse_y + dy).clamp(0, height as i32 - 1);
                        redraw = true;
                    }
                    MouseEvent::LeftDown => {
                        let (mx, my) = (mouse_x as usize, mouse_y as usize);
                        if let Some((id, _)) = APP_LIST.iter().enumerate().find_map(|(i, (id, _))| {
                            app_rect(i, height).contains(mx, my).then(|| (*id, i))
                        }) {
                            break 'wait id;
                        }
                    }
                    MouseEvent::LeftUp | MouseEvent::ScrollUp | MouseEvent::ScrollDown => {}
                }
            }

            if redraw {
                if let Some(fb) = gfx::SCREEN.lock().as_mut() {
                    fb.restore_content();
                    fb.draw_cursor(mouse_x as usize, mouse_y as usize);
                    fb.present();
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
