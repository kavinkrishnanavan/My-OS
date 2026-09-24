//! A minimal HTML-to-text extractor.
//!
//! This is nowhere near a real HTML parser — no DOM, no CSS, no nesting
//! rules. It walks the byte stream once, tracks just enough tag context
//! to know when to insert line breaks (block-level elements, `<br>`) and
//! decodes the handful of entities real pages actually use. That's
//! enough to turn a page like example.com's markup into readable,
//! wrapped text on the framebuffer — the goal is "show the page's
//! content," not "lay it out pixel-for-pixel like a browser would."

use alloc::string::String;

pub enum Block {
    Text { heading: bool, text: String },
    /// An `<img src="...">`, in reading-order position among the text
    /// blocks. `src` is exactly what the page wrote — could be relative,
    /// protocol-relative, or absolute; resolving it against the page's
    /// own URL is the caller's job (see `http::resolve_redirect`-style
    /// logic in `http.rs`), not this parser's.
    Image { src: String },
}

/// Cap on how many `<img>` tags we collect from one page — real pages
/// can have hundreds (icons, tracking pixels, srcset variants), and each
/// one is a full network fetch + decode. This keeps a page load from
/// turning into downloading the entire internet.
const MAX_IMAGES: usize = 12;

pub fn extract(html: &str) -> alloc::vec::Vec<Block> {
    let mut blocks = alloc::vec::Vec::new();
    let mut current = String::new();
    let mut in_tag = false;
    let mut in_heading = false;
    let mut skip_content = false; // inside <script> / <style> / <head>
    let mut tag_name = String::new();
    let mut image_count = 0usize;

    let mut chars = html.chars().peekable();
    let flush = |blocks: &mut alloc::vec::Vec<Block>, current: &mut String, heading: bool| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            blocks.push(Block::Text {
                heading,
                text: String::from(trimmed),
            });
        }
        current.clear();
    };

    while let Some(c) = chars.next() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                let closing = tag_name.starts_with('/');
                let tag = tag_name
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();

                match tag.as_str() {
                    "script" | "style" | "head" | "title" => skip_content = !closing,
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        if closing {
                            flush(&mut blocks, &mut current, true);
                            in_heading = false;
                        } else {
                            flush(&mut blocks, &mut current, in_heading);
                            in_heading = true;
                        }
                    }
                    "p" | "div" | "br" | "li" | "tr" | "section" | "article" | "header"
                    | "footer" | "ul" | "ol" | "table" => {
                        flush(&mut blocks, &mut current, in_heading);
                    }
                    "img" if !closing && image_count < MAX_IMAGES => {
                        // Prefer `data-src`: a very common lazy-loading
                        // convention where the real URL is parked there
                        // and `src` holds a tiny placeholder (or nothing)
                        // until client-side JS swaps it in on scroll.
                        let attr = extract_attr(&tag_name, "data-src")
                            .or_else(|| extract_attr(&tag_name, "src"));
                        if let Some(src) = attr {
                            flush(&mut blocks, &mut current, in_heading);
                            blocks.push(Block::Image { src: String::from(src) });
                            image_count += 1;
                        }
                    }
                    _ => {}
                }
                tag_name.clear();
            } else {
                tag_name.push(c);
            }
            continue;
        }

        if c == '<' {
            in_tag = true;
            continue;
        }

        if skip_content {
            continue;
        }

        if c == '&' {
            let mut entity = String::new();
            let mut consumed = false;
            while let Some(&next) = chars.peek() {
                if next == ';' {
                    chars.next();
                    consumed = true;
                    break;
                }
                if next.is_whitespace() || entity.len() > 8 {
                    break;
                }
                entity.push(next);
                chars.next();
            }
            if consumed {
                current.push_str(match entity.as_str() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" | "#39" => "'",
                    "nbsp" => " ",
                    "mdash" | "#8212" => "\u{2014}",
                    "ndash" | "#8211" => "\u{2013}",
                    "rsquo" | "#8217" => "'",
                    "lsquo" | "#8216" => "'",
                    "ldquo" | "#8220" => "\"",
                    "rdquo" | "#8221" => "\"",
                    _ => " ",
                });
            } else {
                current.push('&');
                current.push_str(&entity);
            }
            continue;
        }

        current.push(c);
    }
    flush(&mut blocks, &mut current, in_heading);
    blocks
}

/// Finds `name="value"` (or `name='value'`) inside a raw tag's contents
/// (everything between `<` and `>`, attributes and all), case-insensitive
/// on the attribute name. No unquoted-value support — vanishingly rare
/// in real markup and not worth the ambiguity.
fn extract_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut search_from = 0;
    loop {
        let rel = lower[search_from..].find(name)?;
        let idx = search_from + rel;
        let after = idx + name.len();
        if lower.as_bytes().get(after) == Some(&b'=') {
            let rest = &tag[after + 1..];
            let quote = *rest.as_bytes().first()?;
            if quote == b'"' || quote == b'\'' {
                let end = rest[1..].find(quote as char)?;
                return Some(&rest[1..1 + end]);
            }
        }
        search_from = idx + 1;
        if search_from >= lower.len() {
            return None;
        }
    }
}
