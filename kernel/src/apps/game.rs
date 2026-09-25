//! The Game app: Snake. A grid of `CELL`-pixel cells filling the screen
//! above the taskbar, moved on a fixed real-time tick (`tsc::now_ns`,
//! not a frame counter — so speed doesn't depend on how fast the
//! executor happens to be polling) and steered with the arrow keys
//! (`keyboard::KEY_UP`/`DOWN`/`LEFT`/`RIGHT`). The mouse is only used to
//! detect a click on the taskbar's Home button — everything else here is
//! keyboard-driven, the way a real Snake should be.

use crate::desktop;
use crate::gfx::{self, Color, FontSize};
use crate::keyboard;
use crate::mouse::MouseEvent;
use crate::net::stack::net_tick;
use crate::rng::Rng;
use crate::tsc;
use alloc::vec::Vec;
use rand_core::RngCore;

const CELL: usize = 16;
const STEP_NS: u64 = 150_000_000; // one move every 150ms
const BG: Color = Color(0x10, 0x14, 0x10);
const SNAKE_COLOR: Color = Color(0x40, 0xc0, 0x50);
const FOOD_COLOR: Color = Color(0xd0, 0x40, 0x40);
const TEXT: Color = Color(0xe8, 0xe8, 0xe8);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn delta(self) -> (i32, i32) {
        match self {
            Dir::Up => (0, -1),
            Dir::Down => (0, 1),
            Dir::Left => (-1, 0),
            Dir::Right => (1, 0),
        }
    }

    fn is_opposite(self, other: Dir) -> bool {
        matches!(
            (self, other),
            (Dir::Up, Dir::Down) | (Dir::Down, Dir::Up) | (Dir::Left, Dir::Right) | (Dir::Right, Dir::Left)
        )
    }
}

struct Game {
    cols: i32,
    rows: i32,
    body: Vec<(i32, i32)>,
    dir: Dir,
    food: (i32, i32),
    over: bool,
}

impl Game {
    fn new(cols: i32, rows: i32) -> Self {
        let start = (cols / 2, rows / 2);
        let mut game = Game {
            cols,
            rows,
            body: alloc::vec![start, (start.0 - 1, start.1), (start.0 - 2, start.1)],
            dir: Dir::Right,
            food: (0, 0),
            over: false,
        };
        game.place_food();
        game
    }

    fn place_food(&mut self) {
        let mut rng = Rng::new();
        for _ in 0..64 {
            let x = (rng.next_u32() % self.cols as u32) as i32;
            let y = (rng.next_u32() % self.rows as u32) as i32;
            if !self.body.contains(&(x, y)) {
                self.food = (x, y);
                return;
            }
        }
        self.food = (0, 0);
    }

    fn set_dir(&mut self, dir: Dir) {
        if !dir.is_opposite(self.dir) {
            self.dir = dir;
        }
    }

    fn step(&mut self) {
        if self.over {
            return;
        }
        let (dx, dy) = self.dir.delta();
        let head = self.body[0];
        let next = (head.0 + dx, head.1 + dy);

        if next.0 < 0 || next.0 >= self.cols || next.1 < 0 || next.1 >= self.rows || self.body.contains(&next) {
            self.over = true;
            return;
        }

        self.body.insert(0, next);
        if next == self.food {
            self.place_food();
        } else {
            self.body.pop();
        }
    }
}

fn draw(game: &Game, width: usize, height: usize) {
    let mut guard = gfx::SCREEN.lock();
    let Some(fb) = guard.as_mut() else { return };
    let viewport_h = height - desktop::TASKBAR_HEIGHT;
    fb.fill_rect(0, 0, width, viewport_h, BG);

    for &(x, y) in &game.body {
        fb.fill_rect(x as usize * CELL, y as usize * CELL, CELL - 1, CELL - 1, SNAKE_COLOR);
    }
    fb.fill_rect(game.food.0 as usize * CELL, game.food.1 as usize * CELL, CELL - 1, CELL - 1, FOOD_COLOR);

    if game.over {
        fb.draw_wrapped(
            "Game Over — press any key to restart",
            24,
            24,
            width - 24,
            TEXT,
            BG,
            FontSize::Size24,
        );
    } else {
        let score = game.body.len();
        fb.draw_wrapped(&alloc::format!("Score: {score}"), 8, 4, width - 8, TEXT, BG, FontSize::Size16);
    }

    desktop::draw_taskbar(fb, width, height, Some(desktop::AppId::Game));
}

pub async fn run(width: usize, height: usize) {
    let viewport_h = height - desktop::TASKBAR_HEIGHT;
    let cols = (width / CELL).max(4) as i32;
    let rows = (viewport_h / CELL).max(4) as i32;
    let home = desktop::home_rect(width, height);

    let mut game = Game::new(cols, rows);
    let mut mouse_x: i32 = (width / 2) as i32;
    let mut mouse_y: i32 = (height / 2) as i32;
    let mut last_step_ns = tsc::now_ns();

    draw(&game, width, height);
    if let Some(fb) = gfx::SCREEN.lock().as_mut() {
        // Snapshotted so a cursor-only redraw (the mouse moved between
        // game ticks, with no direction change or step) is a cheap
        // restore instead of re-running draw() — most redraws here are
        // still full game-state redraws (a real-time tick every
        // STEP_NS), this just avoids extra ones squeezed in between.
        fb.save_content();
        fb.draw_cursor(mouse_x as usize, mouse_y as usize);
        fb.present();
    }

    loop {
        let mut content_dirty = false;
        let mut cursor_dirty = false;

        match keyboard::pop_key() {
            Some(keyboard::KEY_UP) => game.set_dir(Dir::Up),
            Some(keyboard::KEY_DOWN) => game.set_dir(Dir::Down),
            Some(keyboard::KEY_LEFT) => game.set_dir(Dir::Left),
            Some(keyboard::KEY_RIGHT) => game.set_dir(Dir::Right),
            Some(_) if game.over => {
                game = Game::new(cols, rows);
                last_step_ns = tsc::now_ns();
                content_dirty = true;
            }
            _ => {}
        }

        // Drains every currently-queued mouse event before redrawing —
        // see desktop.rs's `run` for why this matters for responsiveness
        // under real, fast mouse motion.
        while let Some(event) = crate::mouse::poll_event() {
            match event {
                MouseEvent::Move { dx, dy } => {
                    mouse_x = (mouse_x + dx).clamp(0, width as i32 - 1);
                    mouse_y = (mouse_y + dy).clamp(0, height as i32 - 1);
                    cursor_dirty = true;
                }
                MouseEvent::LeftDown => {
                    if home.contains(mouse_x as usize, mouse_y as usize) {
                        return;
                    }
                }
                _ => {}
            }
        }

        let now = tsc::now_ns();
        if !game.over && now.saturating_sub(last_step_ns) >= STEP_NS {
            game.step();
            last_step_ns = now;
            content_dirty = true;
        }

        if content_dirty {
            draw(&game, width, height);
            if let Some(fb) = gfx::SCREEN.lock().as_mut() {
                fb.save_content();
                fb.draw_cursor(mouse_x as usize, mouse_y as usize);
                fb.present();
            }
        } else if cursor_dirty {
            if let Some(fb) = gfx::SCREEN.lock().as_mut() {
                fb.restore_content();
                fb.draw_cursor(mouse_x as usize, mouse_y as usize);
                fb.present();
            }
        } else {
            net_tick().await;
        }
    }
}
