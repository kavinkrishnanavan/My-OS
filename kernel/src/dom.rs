//! A pragmatic HTML parser that builds a real, nested DOM tree.
//!
//! This replaces `html.rs`'s role (which only produced a flat list of text
//! blocks) for the new rendering pipeline: a style-resolution/layout stage
//! needs actual parent/child structure to apply CSS and lay out boxes, not
//! just "here's some text and images in reading order."
//!
//! This is *not* a spec-compliant HTML5 parser. Deliberately out of scope:
//!   - No quirks-mode error recovery beyond "never crash, produce a
//!     best-effort tree." Malformed markup gets a reasonable-looking tree,
//!     not the exact tree a real browser's spec-mandated tree-construction
//!     algorithm would produce.
//!   - No foreign content handling (SVG/MathML namespacing, foreign
//!     attribute adjustment, etc). SVG/MathML tags are just parsed as
//!     regular elements.
//!   - No implied-tag insertion rules (e.g. `<table>` implicitly getting a
//!     `<tbody>`, `<li>` closing a previous open `<li>`, optional end tags
//!     like `</p>` before a following block element). Only explicit close
//!     tags close elements; mismatched/missing ones are tolerated (see
//!     below) but not "fixed up" to match what a browser would infer.
//!
//! What it does handle, because real server-rendered pages (this is aimed
//! at Wikipedia's mobile-html output) actually rely on it: proper nesting
//! via a tag stack, void elements, XML-style self-closing tags, quoted and
//! bare attributes, `<script>`/`<style>` raw-text content, comments,
//! doctypes, and entity decoding in text.

use alloc::{string::String, vec::Vec};

/// One DOM node: either an element (`tag` non-empty) or a text node
/// (`tag` empty, content in `text`).
pub struct Node {
    /// Lowercased tag name (e.g. "div", "p", "img"). Empty string ("")
    /// means this is a text node — its content lives in `text`, and
    /// `attrs`/`children` are always empty for a text node.
    pub tag: String,
    /// Every attribute this element had, in document order, with the
    /// name lowercased (HTML attribute names are case-insensitive) but
    /// the value preserved exactly as written (values ARE case-sensitive
    /// — e.g. a URL in `src`/`href`).
    pub attrs: Vec<(String, String)>,
    /// Non-empty only for a text node (`tag.is_empty()`). Entities
    /// already decoded, but whitespace NOT collapsed here — leave runs
    /// of whitespace as-is; that's the layout engine's job (real HTML
    /// renderers collapse whitespace at layout/inline-formatting time,
    /// not at parse time, since `white-space: pre` etc can change the
    /// rule per-element).
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    fn element(tag: String, attrs: Vec<(String, String)>) -> Self {
        Node {
            tag,
            attrs,
            text: String::new(),
            children: Vec::new(),
        }
    }

    fn text_node(text: String) -> Self {
        Node {
            tag: String::new(),
            attrs: Vec::new(),
            text,
            children: Vec::new(),
        }
    }

