//! The Browser app: a thin wrapper around `net::http::fetch`, the same
//! DNS/TCP/TLS/HTTP/render engine the kernel always had, now confined to
//! the screen area above the taskbar and given the taskbar's Home rect
//! so a click there exits back to the desktop instead of being treated
//! as a page click.

use crate::desktop;

/// A reasonable default landing page — same reasoning `net::http`'s old
/// boot-time demo used for its own hardcoded URL: real enough content to
/// prove the whole stack works, light enough (mobile skin, see
/// `MOBILE_USER_AGENT`) to render quickly on this kernel's CSS engine.
const HOME_PAGE: &str = "https://en.wikipedia.org/wiki/Main_Page";

pub async fn run(width: usize, height: usize) {
    let viewport_h = height - desktop::TASKBAR_HEIGHT;
    let home = desktop::home_rect(width, height);
    if let Some(fb) = crate::gfx::SCREEN.lock().as_mut() {
        desktop::draw_taskbar(fb, width, height, Some(desktop::AppId::Browser));
    }
    crate::net::http::fetch(HOME_PAGE, viewport_h, home).await;
}
