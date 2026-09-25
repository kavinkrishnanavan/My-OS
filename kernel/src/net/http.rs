//! The actual "access a website" part: parse a URL, resolve the host
//! over DNS, open a TCP connection (through TLS first if it's
//! `https://`), speak just enough HTTP/1.1 to GET a page, and draw the
//! result onto the framebuffer as an actual page (see `render` below) as
//! well as logging it over serial.
//!
//! Every wait in here is `net::stack::net_tick().await` — hand control
//! back to the executor, get resumed when the NIC IRQ or PIT tick says
//! "check again", never a spin loop.

use crate::desktop::Rect;
use crate::gfx;
use crate::img;
use crate::layout;
use crate::net::stack::{self, net_tick};
use crate::net::tcp_stream::TcpStream;
use crate::net::tls;
use crate::serial_println;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use noto_sans_mono_bitmap::RasterHeight;
use smoltcp::socket::dns::GetQueryResultError;
use smoltcp::wire::{DnsQueryType, IpAddress};

struct Url<'a> {
    https: bool,
    host: &'a str,
    port: u16,
    path: &'a str,
}

/// Parses `[http[s]://]host[:port][/path]`. No query-string handling
/// beyond passing it straight through as part of the path — this is a
/// URL-shaped-string parser, not a general URI implementation.
fn parse_url(url: &str) -> Url<'_> {
    let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        (false, url)
    };

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };

    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(if https { 443 } else { 80 })),
        None => (authority, if https { 443 } else { 80 }),
    };

    Url { https, host, port, path }
}

const MAX_REDIRECTS: u32 = 10;

/// The Browser app's engine (`apps::browser` calls this). Fetches `url`
/// and draws it into the top `viewport_h` pixels of the screen, leaving
/// the desktop taskbar (below `viewport_h`) untouched. `home` is the
/// taskbar's Home button's screen rect — clicking it exits back to the
/// desktop (returns normally) instead of being treated as a page click.
pub async fn fetch(url: &str, viewport_h: usize, home: Rect) {
    serial_println!("http: waiting for DHCP lease...");
    while !stack::has_ip() {
        net_tick().await;
    }

    let mut current = String::from(url);
    // Outer loop: user-driven navigation (clicking a link inside
    // `render_page`, which returns the clicked-through URL instead of
    // ever returning normally on its own) — unbounded, since there's no
    // sense in which "the user clicked too many links" should ever stop
    // working. The inner loop is the *existing*, still-bounded
    // (`MAX_REDIRECTS`) HTTP-redirect-following logic for a single
    // navigation — a real 3xx-loop protection, a different concept from
    // "how many pages has the user visited this session".
    'navigate: loop {
        for redirect in 0..=MAX_REDIRECTS {
            let target = parse_url(&current);

            render_status(&format!("Resolving {}...", target.host), viewport_h);
            serial_println!("http: resolving {}", target.host);
            let ip = match resolve(target.host).await {
                Some(ip) => ip,
                None => {
                    render_status(&format!("Could not resolve {}", target.host), viewport_h);
                    serial_println!("http: DNS resolution for {} failed", target.host);
                    return;
                }
            };
            serial_println!("http: {} -> {}", target.host, ip);

            render_status(
                &format!(
                    "Connecting to {} ({ip}) over {}...",
                    target.host,
                    if target.https { "TLS" } else { "plain HTTP" }
                ),
                viewport_h,
            );
            let response = match get(ip, &target).await {
                Ok(body) => body,
                Err(e) => {
                    render_status(&format!("Request to {} failed: {e}", target.host), viewport_h);
                    serial_println!("http: request failed: {}", e);
                    return;
                }
            };
            serial_println!("http: received {} bytes", response.len());

            let (headers, body) = split_headers_body(&response);

            if let Some(location) = redirect_location(headers) {
                if redirect == MAX_REDIRECTS {
                    render_status("Too many redirects", viewport_h);
                    serial_println!("http: too many redirects, giving up at {}", location);
                    return;
                }
                let next = resolve_url(&target, location);
                serial_println!("http: redirecting to {}", next);
                current = next;
                continue;
            }

            let body = if is_chunked(headers) {
                dechunk(body)
            } else {
                body.to_vec()
            };

            let text = String::from_utf8_lossy(&body);
            match render_page(&target, &text, viewport_h, home).await {
                PageAction::Navigate(clicked_url) => {
                    serial_println!("http: navigating to {}", clicked_url);
                    current = clicked_url;
                    continue 'navigate;
                }
                PageAction::Home | PageAction::Done => return,
            }
        }
    }
}

