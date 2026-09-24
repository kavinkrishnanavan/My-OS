//! A small, deliberately incomplete CSS parser: turns CSS source text into
//! a flat list of [`Rule`]s (selectors + property/value declarations),
//! with no notion of what any of it *means* — no unit parsing, no color
//! parsing, no cascade/specificity resolution, no selector matching. This
//! is purely CSS *syntax* in, structured data out; a later style-resolution
//! layer is expected to walk a DOM tree, match selectors against it using
//! these `Rule`s, and interpret the raw `Declaration::value` strings. This
//! module has zero dependency on any DOM representation.
//!
//! What this deliberately does NOT support (real Wikipedia CSS uses some
//! of these; we skip/degrade rather than implement them):
//! - Attribute selectors (`[href]`, `[lang=en]`).
//! - Pseudo-classes/pseudo-elements (`:hover`, `::before`), other than
//!   best-effort tag/id/class extraction from a selector that contains one.
//! - Combinators other than descendant: `>`, `+`, `~` are treated as
//!   equivalent to whitespace (descendant), not given real child/sibling
//!   semantics.
//! - `@media` (and other at-rule) *condition* evaluation — a block-form
//!   at-rule has its condition ignored and its inner rules are parsed and
//!   applied unconditionally, on the theory that over-applying styles gets
//!   us closer to "looks right" than dropping them entirely.
//! - CSS custom properties / `var()` substitution.
//! - `calc()` (or any other arithmetic) evaluation.
//!
//! Above all: malformed or unsupported input must never crash the parser.
//! Real-world CSS fetched off the internet will contain constructs we
//! don't handle; the contract here is "skip the offending rule/declaration
//! and keep going," never a panic or an infinite loop. Brace/paren
//! matching is done with explicit depth counters and forward-only cursor
//! advancement so malformed nesting can't hang the parser.

use alloc::string::String;
use alloc::vec::Vec;

/// One `property: value` pair inside a declaration block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Lowercased property name, e.g. "color", "background-color".
    pub property: String,
    /// Raw value text, trimmed, exactly as written. Units/colors/etc are
    /// intentionally left unparsed here.
    pub value: String,
}

/// One compound selector: the part between descendant combinators, e.g.
/// the `div.infobox` in `div.infobox span.fn`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompoundSelector {
    /// `None` means "any tag" (e.g. `.foo` or `#bar` alone).
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
}

impl CompoundSelector {
    fn is_empty(&self) -> bool {
        self.tag.is_none() && self.id.is_none() && self.classes.is_empty()
    }
}

/// A full selector: a chain of [`CompoundSelector`]s in descendant order.
/// The last element must match the target element; earlier elements must
/// each match some ancestor, in order. Matching logic lives elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector(pub Vec<CompoundSelector>);

/// One parsed CSS rule: possibly several comma-separated selectors
/// sharing one declaration block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
}

