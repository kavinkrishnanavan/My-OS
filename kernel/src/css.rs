//! CSS preprocessing plus the glue this kernel needs to hand real CSS
//! text to `simplecss` for actual parsing/selector-matching — real
//! specificity computation (`Selector::specificity`), real attribute
//! selectors, and generally more complete CSS2 support than this
//! kernel's own hand-rolled parser used to have (removed once this
//! module switched over: it was fuzz-tested and worked, but "later
//! rule wins" instead of real specificity is a genuine correctness gap
//! on real pages, and duplicating a real parser's work wasn't worth
//! keeping around).
//!
//! What `simplecss` does NOT do that this kernel's rendering still
//! needs, handled here instead:
//! - **At-rule flattening.** `simplecss`'s own docs: "At-rules are not
//!   supported. They will be skipped during parsing." Silently dropping
//!   every `@media`-wrapped rule would be a real regression from this
//!   kernel's established, deliberate choice (see `layout.rs`'s module
//!   doc comment) to over-apply CSS rather than drop it — most real
//!   pages wrap real layout-relevant rules in `@media` blocks.
//!   `flatten_at_rules` strips each at-rule's own condition/wrapper
//!   while keeping (block-form) or dropping (statement-form, e.g.
//!   `@import ...;`) its contents, unconditionally — the same
//!   over-apply choice, just implemented as a text-level preprocessing
//!   pass before `simplecss::StyleSheet::parse` ever sees the CSS,
//!   since `simplecss::Rule`/`Selector` are fully opaque (no public way
//!   to inspect or reconstruct them after the fact).
//! - **DOM traversal for matching.** `simplecss::Selector::matches`
//!   needs something implementing its `Element` trait; `PathElement`
//!   wraps this kernel's own ancestor-`path`-based DOM walk (`layout.rs`
//!   builds `path: &[&dom::Node]` while walking, rather than storing
//!   parent pointers on `Node` itself) to satisfy it.
//! - **Tag-based rule indexing.** `simplecss::Selector` exposes no way
//!   to ask "what tag does this select for" without a full `matches()`
//!   call against a real element — `RuleIndex` (layout.rs) needs that
//!   to avoid the real O(nodes × rules) performance cliff a previous,
//!   naive version of this renderer hit on large real pages (see that
//!   type's own doc comment). `quick_target_tag` is a best-effort,
//!   text-level guess at the target compound's tag name, used ONLY for
//!   bucketing — actual matching always goes through `simplecss`'s own
//!   `Selector::matches`, so a wrong or missed guess here can only make
//!   indexing less optimal (falls back to "checked against every
//!   node," the same as before this optimization existed at all), never
//!   incorrect.

use crate::dom::Node;
use alloc::string::String;

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

/// Strips comments, then flattens every at-rule: a block-form at-rule
/// (`@media (...) { ... }`) is replaced by just its own inner contents
/// (condition discarded, recursing in case of nested at-rules); a
/// statement-form one (`@import url(...);`) is dropped entirely. Never
/// panics or loops on malformed input — unbalanced braces just end
/// preprocessing early, same tolerance the old hand-rolled parser had.
pub fn preprocess(css: &str) -> String {
    let cleaned = strip_comments(css);
    let mut out = String::with_capacity(cleaned.len());
    flatten_at_rules(&cleaned, &mut out);
    out
}

fn flatten_at_rules(src: &str, out: &mut String) {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        if bytes[i] == b'@' {
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
                if let Some(end_idx) = find_matching_brace(bytes, brace_idx) {
                    flatten_at_rules(&src[brace_idx + 1..end_idx], out);
                    out.push('\n');
                    i = end_idx + 1;
                } else {
                    i = len;
                }
            } else if let Some(semi_idx) = semi_pos {
                i = semi_idx + 1;
            } else {
                i = len;
            }
            continue;
        }

        // Ordinary (non-at-rule) content: copy through verbatim up to
        // the next '@' so simplecss sees it unmodified.
        let start = i;
        while i < len && bytes[i] != b'@' {
            i += 1;
        }
        out.push_str(&src[start..i]);
    }
}

/// A view of one element in `layout.rs`'s ancestor-path DOM walk,
/// implementing `simplecss::Element` so its selectors can be matched
/// against this kernel's own `dom::Node` tree. `path`'s last entry is
/// "this" element; everything before it is the ancestor chain,
/// outermost first — exactly what `layout.rs` already builds while
/// walking, so this borrows it rather than needing real parent
/// pointers on `Node` (which the tree doesn't have).
#[derive(Clone, Copy)]
pub struct PathElement<'a> {
    pub path: &'a [&'a Node],
}

impl<'a> PathElement<'a> {
    fn node(&self) -> &'a Node {
        self.path.last().expect("PathElement is never built from an empty path")
    }
}

impl<'a> simplecss::Element for PathElement<'a> {
    fn parent_element(&self) -> Option<Self> {
        let ancestors = &self.path[..self.path.len() - 1];
        if ancestors.is_empty() {
            None
        } else {
            Some(PathElement { path: ancestors })
        }
    }

    // Real sibling tracking would need `layout.rs`'s walk to carry
    // per-level child-index state it doesn't currently have (the
    // ancestor path only ever grows downward, never sideways) — `None`
    // means selectors using `+`/`~` combinators or sibling-position
    // pseudo-classes (`:first-child` etc.) simply never match, the same
    // "degrade gracefully rather than guess wrong" choice the removed
    // hand-rolled parser made for constructs it didn't support either.
    fn prev_sibling_element(&self) -> Option<Self> {
        None
    }

    fn has_local_name(&self, name: &str) -> bool {
        self.node().tag.eq_ignore_ascii_case(name)
    }

    fn attribute_matches(&self, local_name: &str, operator: simplecss::AttributeOperator<'_>) -> bool {
        let Some(value) = self.node().attr(local_name) else { return false };
        match operator {
            simplecss::AttributeOperator::Exists => true,
            simplecss::AttributeOperator::Matches(want) => value == want,
            simplecss::AttributeOperator::Contains(want) => value.split_whitespace().any(|part| part == want),
            simplecss::AttributeOperator::StartsWith(want) => {
                value == want || (value.starts_with(want) && value[want.len()..].starts_with('-'))
            }
        }
    }

    // No interaction state (`:hover`/`:focus`) or document-position
    // state (`:first-child`/`:nth-child`, which would need the sibling
    // tracking `prev_sibling_element` already doesn't have) is tracked
    // — every pseudo-class simply never matches, the same "selector
    // just doesn't match rather than guessing" degradation as above.
    fn pseudo_class_matches(&self, _class: simplecss::PseudoClass<'_>) -> bool {
        false
    }
}

/// A best-effort guess at the tag name a selector's own target (rightmost/
/// subject) compound selector requires, extracted straight from the
/// selector's own text (via `simplecss::Selector`'s `Display` impl) —
/// used only for `RuleIndex`'s tag-bucketing (see this module's own doc
/// comment for why a wrong or missed guess here is harmless). Returns
/// `None` for a tag-agnostic subject (`.foo`, `#bar`, `*`) or anything
/// this simple scan doesn't recognize.
pub fn quick_target_tag(selector_text: &str) -> Option<String> {
    let last_compound = selector_text.split([' ', '\t', '>', '+', '~']).filter(|s| !s.is_empty()).next_back()?;
    let tag: String = last_compound.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    if tag.is_empty() || tag == "*" {
        None
    } else {
        Some(tag.to_ascii_lowercase())
    }
}