/// Same redirect-following GET as the main `fetch` loop, minus the
/// on-screen status updates — used for images embedded in a page, where
/// a failed fetch should just mean "skip this image", not "report an
/// error to the user".
async fn fetch_bytes_quiet(url: &str) -> Option<Vec<u8>> {
    let mut current = String::from(url);
    for redirect in 0..=MAX_REDIRECTS {
        let target = parse_url(&current);
        let ip = resolve(target.host).await?;
        let response = get(ip, &target).await.ok()?;
        let (headers, body) = split_headers_body(&response);

        if let Some(location) = redirect_location(headers) {
            if redirect == MAX_REDIRECTS {
                return None;
            }
            current = resolve_url(&target, location);
            continue;
        }

        return Some(if is_chunked(headers) { dechunk(body) } else { body.to_vec() });
    }
    None
}

/// If `headers` is a 3xx response with a `Location`, returns that location.
fn redirect_location(headers: &str) -> Option<&str> {
    let status_line = headers.lines().next()?;
    let code: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    if !(300..400).contains(&code) {
        return None;
    }
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("location").then(|| value.trim())
    })
}

/// Resolves a possibly-relative URL (a redirect's `Location`, or an
/// `<img src>`) against the page it came from: absolute URLs pass
/// through, `//host/path` keeps `from`'s scheme, `/path` is root-relative
/// on `from`'s host, and anything else is relative to `from`'s directory.
fn resolve_url(from: &Url<'_>, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        return String::from(location);
    }
    let scheme = if from.https { "https" } else { "http" };
    if let Some(rest) = location.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }
    if location.starts_with('/') {
        return format!("{scheme}://{}{}", from.host, location);
    }
    let dir = match from.path.rfind('/') {
        Some(i) => &from.path[..=i],
        None => "/",
    };
    format!("{scheme}://{}{}{}", from.host, dir, location)
}

async fn get(ip: IpAddress, target: &Url<'_>) -> Result<Vec<u8>, &'static str> {
    let tcp = TcpStream::connect(ip, target.port).await?;
    if target.https {
        tls::get(tcp, target.host, target.path).await
    } else {
        plain_get(tcp, target.host, target.path).await
    }
}

async fn plain_get(mut tcp: TcpStream, host: &str, path: &str) -> Result<Vec<u8>, &'static str> {
    use embedded_io_async::Write;

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: {MOBILE_USER_AGENT}\r\n\r\n");
    Write::write_all(&mut tcp, request.as_bytes())
        .await
        .map_err(|_| "tcp write failed")?;

    Ok(tcp.read_to_end().await)
}

/// Splits a raw HTTP response into its header block and body, at the
/// blank line (`\r\n\r\n`) HTTP uses as the separator.
fn split_headers_body(response: &[u8]) -> (&str, &[u8]) {
    const SEP: &[u8] = b"\r\n\r\n";
    if let Some(pos) = response.windows(SEP.len()).position(|w| w == SEP) {
        let headers = core::str::from_utf8(&response[..pos]).unwrap_or("");
        (headers, &response[pos + SEP.len()..])
    } else {
        ("", response)
    }
}

fn is_chunked(headers: &str) -> bool {
    headers.lines().any(|line| {
        line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
    })
}

/// Decodes `Transfer-Encoding: chunked`: each chunk is a hex length, CRLF,
/// that many bytes of data, CRLF, repeating until a zero-length chunk.
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size_str = core::str::from_utf8(&data[..line_end]).unwrap_or("0");
        let size_str = size_str.split(';').next().unwrap_or("0").trim();
        let Ok(size) = usize::from_str_radix(size_str, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let chunk_start = line_end + 2;
        let chunk_end = (chunk_start + size).min(data.len());
        out.extend_from_slice(&data[chunk_start..chunk_end]);
        if chunk_end + 2 > data.len() {
            break;
        }
        data = &data[chunk_end + 2..]; // skip the chunk's trailing CRLF
    }
    out
}

const MARGIN: usize = 24;

/// A real, recognizable mobile-Chrome-on-Android `User-Agent` — sent on
/// every request (see `plain_get` and `net::tls::get`, which uses the
/// same string) so servers doing their own content negotiation serve us
/// their lighter mobile skin/markup automatically, the same page a real
/// phone would get. This matters a lot in practice: Wikipedia's desktop
/// HTML for a long article can be 500KB+ of deeply nested markup (many
/// scripts assume interactivity, dozens of inline `<style>` blocks,
/// heavy navboxes/infoboxes) that this kernel's `O(nodes × css rules)`
/// selector matching (see `layout.rs`) chews through far slower than the
/// mobile skin's much simpler, flatter markup — this isn't just about
/// looking closer to a phone's browser, it's what keeps a real page's
/// render time from being painfully slow (or effectively hung-looking)
/// on this kernel's current CSS engine.
pub const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Mobile Safari/537.36";