/// Strips `/* ... */` comments anywhere in `css` (including across
/// newlines, including inside selectors). An unterminated comment at EOF
/// just eats the rest of the input rather than looping or panicking.
fn strip_comments(css: &str) -> String {
    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len());
    let mut i = 0usize;
    let len = bytes.len();
    while i < len {
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            // Scan forward for the closing `*/`; if there isn't one, the
            // rest of the file is inside the comment.
            let mut j = i + 2;
            let mut closed = false;
            while j + 1 < len {
                if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                    closed = true;
                    break;
                }
                j += 1;
            }
            if closed {
                i = j + 2;
            } else {
                i = len;
            }
        } else {
            // Safe to push byte-by-byte only because we only branch on
            // ASCII '/' and '*'; multi-byte UTF-8 sequences never contain
            // those bytes as continuation bytes, so this can't split a
            // codepoint. Push the char at this position properly instead
            // of assuming ASCII, to avoid corrupting non-ASCII content.
            let ch = css[i..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Parses a full stylesheet into a flat list of rules. Never panics;
/// unparseable rules/at-rules are skipped individually.
pub fn parse(css: &str) -> Vec<Rule> {
    let cleaned = strip_comments(css);
    let mut rules = Vec::new();
    parse_block(&cleaned, &mut rules);
    rules
}

/// Parses a sequence of rules/at-rules from `src` (which may be the whole
/// stylesheet, or the inside of an at-rule block like `@media {...}`),
/// appending results into `rules`. Always makes forward progress: every
/// branch advances past at least the character(s) it just inspected.
fn parse_block(src: &str, rules: &mut Vec<Rule>) {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        // Skip whitespace.
        while i < len && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }

        if bytes[i] == b'@' {
            // At-rule: find whichever comes first, a top-level `{` (block
            // form) or a `;` (statement form, e.g. `@import ...;`).
            let mut j = i + 1;
            let mut brace_pos = None;
            let mut semi_pos = None;
            while j < len {
                match bytes[j] {
                    b'{' => {
                        brace_pos = Some(j);
                        break;
                    }
                    b';' => {
                        semi_pos = Some(j);
                        break;
                    }
                    _ => j += 1,
                }
            }
            if let Some(brace_idx) = brace_pos {
                // Block-form at-rule: find its matching closing brace by
                // depth-counting (handles nested `{}` inside, e.g. rules
                // inside `@media`), then recurse into the interior,
                // ignoring the at-rule's own condition entirely.
                if let Some(end_idx) = find_matching_brace(bytes, brace_idx) {
                    let inner = &src[brace_idx + 1..end_idx];
                    parse_block(inner, rules);
                    i = end_idx + 1;
                } else {
                    // No matching close brace anywhere: malformed input,
                    // bail out of the rest of this block rather than loop.
                    i = len;
                }
            } else if let Some(semi_idx) = semi_pos {
                i = semi_idx + 1;
            } else {
                // Neither `{` nor `;` found before EOF: nothing more to
                // parse in this block.
                i = len;
            }
            continue;
        }

        // Otherwise: an ordinary rule. Find its opening brace.
        let mut j = i;
        while j < len && bytes[j] != b'{' {
            j += 1;
        }
        if j >= len {
            // No `{` found: trailing garbage (e.g. a truncated selector
            // at EOF). Nothing more to parse.
            break;
        }
        let selector_text = &src[i..j];

        let end_idx = match find_matching_brace(bytes, j) {
            Some(e) => e,
            None => {
                // Unterminated block: skip rest of input rather than loop.
                break;
            }
        };
        let body_text = &src[j + 1..end_idx];

        let selectors = parse_selector_list(selector_text);
        let declarations = parse_declarations(body_text);
        if !selectors.is_empty() && !declarations.is_empty() {
            rules.push(Rule {
                selectors,
                declarations,
            });
        }
        i = end_idx + 1;
    }
}

