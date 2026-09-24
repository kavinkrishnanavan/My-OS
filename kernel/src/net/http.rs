//! The actual "access a website" part: parse a URL, resolve the host
//! over DNS, open a TCP connection (through TLS first if it's
//! `https://`), speak just enough HTTP/1.1 to GET a page, and draw the
//! result onto the framebuffer as an actual page (see `render` below) as
//! well as logging it over serial.
//!
//! Every wait in here is `net::stack::net_tick().await` — hand control
//! back to the executor, get resumed when the NIC IRQ or PIT tick says
//! "check again", never a spin loop.

use crate::gfx::{self, Color};
use crate::html;
use crate::img;
use crate::net::stack::{self, net_tick};
use crate::net::tcp_stream::TcpStream;
use crate::net::tls;
use crate::serial_println;
use alloc::format;
use alloc::string::String;
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

/// Spawned as its own task from `main`. Fetches `url`, draws it to the
/// screen, and logs progress over serial. Runs once; a real OS would
/// expose this as a syscall/API a shell or browser chrome could call for
/// any URL, but the point here is proving the whole stack — driver,
/// DHCP, DNS, TCP, TLS, HTTP, and on-screen rendering — works end to end.
pub async fn fetch(url: &'static str) {
    serial_println!("http: waiting for DHCP lease...");
    while !stack::has_ip() {
        net_tick().await;
    }

    let mut current = String::from(url);
    for redirect in 0..=MAX_REDIRECTS {
        let target = parse_url(&current);

        render_status(&format!("Resolving {}...", target.host));
        serial_println!("http: resolving {}", target.host);
        let ip = match resolve(target.host).await {
            Some(ip) => ip,
            None => {
                render_status(&format!("Could not resolve {}", target.host));
                serial_println!("http: DNS resolution for {} failed", target.host);
                return;
            }
        };
        serial_println!("http: {} -> {}", target.host, ip);

        render_status(&format!(
            "Connecting to {} ({ip}) over {}...",
            target.host,
            if target.https { "TLS" } else { "plain HTTP" }
        ));
        let response = match get(ip, &target).await {
            Ok(body) => body,
            Err(e) => {
                render_status(&format!("Request to {} failed: {e}", target.host));
                serial_println!("http: request failed: {}", e);
                return;
            }
        };
        serial_println!("http: received {} bytes", response.len());

        let (headers, body) = split_headers_body(&response);

        if let Some(location) = redirect_location(headers) {
            if redirect == MAX_REDIRECTS {
                render_status("Too many redirects");
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

        serial_println!("---- response ----");
        serial_println!("{}", String::from_utf8_lossy(&response));
        serial_println!("-------------------");

        let text = String::from_utf8_lossy(&body);
        render_page(&target, &text).await;
        return;
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

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: myos/0.1\r\n\r\n"
    );
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

fn render_status(msg: &str) {
    let mut guard = gfx::SCREEN.lock();
    if let Some(fb) = guard.as_mut() {
        let w = fb.width();
        fb.clear(gfx::BLACK);
        fb.draw_wrapped(msg, MARGIN, MARGIN, w - MARGIN, gfx::GRAY, gfx::BLACK, RasterHeight::Size16);
    }
}

/// Extracts visible text and images from `html` (see `html::extract`) and
/// flows them onto the framebuffer in reading order: a title bar showing
/// the host, then each block — headings in a larger bold-ish size, images
/// fetched (against `target`, in case of a relative `src`), decoded, and
/// blitted in place. Fetching each image is its own network round trip,
/// so this holds the framebuffer lock only while actually drawing, never
/// across an `.await`.
async fn render_page(target: &Url<'_>, html_src: &str) {
    let blocks = html::extract(html_src);

    let (w, h, mut y) = {
        let mut guard = gfx::SCREEN.lock();
        let Some(fb) = guard.as_mut() else { return };
        fb.clear(gfx::BLACK);
        let w = fb.width();
        let h = fb.height();
        let y = fb.draw_wrapped(target.host, MARGIN, MARGIN, w - MARGIN, gfx::BLUE, gfx::BLACK, RasterHeight::Size16) + 12;
        (w, h, y)
    };

    if blocks.is_empty() {
        let mut guard = gfx::SCREEN.lock();
        if let Some(fb) = guard.as_mut() {
            fb.draw_wrapped(
                "(this page has no readable text — it likely relies on \
                 JavaScript or CSS this kernel doesn't run)",
                MARGIN,
                y,
                w - MARGIN,
                gfx::GRAY,
                gfx::BLACK,
                RasterHeight::Size16,
            );
        }
        return;
    }

    for block in blocks {
        if y > h {
            break; // no scrolling yet — see README for what's next
        }
        match block {
            html::Block::Text { heading, text } => {
                let (color, size) = if heading {
                    (gfx::WHITE, RasterHeight::Size24)
                } else {
                    (Color(0xd0, 0xd0, 0xd0), RasterHeight::Size16)
                };
                let mut guard = gfx::SCREEN.lock();
                if let Some(fb) = guard.as_mut() {
                    y = fb.draw_wrapped(&text, MARGIN, y, w - MARGIN, color, gfx::BLACK, size) + 6;
                }
            }
            html::Block::Image { src } => {
                let image_url = resolve_url(target, &src);
                serial_println!("http: fetching image {}", image_url);
                let bitmap = fetch_bytes_quiet(&image_url).await.and_then(|bytes| img::decode(&bytes));
                match bitmap {
                    Some(bitmap) => {
                        let mut guard = gfx::SCREEN.lock();
                        if let Some(fb) = guard.as_mut() {
                            y = fb.draw_bitmap(&bitmap, MARGIN, y, w - 2 * MARGIN) + 6;
                        }
                    }
                    None => serial_println!("http: image {} failed or unsupported format", image_url),
                }
            }
        }
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