/// Clears only the top `viewport_h` pixels (never the taskbar strip below
/// it) before drawing a one-line status message — used while nothing else
/// is on screen yet (DNS resolving, connecting, etc.).
fn render_status(msg: &str, viewport_h: usize) {
    let mut guard = gfx::SCREEN.lock();
    if let Some(fb) = guard.as_mut() {
        let w = fb.width();
        fb.fill_rect(0, 0, w, viewport_h, gfx::BLACK);
        fb.draw_wrapped(msg, MARGIN, MARGIN, w - MARGIN, gfx::GRAY, gfx::BLACK, RasterHeight::Size16);
        // Every status screen used to skip drawing a cursor at all — a
        // real page's DNS/connect/parse/stylesheet fetch can easily take
        // tens of seconds over this kernel's own TCP/TLS stack, and for
        // that whole stretch the mouse pointer was simply invisible.
        // Not yet interactive here (nothing polls mouse events during
        // these network waits), but at least visible — centered, since
        // there's no tracked real position yet this early.
        fb.draw_cursor(w / 2, viewport_h / 2);
        fb.present();
    }
}

/// A `layout::LayoutItem` with any network fetch already done — an
/// `Image` here holds a decoded `Bitmap`, not a `src` string. Resolving
/// everything up front (`resolve_items`) means the actual draw pass
/// (`draw_at_scroll`) is pure and local: no `.await`, no network, so it
/// can re-run on every scroll-key press without re-fetching anything.
enum ResolvedItem {
    Text { text: String, style: layout::ComputedStyle, href: Option<String> },
    Image { bitmap: img::Bitmap, intended_width: Option<usize>, intended_height: Option<usize> },
    TableRow { cells: Vec<String>, style: layout::ComputedStyle },
}

/// What the scroll/click loop below decided once it stopped: either the
/// user clicked a link (navigate `fetch`'s outer loop to it), clicked the
/// desktop taskbar's Home button (exit the Browser app back to the
/// desktop), or the page itself gave up (DNS/fetch failure, or genuinely
/// no readable content) and there's nothing left to interact with.
enum PageAction {
    Navigate(String),
    Home,
    Done,
}

