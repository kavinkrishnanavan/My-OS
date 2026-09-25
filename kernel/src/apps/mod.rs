//! The desktop's four apps. Each exposes `pub async fn run(width: usize,
//! height: usize)`, is handed the full screen size, draws its own content
//! above `crate::desktop::TASKBAR_HEIGHT` pixels of taskbar at the
//! bottom (via `crate::desktop::draw_taskbar`), and returns once the user
//! clicks the taskbar's Home button — at which point `desktop::run`'s
//! loop redraws the desktop and waits for the next click.

pub mod browser;
pub mod calculator;
pub mod files;
pub mod game;