    /// Convenience: the first attribute value matching `name`
    /// (case-insensitive), or `None`.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Elements that never have a closing tag or children.
fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Parses `html` into a tree, returned as a single synthetic root node
/// (`tag == "root"`, no attrs, empty text) whose children are the
/// top-level parsed nodes (typically just `<html>`, but a fragment with
/// no `<html>` wrapper works fine too — it just ends up as a handful of
/// direct children of `root`).
pub fn parse(html: &str) -> Node {
    // The open-element stack. Index 0 is always the synthetic root and is
    // never popped. Text/elements are always attached under `stack.last()`.
    let mut stack: Vec<Node> = alloc::vec![Node::element(String::from("root"), Vec::new())];
    let len = html.len();
    let mut pos = 0usize;

    while pos < len {
        match html[pos..].find('<') {
            Some(rel) => {
                if rel > 0 {
                    push_text(&mut stack, &html[pos..pos + rel]);
                    pos += rel;
                }
                // html[pos] is now the '<' we found (or we already were at one).
                if html[pos..].starts_with("<!--") {
                    pos = match html[pos + 4..].find("-->") {
                        Some(end_rel) => pos + 4 + end_rel + 3,
                        None => len,
                    };
                } else if html[pos..].starts_with("<!") {
                    pos = match html[pos..].find('>') {
                        Some(end_rel) => pos + end_rel + 1,
                        None => len,
                    };
                } else if html[pos..].starts_with("</") {
                    let (name, new_pos) = read_close_tag_name(html, pos + 2);
                    pos = new_pos;
                    close_matching(&mut stack, &name);
                } else {
                    // Regular '<' — could still be a stray/malformed '<'
                    // with no sane tag name; parse_open_tag degrades
                    // gracefully either way.
                    let (tag_name, attrs, self_close, new_pos) = parse_open_tag(html, pos + 1);
                    pos = new_pos;
                    if tag_name.is_empty() {
                        continue;
                    }

                    if tag_name == "script" || tag_name == "style" {
                        let mut node = Node::element(tag_name.clone(), attrs);
                        match find_closing_tag(&html[pos..], &tag_name) {
                            Some(rel) => {
                                let raw = &html[pos..pos + rel];
                                if !raw.is_empty() {
                                    node.children.push(Node::text_node(String::from(raw)));
                                }
                                let after = pos + rel;
                                pos = match html[after..].find('>') {
                                    Some(gt_rel) => after + gt_rel + 1,
                                    None => len,
                                };
                            }
                            None => {
                                let raw = &html[pos..];
                                if !raw.is_empty() {
                                    node.children.push(Node::text_node(String::from(raw)));
                                }
                                pos = len;
                            }
                        }
                        stack.last_mut().unwrap().children.push(node);
                    } else if self_close || is_void_element(&tag_name) {
                        let node = Node::element(tag_name, attrs);
                        stack.last_mut().unwrap().children.push(node);
                    } else {
                        stack.push(Node::element(tag_name, attrs));
                    }
                }
            }
            None => {
                push_text(&mut stack, &html[pos..]);
                pos = len;
            }
        }
    }

    // Anything still open at EOF (unclosed tags) gets folded into its
    // parent in order, innermost first.
    while stack.len() > 1 {
        let node = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(node);
    }
    stack.pop().unwrap_or_else(|| Node::element(String::from("root"), Vec::new()))
}

/// Decodes `raw` and, if the result is non-empty, appends it as a text
/// child of the current top-of-stack element.
fn push_text(stack: &mut Vec<Node>, raw: &str) {
    if raw.is_empty() {
        return;
    }
    let decoded = decode_entities(raw);
    if decoded.is_empty() {
        return;
    }
    // `stack` always has at least the root, so this is safe.
    if let Some(top) = stack.last_mut() {
        top.children.push(Node::text_node(decoded));
    }
}

/// Pops elements off `stack` to close the nearest open element named
/// `name` (case handled by caller — `name` is already lowercased). If no
/// matching open element exists (anywhere but the synthetic root), the
/// close tag is ignored — tolerates a stray/mismatched close tag without
/// corrupting the tree.
fn close_matching(stack: &mut Vec<Node>, name: &str) {
    if name.is_empty() {
        return;
    }
    let mut target = None;
    // Search from the top down, but never match/pop index 0 (the root).
    let mut i = stack.len();
    while i > 1 {
        i -= 1;
        if stack[i].tag == name {
            target = Some(i);
            break;
        }
    }
    if let Some(target) = target {
        while stack.len() > target {
            let node = stack.pop().unwrap();
            if let Some(parent) = stack.last_mut() {
                parent.children.push(node);
            }
        }
    }
    // else: no matching open element anywhere on the stack — ignore.
}

/// Reads a closing tag's name starting right after `</`, then skips past
/// its `>` (tolerating stray attributes/whitespace in a malformed close
/// tag like `</div class="x">`). Returns (lowercased name, position after
/// the `>`, or end-of-string if none was found).
fn read_close_tag_name(html: &str, pos: usize) -> (String, usize) {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let start = pos.min(len);
    let mut i = start;
    while i < len {
        let c = bytes[i];
        if c == b'>' || c == b'/' || c.is_ascii_whitespace() {
            break;
        }
        i += 1;
    }
    let name = html.get(start..i).unwrap_or("").to_ascii_lowercase();
    let after = match html[i.min(len)..].find('>') {
        Some(rel) => i + rel + 1,
        None => len,
    };
    (name, after)
}

/// Scans an open tag's contents starting right after `<`, tracking quote
/// state so a `>` inside a quoted attribute value doesn't end the tag
/// early. Returns (lowercased tag name, attributes, self-closing flag,
/// position right after the tag's `>`).
fn parse_open_tag(html: &str, pos: usize) -> (String, Vec<(String, String)>, bool, usize) {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let start = pos.min(len);
    let mut i = start;
    let mut in_squote = false;
    let mut in_dquote = false;
    while i < len {
        let c = bytes[i];
        if in_squote {
            if c == b'\'' {
                in_squote = false;
            }
        } else if in_dquote {
            if c == b'"' {
                in_dquote = false;
            }
        } else {
            match c {
                b'"' => in_dquote = true,
                b'\'' => in_squote = true,
                b'>' => break,
                _ => {}
            }
        }
        i += 1;
    }
    let inner_end = i.min(len);
    let after = if inner_end < len { inner_end + 1 } else { len };
    let raw = html.get(start..inner_end).unwrap_or("");

    let trimmed_end = raw.trim_end();
    let self_close = trimmed_end.ends_with('/');
    let content = if self_close {
        &trimmed_end[..trimmed_end.len() - 1]
    } else {
        raw
    };
    let content = content.trim_start();

    let name_end = content
        .find(|c: char| c.is_whitespace())
        .unwrap_or(content.len());
    let tag_name = content[..name_end].to_ascii_lowercase();
    let attr_str = content.get(name_end..).unwrap_or("");
    let attrs = parse_attrs(attr_str);

    (tag_name, attrs, self_close, after)
}

/// Parses `name="value"` / `name='value'` / bare `name` attributes,
/// whitespace-tolerant, never panicking on malformed input (stray `=`,
/// unterminated quotes, etc).
fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    loop {
        while i < len && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= len {
            break;
        }
        let name_start = i;
        while i < len && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'/' {
            i += 1;
        }
        if i == name_start {
            // Stray delimiter (e.g. a lone '='); skip it to avoid looping.
            i += 1;
            continue;
        }
        let name = s
            .get(name_start..i)
            .unwrap_or("")
            .to_ascii_lowercase();

        let mut j = i;
        while j < len && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < len && bytes[j] == b'=' {
            j += 1;
            while j < len && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < len && (bytes[j] == b'"' || bytes[j] == b'\'') {
                let quote = bytes[j];
                j += 1;
                let val_start = j;
                while j < len && bytes[j] != quote {
                    j += 1;
                }
                let value = decode_entities(s.get(val_start..j).unwrap_or(""));
                attrs.push((name, value));
                i = if j < len { j + 1 } else { j };
            } else {
                let val_start = j;
                while j < len && !bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let value = decode_entities(s.get(val_start..j).unwrap_or(""));
                attrs.push((name, value));
                i = j;
            }
        } else {
            attrs.push((name, String::new()));
            i = j;
        }
    }