/// Parses `html` into a real DOM (`dom.rs`), resolves CSS (a built-in UA
/// default stylesheet, page `<style>` tags, and up to one externally
/// linked stylesheet — see `MAX_LINKED_STYLESHEETS` — merged via
/// `layout::build_rules`), fetches/decodes every image once
/// (`resolve_items`), then draws and re-draws the result within the top
/// `viewport_h` pixels of the screen as the user scrolls (arrow keys /
/// Page Up/Page Down — `crate::keyboard`'s extended-scancode support) via
/// `draw_at_scroll`, polling non-blockingly between keystrokes the same
/// way every other wait in this file does (`net_tick().await`, never a
/// spin loop). `home` is the desktop taskbar's Home button rect, checked
/// on every click alongside the page's own link regions.
async fn render_page(target: &Url<'_>, html_src: &str, viewport_h: usize, home: Rect) -> PageAction {
    let (screen_w, screen_h) = {
        let mut guard = gfx::SCREEN.lock();
        let Some(fb) = guard.as_mut() else { return PageAction::Done };
        (fb.width(), fb.height())
    };
    // Tracked from here (not just once the page is fully loaded) and
    // drawn on every status update below — a real page's DNS/connect/
    // parse/stylesheet/image fetches can take a long time (tens of
    // seconds isn't unusual for a real site over this kernel's own
    // TCP/TLS stack), and none of those status screens used to draw a
    // cursor at all, so the mouse pointer was simply invisible for that
    // entire stretch. Not yet interactive during loading (the fetch
    // loops below don't poll mouse events), but at least visible.
    let mut mouse_x: i32 = (screen_w / 2) as i32;
    let mut mouse_y: i32 = (viewport_h / 2) as i32;
    render_status("Parsing page...", viewport_h);
    let dom_root = crate::dom::parse(html_src);

    // A fast, plain first pass — UA defaults only, no page CSS, no
    // images — so *something readable* appears immediately instead of a
    // blank/status screen for however long full CSS parsing and
    // selector matching takes on a large real page (this kernel has no
    // network-level streaming parser yet; this is the pragmatic
    // equivalent of a real browser's "flash of unstyled content" moment,
    // not true incremental parsing). Overwritten by the fully-styled
    // pass below once it's ready.
    let no_css = layout::build_rules("");
    let quick_items = layout::flatten(&dom_root, &no_css);
    let quick_bg = layout::page_background(&dom_root, &no_css);
    let quick_x0 = MARGIN;
    let quick_w = screen_w - 2 * MARGIN;
    let quick_resolved: Vec<ResolvedItem> = quick_items
        .into_iter()
        .filter_map(|item| match item {
            layout::LayoutItem::Text { text, style, href } => Some(ResolvedItem::Text { text, style, href }),
            layout::LayoutItem::TableRow { cells, style } => Some(ResolvedItem::TableRow { cells, style }),
            layout::LayoutItem::Image { .. } => None,
        })
        .collect();
    draw_at_scroll(&quick_resolved, quick_bg, quick_x0, quick_w, viewport_h, 0);
    if let Some(fb) = gfx::SCREEN.lock().as_mut() {
        fb.draw_cursor(mouse_x as usize, mouse_y as usize);
        fb.present();
    }

    let mut css_text = String::new();
    layout::collect_inline_css(&dom_root, &mut css_text);

    const MAX_LINKED_STYLESHEETS: usize = 2;
    let mut fetched_stylesheets = 0usize;
    for href in linked_stylesheet_hrefs(&dom_root) {
        if fetched_stylesheets >= MAX_LINKED_STYLESHEETS {
            break;
        }
        let css_url = resolve_url(target, &href);
        serial_println!("http: fetching stylesheet {}", css_url);
        if let Some(bytes) = fetch_bytes_quiet(&css_url).await {
            css_text.push_str(&String::from_utf8_lossy(&bytes));
            css_text.push('\n');
            fetched_stylesheets += 1;
        }
    }

    let rules = layout::build_rules(&css_text);
    let items = layout::flatten(&dom_root, &rules);
    let bg = layout::page_background(&dom_root, &rules);

    // A centered, narrower reading column when the page's own CSS asks
    // for one (`width`/`max-width` on `<body>`/`<html>`, e.g.
    // `example.com`'s `width:60vw`) — falls back to the full viewport
    // (minus the outer gutter) otherwise.
    let content_w = layout::page_content_width(&dom_root, &rules, screen_w).unwrap_or(screen_w - 2 * MARGIN);
    let content_x0 = (screen_w.saturating_sub(content_w)) / 2;

    if items.is_empty() {
        let mut guard = gfx::SCREEN.lock();
        if let Some(fb) = guard.as_mut() {
            fb.fill_rect(0, 0, screen_w, viewport_h, bg);
            fb.draw_wrapped(
                "(this page has no readable text — it likely relies on \
                 JavaScript this kernel doesn't run)",
                content_x0,
                MARGIN,
                content_x0 + content_w,
                gfx::GRAY,
                bg,
                RasterHeight::Size16,
            );
            fb.present();
        }
        return PageAction::Done;
    }

    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        match item {
            layout::LayoutItem::Text { text, style, href } => resolved.push(ResolvedItem::Text { text, style, href }),
            layout::LayoutItem::TableRow { cells, style } => resolved.push(ResolvedItem::TableRow { cells, style }),
            layout::LayoutItem::Image { src, intended_width, intended_height } => {
                let image_url = resolve_url(target, &src);
                serial_println!("http: fetching image {}", image_url);
                match fetch_bytes_quiet(&image_url).await.and_then(|bytes| img::decode(&bytes)) {
                    Some(bitmap) => resolved.push(ResolvedItem::Image { bitmap, intended_width, intended_height }),
                    None => serial_println!("http: image {} failed or unsupported format", image_url),
                }
            }
        }
    }

    let mut scroll_y: usize = 0;
    let (content_height, mut click_regions) = draw_at_scroll(&resolved, bg, content_x0, content_w, viewport_h, 0);
    let max_scroll = content_height.saturating_sub(viewport_h);
    // Whether the mouse is currently dragging the scrollbar's thumb —
    // set on a LeftDown that lands in the scrollbar track, cleared on
    // LeftUp; while set, every subsequent Move (until LeftUp) adjusts
    // `scroll_y` proportionally instead of just moving the cursor.
    let mut dragging_scrollbar = false;

    // `mouse_x`/`mouse_y` (not in `mouse.rs`, which only reports relative
    // deltas — only this loop knows the viewport bounds to clamp
    // against) were already tracked from the very top of this function,
    // so no re-centering needed here — just snapshot the freshly-drawn
    // content (so `restore_content` below has something real to fall
    // back to) and draw the cursor on top of it.
    if let Some(fb) = gfx::SCREEN.lock().as_mut() {
        fb.save_content();
        fb.draw_cursor(mouse_x as usize, mouse_y as usize);
        fb.present();
    }

    // Scroll/click loop: redraws only on an actual key press, scroll
    // wheel tick, or scroll-changing event; a click checks
    // `click_regions` and returns the target URL for the caller
    // (`fetch`) to navigate to. Otherwise just yields to the executor —
    // same non-blocking-poll discipline as every network wait in this
    // file, just driven by the keyboard/mouse buffers instead of a
    // socket.
    loop {
        // Two separate dirty flags, not one: `content_dirty` means the
        // page itself changed (scrolled) and needs a full, comparatively
        // expensive `draw_at_scroll` re-render; `cursor_dirty` means only
        // the mouse position changed, which — now that there's a
        // `save_content`/`restore_content` snapshot to fall back to —
        // only needs a cheap buffer copy plus a cursor draw, not a full
        // page re-render. Before this split, EVERY mouse Move triggered a
        // full `draw_at_scroll` (re-wrapping and re-drawing every text
        // item, image, and table row currently on screen) just to shift
        // the cursor a few pixels — genuinely slow for a real page with
        // many items, which is what made the Browser specifically feel
        // sluggish even after event-coalescing already cut down how
        // *often* a redraw happens.
        let mut content_dirty = false;
        let mut cursor_dirty = false;
        match crate::keyboard::pop_key() {
            Some(crate::keyboard::KEY_DOWN) => {
                scroll_y = (scroll_y + LINE_SCROLL_STEP).min(max_scroll);
                content_dirty = true;
            }
            Some(crate::keyboard::KEY_UP) => {
                scroll_y = scroll_y.saturating_sub(LINE_SCROLL_STEP);
                content_dirty = true;
            }
            Some(crate::keyboard::KEY_PAGE_DOWN) => {
                scroll_y = (scroll_y + viewport_h).min(max_scroll);
                content_dirty = true;
            }
            Some(crate::keyboard::KEY_PAGE_UP) => {
                scroll_y = scroll_y.saturating_sub(viewport_h);
                content_dirty = true;
            }
            Some(_) | None => {}
        }

        // Drains every currently-queued mouse event before redrawing —
        // redrawing a whole page is comparatively expensive, and a real
        // PS/2 mouse queues many small-delta Move packets per screen
        // refresh; redrawing per-event throttled the whole pipeline down
        // to roughly one screen update per packet, which is what made
        // the cursor feel unresponsive under real, fast mouse motion.
        while let Some(event) = crate::mouse::poll_event() {
            match event {
                crate::mouse::MouseEvent::Move { dx, dy } => {
                    mouse_x = (mouse_x + dx).clamp(0, screen_w as i32 - 1);
                    mouse_y = (mouse_y + dy).clamp(0, screen_h as i32 - 1);
                    cursor_dirty = true;
                    if dragging_scrollbar && max_scroll > 0 {
                        let thumb_h = ((viewport_h * viewport_h) / content_height.max(1)).clamp(SCROLLBAR_MIN_THUMB, viewport_h);
                        let free = viewport_h.saturating_sub(thumb_h);
                        if free > 0 {
                            let delta_scroll = (dy as isize * max_scroll as isize) / free as isize;
                            scroll_y = (scroll_y as isize + delta_scroll).clamp(0, max_scroll as isize) as usize;
                            content_dirty = true;
                        }
                    }
                }
                crate::mouse::MouseEvent::ScrollDown => {
                    scroll_y = (scroll_y + LINE_SCROLL_STEP).min(max_scroll);
                    content_dirty = true;
                }
                crate::mouse::MouseEvent::ScrollUp => {
                    scroll_y = scroll_y.saturating_sub(LINE_SCROLL_STEP);
                    content_dirty = true;
                }
                crate::mouse::MouseEvent::LeftDown => {
                    let (mx, my) = (mouse_x as usize, mouse_y as usize);
                    if home.contains(mx, my) {
                        return PageAction::Home;
                    }
                    if max_scroll > 0 && scrollbar_track_rect(screen_w, viewport_h).contains(mx, my) {
                        // Jump-to-position: center the thumb on the click,
                        // then start dragging from there — a click
                        // anywhere in the track (not just precisely on the
                        // thumb) both jumps and starts a drag, the same
                        // behavior a real OS's scrollbar track has.
                        dragging_scrollbar = true;
                        let thumb_h = ((viewport_h * viewport_h) / content_height.max(1)).clamp(SCROLLBAR_MIN_THUMB, viewport_h);
                        let free = viewport_h.saturating_sub(thumb_h);
                        if free > 0 {
                            let thumb_y = my.saturating_sub(thumb_h / 2).min(free);
                            scroll_y = (thumb_y * max_scroll) / free;
                        }
                        content_dirty = true;
                        continue;
                    }
                    if let Some(region) = click_regions.iter().find(|r| mx >= r.x0 && mx < r.x1 && my >= r.y0 && my < r.y1) {
                        return PageAction::Navigate(resolve_url(target, &region.href));
                    }
                }
                crate::mouse::MouseEvent::LeftUp => {
                    dragging_scrollbar = false;
                }
            }
        }

        if content_dirty {
            let (_, regions) = draw_at_scroll(&resolved, bg, content_x0, content_w, viewport_h, scroll_y);
            click_regions = regions;
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

const LINE_SCROLL_STEP: usize = 32;

/// A link's on-screen bounding box for the most recent `draw_at_scroll`
/// call, in screen (not content/document) coordinates — rebuilt on every
/// redraw since scrolling moves everything. `render_page`'s click
/// handling checks a mouse-click position against these.
struct ClickRegion {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    href: String,
}

/// Draws `resolved` at vertical scroll offset `scroll_y` — pure and
/// local (no `.await`, no network), so it's cheap enough to call on
/// every scroll key press. Returns the total (unscrolled) content
/// height (which the caller uses once, on the initial call, to compute
/// `max_scroll`) plus every currently-visible link's on-screen bounding
/// box. Every item's position is computed unconditionally (`content_y`
/// always advances) regardless of whether it's actually visible, so
/// later items stay correctly positioned even while earlier ones are
/// scrolled off — only the actual pixel-drawing (and click-region
/// recording) is skipped for anything wholly outside
/// `[scroll_y, scroll_y + viewport_h)`.
fn draw_at_scroll(resolved: &[ResolvedItem], bg: gfx::Color, content_x0: usize, content_w: usize, viewport_h: usize, scroll_y: usize) -> (usize, Vec<ClickRegion>) {
    let mut guard = gfx::SCREEN.lock();
    let Some(fb) = guard.as_mut() else { return (0, Vec::new()) };
    // Only the app's own content area — never the taskbar strip below it
    // (`viewport_h` is always the screen height minus the taskbar, or the
    // full screen height for a taskbar-less caller, so this is safe either way).
    let width = fb.width();
    fb.fill_rect(0, 0, width, viewport_h, bg);

    let mut content_y: usize = MARGIN;
    let right_edge = content_x0 + content_w;
    let mut click_regions = Vec::new();

    for item in resolved {
        match item {
            ResolvedItem::Text { text, style, href } => {
                content_y += style.margin_top;
                let pad = style.padding;
                let mut x0 = (if style.align_center {
                    content_x0 + content_w.saturating_sub(text.len() * 8) / 2
                } else {
                    content_x0
                }) + pad;
                let weight = layout::font_weight(style);
                let text_height = gfx::measure_wrapped_height(text, x0, right_edge - pad, style.size, weight);
                let box_height = text_height + 2 * pad;

                // `position` support: `Relative` nudges the normal-flow
                // position by `top`/`left`; `Absolute`/`Fixed` draw at an
                // explicit pixel position (relative to the content
                // column's own top-left corner — this renderer has no
                // containing-block tracking, so that's the one "positioned
                // ancestor" every absolutely-positioned element gets) INSTEAD
                // of the normal-flow one, with `Fixed` additionally staying
                // put regardless of `scroll_y`, matching real `position:
                // fixed`. The item still consumes its normal-flow vertical
                // space either way (see `layout::ComputedStyle::position`'s
                // doc comment for why that's a real, documented gap, not an
                // oversight) — this only fixes WHERE it draws, not whether
                // later content reflows around its absence.
                let screen_y = match style.position {
                    layout::Position::Static => (content_y as isize) - (scroll_y as isize),
                    layout::Position::Relative => {
                        x0 = (x0 as isize + style.offset_left).max(0) as usize;
                        (content_y as isize) - (scroll_y as isize) + style.offset_top
                    }
                    layout::Position::Absolute => {
                        x0 = (content_x0 as isize + style.offset_left).max(0) as usize + pad;
                        style.offset_top - (scroll_y as isize)
                    }
                    layout::Position::Fixed => {
                        x0 = (content_x0 as isize + style.offset_left).max(0) as usize + pad;
                        style.offset_top
                    }
                };
                if screen_y + box_height as isize >= 0 && screen_y <= viewport_h as isize {
                    let draw_y = screen_y.max(0) as usize;
                    let para_bg = if let Some(box_color) = style.background {
                        fb.fill_rect(content_x0, draw_y, content_w, box_height, box_color);
                        box_color
                    } else {
                        bg
                    };
                    fb.draw_wrapped_styled(text, x0, draw_y + pad, right_edge - pad, style.color, para_bg, style.size, weight);
                    if let Some((border_color, thickness)) = style.border_bottom {
                        fb.fill_rect(content_x0, draw_y + box_height, content_w, thickness, border_color);
                    }
                    if let Some(link_href) = href {
                        click_regions.push(ClickRegion {
                            x0: content_x0,
                            y0: draw_y,
                            x1: right_edge,
                            y1: draw_y + box_height,
                            href: link_href.clone(),
                        });
                    }
                }
                content_y += box_height + 6 + style.margin_bottom;
                if let Some((_, thickness)) = style.border_bottom {
                    content_y += thickness + 4;
                }
            }
            ResolvedItem::Image { bitmap, intended_width, intended_height } => {
                if bitmap.width == 0 || bitmap.height == 0 {
                    continue;
                }
                // The page's own intended on-page size wins over
                // "fit to column" when it declares one — see
                // `layout::LayoutItem::Image`'s doc comment for why a
                // real thumbnail's *decoded* size is usually much
                // bigger than its *intended* display size. Still capped
                // to `content_w` so an unusually large declared width
                // (or a narrow viewport) can't overflow the page.
                let (target_w, target_h) = match (intended_width, intended_height) {
                    (Some(iw), Some(ih)) => {
                        let w = (*iw).min(content_w).max(1);
                        let h = (ih * w / iw.max(&1)).max(1);
                        (w, h)
                    }
                    (Some(iw), None) => {
                        let w = (*iw).min(content_w).max(1);
                        let h = (bitmap.height * w / bitmap.width).max(1);
                        (w, h)
                    }
                    (None, Some(ih)) => {
                        let w = (bitmap.width * ih / bitmap.height).min(content_w).max(1);
                        let h = (bitmap.height * w / bitmap.width).max(1);
                        (w, h)
                    }
                    (None, None) => match gfx::bitmap_draw_size(bitmap, content_w) {
                        Some(size) => size,
                        None => continue,
                    },
                };
                let screen_y = (content_y as isize) - (scroll_y as isize);
                if screen_y + target_h as isize >= 0 && screen_y <= viewport_h as isize {
                    fb.draw_bitmap_scaled(bitmap, content_x0, screen_y.max(0) as usize, target_w, target_h);
                }
                content_y += target_h + 6;
            }
            ResolvedItem::TableRow { cells, style } => {
                if cells.is_empty() {
                    continue;
                }
                const CELL_PAD: usize = 6;
                const BORDER: gfx::Color = gfx::Color(0x60, 0x60, 0x68);
                let weight = layout::font_weight(style);
                let col_w = content_w / cells.len();

                let row_height = cells
                    .iter()
                    .map(|cell| {
                        let x0 = content_x0 + CELL_PAD;
                        gfx::measure_wrapped_height(cell, x0, x0 + col_w.saturating_sub(2 * CELL_PAD), style.size, weight)
                    })
                    .max()
                    .unwrap_or(0)
                    + 2 * CELL_PAD;

                let screen_y = (content_y as isize) - (scroll_y as isize);
                if screen_y + row_height as isize >= 0 && screen_y <= viewport_h as isize {
                    let draw_y = screen_y.max(0) as usize;
                    for (i, cell) in cells.iter().enumerate() {
                        let col_x0 = content_x0 + i * col_w;
                        fb.draw_wrapped_styled(
                            cell,
                            col_x0 + CELL_PAD,
                            draw_y + CELL_PAD,
                            col_x0 + col_w.saturating_sub(CELL_PAD),
                            style.color,
                            bg,
                            style.size,
                            weight,
                        );
                        if i > 0 {
                            fb.fill_rect(col_x0, draw_y, 1, row_height, BORDER);
                        }
                    }
                    fb.fill_rect(content_x0, draw_y + row_height, content_w, 1, BORDER);
                }
                content_y += row_height + 1;
            }
        }
    }

    if content_y > viewport_h {
        let track = scrollbar_track_rect(width, viewport_h);
        fb.fill_rect(track.x0, track.y0, track.x1 - track.x0, track.y1 - track.y0, SCROLLBAR_TRACK);
        let max_scroll = content_y.saturating_sub(viewport_h);
        let thumb = scrollbar_thumb_rect(width, viewport_h, scroll_y, max_scroll, content_y);
        fb.fill_rect(thumb.x0, thumb.y0, thumb.x1 - thumb.x0, thumb.y1 - thumb.y0, SCROLLBAR_THUMB);
    }

    (content_y, click_regions)
}

const SCROLLBAR_WIDTH: usize = 10;
const SCROLLBAR_TRACK: gfx::Color = gfx::Color(0x30, 0x30, 0x36);
const SCROLLBAR_THUMB: gfx::Color = gfx::Color(0x80, 0x80, 0x8c);
/// Smallest a scrollbar thumb is ever drawn, regardless of how long the
/// page is relative to the viewport — otherwise a very long page would
/// shrink the thumb to a sliver too small to reliably click.
const SCROLLBAR_MIN_THUMB: usize = 24;

/// The scrollbar's full track — drawn behind the thumb by `draw_at_scroll`
/// and used by `render_page`'s click handling for "clicked outside the
/// thumb, but still in the track" hit-testing (jump-to-position) as well
/// as the thumb-drag hit test. Sits in the page's own right-hand margin
/// (`MARGIN` is 24px; this is 12px wide including its gaps), so it never
/// overlaps the actual page content column.
fn scrollbar_track_rect(screen_w: usize, viewport_h: usize) -> Rect {
    Rect { x0: screen_w - SCROLLBAR_WIDTH - 2, y0: 0, x1: screen_w - 2, y1: viewport_h }
}

/// The scrollbar's thumb — its height shrinks as the page gets longer
/// relative to the viewport (down to `SCROLLBAR_MIN_THUMB`), and its
/// position within the track tracks `scroll_y / max_scroll`, exactly
/// like a real OS's scrollbar.
fn scrollbar_thumb_rect(screen_w: usize, viewport_h: usize, scroll_y: usize, max_scroll: usize, content_height: usize) -> Rect {
    let track = scrollbar_track_rect(screen_w, viewport_h);
    let thumb_h = ((viewport_h * viewport_h) / content_height.max(1))
        .clamp(SCROLLBAR_MIN_THUMB, viewport_h);
    let free = viewport_h.saturating_sub(thumb_h);
    let thumb_y = if max_scroll == 0 { 0 } else { (scroll_y * free) / max_scroll };
    Rect { x0: track.x0, y0: thumb_y, x1: track.x1, y1: thumb_y + thumb_h }
}

/// Finds every `<link rel="stylesheet" href="...">` in document order —
/// real pages can reference several; `render_page` bounds how many it
/// actually fetches (`MAX_LINKED_STYLESHEETS`), same "don't let one page
/// load turn into downloading the whole site" reasoning `html.rs` used to
/// document for its own `MAX_IMAGES` cap.
fn linked_stylesheet_hrefs(node: &crate::dom::Node) -> Vec<String> {
    let mut out = Vec::new();
    collect_stylesheet_hrefs(node, &mut out);
    out
}

fn collect_stylesheet_hrefs(node: &crate::dom::Node, out: &mut Vec<String>) {
    if node.tag == "link" && node.attr("rel").is_some_and(|r| r.eq_ignore_ascii_case("stylesheet")) {
        if let Some(href) = node.attr("href") {
            out.push(href.to_string());
        }
    }
    for child in &node.children {
        collect_stylesheet_hrefs(child, out);
    }
}

async fn resolve(host: &str) -> Option<IpAddress> {
    let handle = stack::with_stack(|s| {
        let (sockets, iface) = (&mut s.sockets, &mut s.iface);
        let socket = sockets.get_mut::<smoltcp::socket::dns::Socket>(s.dns_handle);
        socket
            .start_query(iface.context(), host, DnsQueryType::A)
            .ok()
    })?;

    loop {
        let result = stack::with_stack(|s| {
            let socket = s
                .sockets
                .get_mut::<smoltcp::socket::dns::Socket>(s.dns_handle);
            match socket.get_query_result(handle) {
                Ok(addrs) => Some(Ok(addrs)),
                Err(GetQueryResultError::Pending) => None,
                Err(e) => Some(Err(e)),
            }
        });

        match result {
            None => net_tick().await,
            Some(Ok(addrs)) => return addrs.into_iter().next(),
            Some(Err(_)) => return None,
        }
    }
}