/// Given the byte index of an opening `{`, finds the index of its
/// matching `}` by depth-counting. Returns `None` if unbalanced (no
/// matching close before EOF), which the caller treats as "malformed,
/// stop parsing this block" rather than looping forever.
fn find_matching_brace(bytes: &[u8], open_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut k = open_idx;
    let len = bytes.len();
    while k < len {
        match bytes[k] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
}

/// Splits a selector-list string (the part before `{`) on top-level
/// commas (not nested inside `()`/`[]`) and parses each piece as a
/// [`Selector`]. Empty/unparseable pieces are dropped.
fn parse_selector_list(text: &str) -> Vec<Selector> {
    let mut selectors = Vec::new();
    for piece in split_top_level(text, ',') {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(sel) = parse_selector(trimmed) {
            selectors.push(sel);
        }
    }
    selectors
}

/// Splits `text` on occurrences of `sep` that are not nested inside `(`/`)`
/// or `[`/`]`, returning the resulting substrings (unmodified, not trimmed).
fn split_top_level(text: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut last_idx = 0usize;
    for (idx, ch) in text.char_indices() {
        last_idx = idx + ch.len_utf8();
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            c if c == sep && depth == 0 => {
                parts.push(&text[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..last_idx.max(start)]);
    parts
}

/// Parses one selector (no top-level commas) into a chain of
/// [`CompoundSelector`]s, splitting on whitespace and normalizing `>`,
/// `+`, `~` combinators to plain descendant (i.e. just more whitespace).
/// Returns `None` only if nothing usable could be extracted at all.
fn parse_selector(text: &str) -> Option<Selector> {
    // Replace explicit combinators with spaces so whitespace-splitting
    // below treats them the same as the descendant combinator.
    let mut normalized = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '>' | '+' | '~' => normalized.push(' '),
            _ => normalized.push(ch),
        }
    }

    let mut compounds = Vec::new();
    for token in normalized.split_whitespace() {
        if let Some(cs) = parse_compound_selector(token) {
            if !cs.is_empty() {
                compounds.push(cs);
            }
        }
    }

    if compounds.is_empty() {
        None
    } else {
        Some(Selector(compounds))
    }
}

/// Parses one compound selector token, e.g. `div.infobox.hidden#foo`,
/// `.foo`, `#bar`, `*`, or something with unsupported syntax like
/// `a[href]:hover` (from which we extract `tag: "a"` and ignore the
/// rest). Best-effort: never fails outright, just extracts whatever
/// tag/id/class fragments it can recognize.
fn parse_compound_selector(token: &str) -> Option<CompoundSelector> {
    let mut cs = CompoundSelector::default();
    let bytes = token.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    // Optional leading tag name: runs of identifier-ish chars up to the
    // first `.`, `#`, `[`, `:`, or `*`.
    let tag_start = i;
    while i < len {
        let b = bytes[i];
        if b == b'.' || b == b'#' || b == b'[' || b == b':' {
            break;
        }
        i += 1;
    }
    if i > tag_start {
        let tag_str = &token[tag_start..i];
        if tag_str != "*" && !tag_str.is_empty() {
            cs.tag = Some(String::from(tag_str).to_ascii_lowercase());
        }
    }

    // Walk the rest, picking out `.class` and `#id` fragments and
    // skipping anything else (attribute selectors, pseudo-classes) by
    // jumping to the next recognized fragment start.
    while i < len {
        match bytes[i] {
            b'.' => {
                let start = i + 1;
                let mut j = start;
                while j < len && is_ident_byte(bytes[j]) {
                    j += 1;
                }
                if j > start {
                    cs.classes.push(String::from(&token[start..j]));
                }
                i = if j > i { j } else { i + 1 };
            }
            b'#' => {
                let start = i + 1;
                let mut j = start;
                while j < len && is_ident_byte(bytes[j]) {
                    j += 1;
                }
                if j > start {
                    cs.id = Some(String::from(&token[start..j]));
                }
                i = if j > i { j } else { i + 1 };
            }
            b'[' => {
                // Attribute selector: skip to matching `]` (or EOF).
                let mut j = i + 1;
                while j < len && bytes[j] != b']' {
                    j += 1;
                }
                i = if j < len { j + 1 } else { len };
            }
            b':' => {
                // Pseudo-class/element: skip the `:`/`::` and the
                // following identifier (and an optional `(...)` arg).
                let mut j = i + 1;
                if j < len && bytes[j] == b':' {
                    j += 1;
                }
                while j < len && is_ident_byte(bytes[j]) {
                    j += 1;
                }
                if j < len && bytes[j] == b'(' {
                    let mut depth = 1i32;
                    j += 1;
                    while j < len && depth > 0 {
                        if bytes[j] == b'(' {
                            depth += 1;
                        } else if bytes[j] == b')' {
                            depth -= 1;
                        }
                        j += 1;
                    }
                }
                i = if j > i { j } else { i + 1 };
            }
            _ => {
                // Unrecognized character: skip it, don't get stuck.
                i += 1;
            }
        }
    }

    Some(cs)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b >= 0x80
}

/// Parses the inside of a `{ ... }` declaration block into a list of
/// `property: value` pairs, splitting on top-level `;` (not nested
/// inside `()`), tolerant of a missing trailing `;`, extra whitespace,
/// and declarations that don't contain a `:` at all (skipped).
fn parse_declarations(body: &str) -> Vec<Declaration> {
    let mut decls = Vec::new();
    for piece in split_top_level(body, ';') {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Split on the FIRST top-level `:` only, so values containing
        // `:` (e.g. `content: "a:b"`, `background: url(http://x)`)
        // survive intact.
        if let Some(colon_idx) = find_first_top_level_colon(trimmed) {
            let (prop, val) = trimmed.split_at(colon_idx);
            let val = &val[1..]; // drop the ':' itself
            let prop = prop.trim();
            let val = val.trim();
            if !prop.is_empty() && !val.is_empty() {
                decls.push(Declaration {
                    property: prop.to_ascii_lowercase(),
                    value: String::from(val),
                });
            }
        }
        // else: no ':' at all in this piece — malformed declaration,
        // skip it silently rather than failing the whole block.
    }
    decls
}

/// Finds the byte index of the first `:` in `s` that isn't nested inside
/// `(`/`)` or a quoted string (`"..."`/`'...'`), so `content: "a:b"` and
/// `background: url(http://x)` don't get split on the wrong colon.
fn find_first_top_level_colon(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_squote = false;
    let mut in_dquote = false;
    for (idx, ch) in s.char_indices() {
        if in_squote {
            if ch == '\'' {
                in_squote = false;
            }
            continue;
        }
        if in_dquote {
            if ch == '"' {
                in_dquote = false;
            }
            continue;
        }
        match ch {
            '\'' => in_squote = true,
            '"' => in_dquote = true,
            '(' => depth += 1,
            ')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            ':' if depth == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}