    attrs
}

/// Case-insensitive search for a closing tag `</name` inside `haystack`,
/// requiring the name to be immediately followed by whitespace, `/`, `>`,
/// or end-of-string (so `</scripture` doesn't falsely match `</script`).
/// Returns the byte offset of the `<`.
fn find_closing_tag(haystack: &str, tag_lower: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let pat_len = 2 + tag_lower.len();
    if h.len() < pat_len {
        return None;
    }
    let mut i = 0usize;
    while i + pat_len <= h.len() {
        if h[i] == b'<' && h[i + 1] == b'/' {
            if let Some(seg) = haystack.get(i + 2..i + 2 + tag_lower.len()) {
                if seg.eq_ignore_ascii_case(tag_lower) {
                    let after = i + pat_len;
                    let ok = after >= h.len()
                        || h[after] == b'>'
                        || h[after] == b'/'
                        || h[after].is_ascii_whitespace();
                    if ok {
                        return Some(i);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Decodes HTML entities in text content. Mirrors `html.rs`'s entity
/// table (same named entities it handles) plus numeric `&#NNN;` /
/// `&#xHH;` support, which `html.rs` didn't need but a real DOM parser
/// should have.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut entity = String::new();
        let mut consumed = false;
        while let Some(&next) = chars.peek() {
            if next == ';' {
                chars.next();
                consumed = true;
                break;
            }
            if next.is_whitespace() || next == '&' || entity.len() > 12 {
                break;
            }
            entity.push(next);
            chars.next();
        }
        if !consumed {
            out.push('&');
            out.push_str(&entity);
            continue;
        }
        match decode_one_entity(&entity) {
            Some(rep) => out.push_str(&rep),
            None => {
                out.push('&');
                out.push_str(&entity);
                out.push(';');
            }
        }
    }
    out
}

/// Resolves one entity name/number (without the surrounding `&`/`;`) to
/// its replacement text, or `None` if unrecognized.
fn decode_one_entity(entity: &str) -> Option<String> {
    let named = match entity {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "quot" => Some("\""),
        "apos" | "#39" => Some("'"),
        "nbsp" => Some(" "),
        "mdash" | "#8212" => Some("\u{2014}"),
        "ndash" | "#8211" => Some("\u{2013}"),
        "rsquo" | "#8217" => Some("'"),
        "lsquo" | "#8216" => Some("'"),
        "ldquo" | "#8220" => Some("\""),
        "rdquo" | "#8221" => Some("\""),
        _ => None,
    };
    if let Some(rep) = named {
        return Some(String::from(rep));
    }

    if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
        if let Ok(code) = u32::from_str_radix(hex, 16) {
            if let Some(ch) = char::from_u32(code) {
                return Some(String::from(ch));
            }
        }
        return None;
    }

    if let Some(dec) = entity.strip_prefix('#') {
        if let Ok(code) = dec.parse::<u32>() {
            if let Some(ch) = char::from_u32(code) {
                return Some(String::from(ch));
            }
        }
        return None;
    }

    None
}
