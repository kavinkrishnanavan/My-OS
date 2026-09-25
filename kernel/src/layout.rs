//! Ties `dom.rs` (HTML -> tree) and `css.rs` (CSS -> rules) together into
//! something actually drawable: a resolved style per element (cascade +
//! inheritance, real CSS semantics for the handful of properties we
//! support) and a flattened, document-order list of things to draw.
//!
//! Deliberately NOT a real box-model layout engine: there's no box
//! geometry, no nested background rectangles, no floats, no flexbox/grid.
//! Every element still ultimately becomes either an inline run of styled
//! text (flowed via `gfx::Framebuffer::draw_wrapped_styled`, which does
//! its own word-wrapping) or a block boundary that starts a new line —
//! this gets us real per-element *styling* (colors, bold, heading sizes,
//! text-align, hiding `display:none` content) without the much larger
//! undertaking of a real box model. "Close enough to look like a page,"
//! not "pixel-accurate," is the explicit bar here.
//!
//! Images are handled by flattening to a list first, then walking that
//! list with `async`/`.await` for the network fetch — deliberately not
//! fetching inline during the (synchronous) tree walk, since recursive
//! `async fn`s need boxing/pinning in a way that would complicate the
//! tree-walk code for no real benefit; two passes (flatten, then render)
//! is simpler and just as correct.

use crate::css::{CompoundSelector, Declaration, Rule};
use crate::dom::Node;
use crate::gfx::{Color, FontSize, FontWeight};
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// `build_rules`' actual return type: every parsed `Rule`, plus an index
/// from tag name to the indices of rules that could possibly match an
/// element with that tag — letting `resolve_style` skip straight to the
/// handful of relevant rules instead of scanning every rule for every
/// DOM node. This matters a lot in practice: a real page's fetched
/// stylesheet can carry thousands of rules, and a naive `O(nodes × rules)`
/// scan over a large real article's thousands of DOM nodes was measured
/// taking minutes (see the commit this was added in) — bad enough to
/// look like a hang. A rule with a *tag-agnostic* selector (e.g. `.foo`
/// or `#bar` with no element name) has to be checked against every node
/// regardless (`tag_agnostic`); one with an explicit tag (`div.foo`)
/// only needs checking against nodes with that exact tag (`by_tag`).
pub struct RuleIndex {
    rules: Vec<Rule>,
    by_tag: BTreeMap<String, Vec<usize>>,
    tag_agnostic: Vec<usize>,
}

impl RuleIndex {
    fn build(rules: Vec<Rule>) -> Self {
        let mut by_tag: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut tag_agnostic = Vec::new();
        for (i, rule) in rules.iter().enumerate() {
            let mut added_agnostic = false;
            for sel in &rule.selectors {
                match sel.0.last().and_then(|c| c.tag.as_ref()) {
                    Some(tag) => by_tag.entry(tag.clone()).or_default().push(i),
                    None => {
                        if !added_agnostic {
                            tag_agnostic.push(i);
                            added_agnostic = true;
                        }
                    }
                }
            }
        }
        RuleIndex { rules, by_tag, tag_agnostic }
    }

    /// Every rule that could possibly match an element with tag `tag`,
    /// in original stylesheet order (required for the cascade's "later
    /// rule wins" rule — see `resolve_style`) — a merge of the two
    /// relevant index buckets, not just one chained after the other,
    /// since either could legitimately come first in the real
    /// stylesheet.
    fn candidates<'a>(&'a self, tag: &str) -> impl Iterator<Item = &'a Rule> + 'a {
        let mut indices: Vec<usize> = self.by_tag.get(tag).cloned().unwrap_or_default();
        indices.extend_from_slice(&self.tag_agnostic);
        indices.sort_unstable();
        indices.dedup();
        indices.into_iter().map(move |i| &self.rules[i])
    }
}

#[derive(Clone, Copy)]
pub struct ComputedStyle {
    pub color: Color,
    /// Unlike `color`/`bold`/`size`/`align_center`, `background` does NOT
    /// inherit in real CSS — a `None` here means "whatever the page
    /// background already is", not "transparent black". Used two ways:
    /// once for the whole page (`page_background`, from `<body>`/`<html>`),
    /// and per-paragraph (a filled rect drawn behind a `LayoutItem::Text`
    /// whose own block element set one — see `render_page`'s draw loop).
    /// Still not a real box model: the rect is sized to exactly the
    /// paragraph's own rendered bounds, with no padding, no border, and
    /// no support for a background on an element that contains other
    /// *block* children (only ones whose content is a single paragraph).
    pub background: Option<Color>,
    /// Extra vertical space reserved before/after this element's own
    /// paragraph, from `margin-top`/`margin-bottom` (or the `margin`
    /// shorthand's vertical component) — real spacing, not a fixed
    /// constant, but still not real margin *collapsing* (adjacent
    /// margins between two block siblings simply add, rather than the
    /// larger of the two winning as real CSS does — a deliberate
    /// simplification, not an oversight).
    pub margin_top: usize,
    pub margin_bottom: usize,
    /// Padding inside a `background`-box (has no effect without one —
    /// there's no visible box to pad otherwise). Unlike margin, padding
    /// is inside the background fill, matching real CSS box geometry.
    pub padding: usize,
    /// A solid line drawn along the bottom edge of this paragraph's own
    /// box (`border-bottom`) — `(color, thickness_px)`. The only border
    /// side supported: it's overwhelmingly the common case for real
    /// pages (section dividers, table rows), and a full 4-sided border
    /// would need real box geometry this renderer doesn't have.
    pub border_bottom: Option<(Color, usize)>,
    pub bold: bool,
    pub size: FontSize,
    pub align_center: bool,
}

/// A reasonable default for ordinary body text — what a text node
/// inherits if nothing overrode it anywhere up the tree. Deliberately
/// matches real browsers' own UA-default expectation (black-ish text),
/// not this kernel's own dark-terminal aesthetic used everywhere else —
/// real pages (like `example.com`'s own CSS) routinely set only a
/// *background* color and rely on the browser default for text, exactly
/// the same way a real browser's UA stylesheet works.
fn default_style() -> ComputedStyle {
    ComputedStyle {
        color: Color(0x1a, 0x1a, 0x1a),
        background: None,
        margin_top: 0,
        margin_bottom: 0,
        padding: 0,
        border_bottom: None,
        bold: false,
        size: FontSize::Size16,
        align_center: false,
    }
}

/// Block-level tags: each one forces a line break before/after (ends
/// whatever inline run was in progress). Anything not in this list is
/// treated as inline (its text just flows into the surrounding run with
/// its own style applied) — a reasonable default even for tags we don't
/// explicitly know about, since unknown tags in real HTML are far more
/// often inline-ish wrapper elements than block ones.
fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "div" | "p" | "section" | "article" | "header" | "footer" | "nav" | "main" | "aside"
            | "ul" | "ol" | "li" | "table" | "thead" | "tbody" | "tr" | "td" | "th"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "blockquote" | "figure" | "figcaption" | "hr" | "pre" | "form" | "root" | "html" | "body"
    )
}

/// Tags whose entire subtree never contributes visible content — same
/// reasoning `html.rs` already documents for skipping `<script>`/`<style>`.
fn is_never_rendered(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "head" | "noscript" | "template")
}

/// A minimal built-in "user-agent stylesheet" — the same layering real
/// browsers use: page CSS is applied on TOP of sensible tag defaults, not
/// instead of them, so a page still looks roughly right even where our
/// selector matching or the page's own CSS misses something. Lowest
/// priority: real rules parsed from the page always override these.
fn ua_default_rules() -> Vec<Rule> {
    fn rule(tags: &[&str], decls: &[(&str, &str)]) -> Rule {
        Rule {
            selectors: tags
                .iter()
                .map(|t| crate::css::Selector(alloc::vec![CompoundSelector { tag: Some(t.to_string()), id: None, classes: Vec::new() }]))
                .collect(),
            declarations: decls.iter().map(|(p, v)| Declaration { property: p.to_string(), value: v.to_string() }).collect(),
        }
    }
    alloc::vec![
        rule(&["h1"], &[("font-weight", "bold"), ("font-size", "32px"), ("margin-top", "22px"), ("margin-bottom", "22px")]),
        rule(&["h2"], &[("font-weight", "bold"), ("font-size", "24px"), ("margin-top", "20px"), ("margin-bottom", "20px")]),
        rule(&["h3", "h4", "h5", "h6"], &[("font-weight", "bold"), ("font-size", "20px"), ("margin-top", "18px"), ("margin-bottom", "18px")]),
        rule(&["p", "ul", "ol", "blockquote", "table"], &[("margin-top", "16px"), ("margin-bottom", "16px")]),
        rule(&["a"], &[("color", "#5a9cf5")]),
        rule(&["b", "strong"], &[("font-weight", "bold")]),
        rule(&["caption", "th"], &[("font-weight", "bold")]),
    ]
}

/// Does `node` (given its ancestor chain, root-first, `node` itself last)
/// match `selector`'s descendant chain? Each compound selector in the
/// chain must match SOME node at or after the previous match's position
/// walking outward-to-inward along `path` — i.e. real (if simplified,
/// descendant-only) CSS descendant-combinator matching, not requiring
/// direct parentage.
fn selector_matches(path: &[&Node], selector: &crate::css::Selector) -> bool {
    let parts = &selector.0;
    let Some(last) = parts.last() else { return false };
    let Some((&target, ancestors)) = path.split_last() else { return false };
    if !compound_matches(target, last) {
        return false;
    }
    if parts.len() == 1 {
        return true;
    }
    // Walk the remaining (earlier) compound selectors outward through the
    // remaining ancestors, each one needing to match some ancestor at or
    // before the previous match — a simple greedy scan is sufficient
    // for descendant-only matching (no backtracking needed: matching the
    // nearest possible ancestor first can never make an earlier part
    // harder to satisfy, since ancestors only run out, never regrow).
    let mut remaining = &parts[..parts.len() - 1];
    let mut search_space = ancestors;
    while let Some((needle, rest)) = remaining.split_last() {
        let mut found = false;
        for i in (0..search_space.len()).rev() {
            if compound_matches(search_space[i], needle) {
                search_space = &search_space[..i];
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
        remaining = rest;
    }
    true
}

fn compound_matches(node: &Node, sel: &CompoundSelector) -> bool {
    if let Some(tag) = &sel.tag {
        if node.tag != *tag {
            return false;
        }
    }
    if let Some(id) = &sel.id {
        if node.attr("id") != Some(id.as_str()) {
            return false;
        }
    }
    if !sel.classes.is_empty() {
        let node_classes: Vec<&str> = node.attr("class").map(|c| c.split_whitespace().collect()).unwrap_or_default();
        for want in &sel.classes {
            if !node_classes.contains(&want.as_str()) {
                return false;
            }
        }
    }
    true
}

/// Merges every declaration from every rule whose selector matches
/// `path` (in stylesheet order — later rules win on a given property,
/// the same simplification `css.rs`'s own doc comment already
/// documents choosing over real specificity math) onto `inherited`,
/// producing this element's own computed style. `color`/`bold`/`size`/
/// `align_center` all inherit by default (matching real CSS inheritance
/// for text-ish properties) when not explicitly set on this element.
fn resolve_style(path: &[&Node], rules: &RuleIndex, inherited: ComputedStyle) -> ComputedStyle {
    let mut style = inherited;
    // background/margin do not inherit in real CSS — each element starts
    // fresh and only gets one from its OWN matched rules below.
    style.background = None;
    style.margin_top = 0;
    style.margin_bottom = 0;
    style.padding = 0;
    style.border_bottom = None;
    let node = *path.last().expect("path is never empty");

    for rule in rules.candidates(&node.tag) {
        if rule.selectors.iter().any(|sel| selector_matches(path, sel)) {
            for decl in &rule.declarations {
                apply_declaration(&mut style, decl);
            }
        }
    }
    // Inline `style="..."` attribute wins over stylesheet rules, same
    // priority order real browsers use. Parsed directly here (not via
    // `css::parse`, which parses a whole stylesheet with selectors) since
    // an inline `style` attribute is just a bare declaration list.
    if let Some(inline) = node.attr("style") {
        for decl_str in inline.split(';') {
            if let Some((prop, val)) = decl_str.split_once(':') {
                apply_declaration(&mut style, &Declaration { property: prop.trim().to_ascii_lowercase(), value: val.trim().to_string() });
            }
        }
    }
    style
}

fn apply_declaration(style: &mut ComputedStyle, decl: &Declaration) {
    match decl.property.as_str() {
        "color" => {
            if let Some(c) = parse_color(&decl.value) {
                style.color = c;
            }
        }
        "font-weight" => {
            let v = decl.value.trim();
            if v == "bold" || v.parse::<u32>().is_ok_and(|n| n >= 600) {
                style.bold = true;
            } else if v == "normal" {
                style.bold = false;
            }
        }
        "font-size" => {
            if let Some(size) = parse_font_size(&decl.value) {
                style.size = size;
            }
        }
        "text-align" => {
            style.align_center = decl.value.trim() == "center";
        }
        "background-color" => {
            style.background = parse_color(&decl.value);
        }
        // The `background` shorthand can carry position/repeat/image
        // keywords too (`background: url(x) no-repeat`) — we only ever
        // pull a plain color out of it, trying each whitespace-separated
        // token until one parses as a color (real browsers do fuller
        // shorthand parsing; this is deliberately the simple subset).
        "background" => {
            if let Some(c) = decl.value.split_whitespace().find_map(parse_color) {
                style.background = Some(c);
            }
        }
        "margin-top" => {
            if let Some(px) = parse_px(&decl.value) {
                style.margin_top = px;
            }
        }
        "margin-bottom" => {
            if let Some(px) = parse_px(&decl.value) {
                style.margin_bottom = px;
            }
        }
        // The `margin` shorthand: 1 value = all sides, 2 = vert/horiz,
        // 3 = top/horiz/bottom, 4 = top/right/bottom/left. We only ever
        // care about the vertical component (no horizontal box model to
        // apply left/right margin to), so just pick out top/bottom per
        // the standard shorthand-expansion rule.
        "margin" => {
            let parts: Vec<&str> = decl.value.split_whitespace().collect();
            let (top, bottom) = match parts.len() {
                1 => (parts[0], parts[0]),
                2 => (parts[0], parts[0]),
                3 => (parts[0], parts[2]),
                4 => (parts[0], parts[2]),
                _ => return,
            };
            if let Some(px) = parse_px(top) {
                style.margin_top = px;
            }
            if let Some(px) = parse_px(bottom) {
                style.margin_bottom = px;
            }
        }
        // Single scalar padding, applied uniformly — real CSS padding is
        // 4-sided, but with no left/right box geometry here, one number
        // driving "how much space around the text inside its background
        // box" is the honest achievable subset.
        "padding" => {
            if let Some(px) = decl.value.split_whitespace().next().and_then(parse_px) {
                style.padding = px;
            }
        }
        "padding-top" | "padding-bottom" | "padding-left" | "padding-right" => {
            if let Some(px) = parse_px(&decl.value) {
                style.padding = px;
            }
        }
        "border-bottom" | "border" => {
            let color = decl.value.split_whitespace().find_map(parse_color).unwrap_or(style.color);
            let thickness = decl
                .value
                .split_whitespace()
                .find_map(parse_px)
                .filter(|&px| px > 0)
                .unwrap_or(1);
            if decl.value.trim() == "none" || decl.value.trim() == "0" {
                style.border_bottom = None;
            } else {
                style.border_bottom = Some((color, thickness));
            }
        }
        _ => {}
    }
}

/// `#rrggbb` / `#rgb` hex, or a small set of named colors real
/// stylesheets actually use for text — anything else (rgb()/hsl()/other
/// names) is left unparsed (returns `None`, caller keeps the inherited
/// color) rather than guessed at.
fn parse_color(value: &str) -> Option<Color> {
    let v = value.trim();
    if let Some(hex) = v.strip_prefix('#') {
        return match hex.len() {
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(Color(r, g, b))
            }
            3 => {
                let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
                let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
                let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
                Some(Color(r, g, b))
            }
            _ => None,
        };
    }
    match v.to_ascii_lowercase().as_str() {
        "white" => Some(Color(0xea, 0xea, 0xea)),
        "black" => Some(Color(0x18, 0x18, 0x1c)),
        "gray" | "grey" => Some(Color(0x90, 0x90, 0x98)),
        "blue" => Some(Color(0x5a, 0x9c, 0xf5)),
        "red" => Some(Color(0xe0, 0x5a, 0x5a)),
        "green" => Some(Color(0x5a, 0xc9, 0x7a)),
        _ => None,
    }
}

/// Parses a CSS length into whole pixels, for margins — `px`/`em`/`%`
/// (against a 16px baseline, same assumption `parse_font_size` makes),
/// or a bare unitless `0`. `auto` and anything else unparseable returns
/// `None` (caller leaves the existing value alone, same permissive
/// posture every other property parser here takes).
fn parse_px(value: &str) -> Option<usize> {
    let v = value.trim();
    if v == "0" {
        return Some(0);
    }
    let px: f32 = if let Some(n) = v.strip_suffix("px") {
        n.trim().parse().ok()?
    } else if let Some(n) = v.strip_suffix("em") {
        n.trim().parse::<f32>().ok()? * 16.0
    } else if let Some(n) = v.strip_suffix('%') {
        n.trim().parse::<f32>().ok()? / 100.0 * 16.0
    } else {
        return None;
    };
    Some(px.max(0.0) as usize)
}

/// Buckets a CSS font-size value into one of the four discrete raster
/// sizes this kernel's bitmap font actually has — there's no continuous
/// scaling available (no vector/TTF rasterizer here), so "1.5em" and
/// "24px" and "large" all just pick the nearest of four fixed sizes.
fn parse_font_size(value: &str) -> Option<FontSize> {
    let v = value.trim();
    let px: f32 = if let Some(n) = v.strip_suffix("px") {
        n.trim().parse().ok()?
    } else if let Some(n) = v.strip_suffix("em") {
        n.trim().parse::<f32>().ok()? * 16.0
    } else if let Some(n) = v.strip_suffix('%') {
        n.trim().parse::<f32>().ok()? / 100.0 * 16.0
    } else {
        match v {
            "small" => 14.0,
            "medium" => 16.0,
            "large" => 20.0,
            "x-large" => 24.0,
            "xx-large" => 32.0,
            _ => return None,
        }
    };
    Some(if px < 18.0 {
        FontSize::Size16
    } else if px < 22.0 {
        FontSize::Size20
    } else if px < 28.0 {
        FontSize::Size24
    } else {
        FontSize::Size32
    })
}

pub enum LayoutItem {
    /// One paragraph's worth of text, already fully concatenated. Every
    /// text node found anywhere within one block-level element (including
    /// inside nested inline children like `<b>`/`<a>`) is joined into a
    /// single string using the BLOCK element's own resolved style — finer
    /// per-inline-child style differences (e.g. one bold word in the
    /// middle of an otherwise-plain paragraph) are deliberately not
    /// preserved. This is a real fidelity tradeoff, not an oversight: the
    /// alternative (drawing each inline run separately) would put each
    /// styled span on its OWN line with the primitives this kernel's font
    /// renderer actually has (`draw_wrapped_styled` always starts a fresh
    /// line at the left margin — there's no "continue mid-line with a
    /// different style" primitive), which would look far worse than
    /// today's flat-but-correctly-wrapped paragraph. A whole-paragraph
    /// link (an `<a>` that IS the block, e.g. a nav item) still gets its
    /// own correct color/weight — it's only a style change *within* a
    /// paragraph's running text that's lost.
    /// `href`: `Some` when this whole item IS a link's own text — see
    /// `walk`'s handling of `<a>` for why a link always gets its own
    /// item (flushed as its own paragraph boundary) rather than flowing
    /// inline within surrounding text, even though `<a>` is technically
    /// an inline element in real CSS: this kernel's click-to-navigate
    /// needs a way to know which on-screen text maps to which URL, and
    /// the "concatenate everything in a block into one string" design
    /// has no way to track that for a link buried mid-sentence without a
    /// much bigger rearchitecture — breaking a link onto its own line is
    /// the deliberate tradeoff made here to keep links genuinely
    /// clickable at all.
    Text { text: String, style: ComputedStyle, href: Option<String> },
    /// `intended_width`/`intended_height`: the page's own `width`/
    /// `height` HTML attributes on the `<img>` tag, when present — a
    /// real Wikipedia thumbnail is typically encoded much larger than
    /// its intended on-page display size (e.g. a `250px`-wide `<img>`
    /// whose actual decoded bitmap is 1000px+), so scaling to "fit the
    /// content column" instead of the page's own intended size makes
    /// every image look oversized. `None` (no attribute present) falls
    /// back to that fit-to-column behavior.
    Image { src: String, intended_width: Option<usize>, intended_height: Option<usize> },
    /// One `<tr>`'s worth of cells, each already reduced to its own
    /// plain concatenated text (nested tags within a cell contribute
    /// their text but not their own individual styling — the same
    /// "whole element, one style" tradeoff `Text` already makes, just
    /// per-cell instead of per-paragraph). Real tables (Wikipedia
    /// infoboxes, wikitables, nav footers) were previously the single
    /// worst-looking thing this renderer produced: every `<td>`/`<th>`
    /// was just another block, so a whole table became a long vertical
    /// wall of one-cell-per-line text with no columns at all. This gets
    /// an actual grid — `net/http.rs`'s `draw_at_scroll` divides the
    /// content column evenly across `cells.len()` and draws each cell
    /// in its own column, wrapped, with light divider lines — not a
    /// real box model (no colspan/rowspan, no per-column sizing based
    /// on content), but a real visual grid instead of a flat stack.
    TableRow { cells: Vec<String>, style: ComputedStyle },
}

/// Walks `node` and its subtree in document order, producing a flat,
/// linear `LayoutItem` list — the synchronous half of rendering (no
/// network I/O happens here; see the module doc comment for why images
/// are deferred to a second pass).
pub fn flatten(root: &Node, rules: &RuleIndex) -> Vec<LayoutItem> {
    let mut out = Vec::new();
    let mut path: Vec<&Node> = Vec::new();
    let mut state = ParagraphState { text: String::new(), style: default_style(), href: None };
    walk(root, rules, default_style(), &mut path, &mut out, &mut state);
    flush(&mut state, &mut out);
    out
}

struct ParagraphState {
    text: String,
    style: ComputedStyle,
    href: Option<String>,
}

fn flush(state: &mut ParagraphState, out: &mut Vec<LayoutItem>) {
    let trimmed = state.text.trim();
    if !trimmed.is_empty() {
        out.push(LayoutItem::Text { text: trimmed.to_string(), style: state.style, href: state.href.clone() });
    }
    state.text.clear();
}

fn walk<'a>(node: &'a Node, rules: &RuleIndex, inherited: ComputedStyle, path: &mut Vec<&'a Node>, out: &mut Vec<LayoutItem>, state: &mut ParagraphState) {
    if node.tag.is_empty() {
        // Text node: collapse internal whitespace runs to one space (real
        // HTML whitespace-collapsing behavior — `dom.rs` deliberately
        // leaves this to us) and append into the paragraph in progress.
        let mut last_was_space = state.text.ends_with(char::is_whitespace) || state.text.is_empty();
        for ch in node.text.chars() {
            if ch.is_whitespace() {
                if !last_was_space {
                    state.text.push(' ');
                }
                last_was_space = true;
            } else {
                state.text.push(ch);
                last_was_space = false;
            }
        }
        return;
    }
    if is_never_rendered(&node.tag) {
        return;
    }
    if node.tag == "img" {
        flush(state, out);
        if let Some(src) = node.attr("src") {
            out.push(LayoutItem::Image {
                src: src.to_string(),
                intended_width: node.attr("width").and_then(|w| w.parse().ok()),
                intended_height: node.attr("height").and_then(|h| h.parse().ok()),
            });
        }
        return;
    }
    if node.tag == "table" {
        flush(state, out);
        path.push(node);
        let style = resolve_style(path, rules, inherited);
        walk_table_rows(node, rules, style, path, out);
        path.pop();
        return;
    }

    path.push(node);
    let style = resolve_style(path, rules, inherited);
    let display_none = node.attr("style").is_some_and(|s| s.contains("display:none") || s.contains("display: none"))
        || rules.candidates(&node.tag).any(|r| {
            r.selectors.iter().any(|s| selector_matches(path, s))
                && r.declarations.iter().any(|d| d.property == "display" && d.value.trim() == "none")
        });

    if !display_none {
        // A real `<a href>` gets its own flush boundary same as a block
        // element — see `LayoutItem::Text`'s doc comment on `href` for
        // why (click-to-navigate needs a way to know which on-screen
        // text maps to which URL, which the paragraph-concatenation
        // design can't track for a link buried mid-sentence otherwise).
        let link_href = (node.tag == "a").then(|| node.attr("href")).flatten();
        let block = is_block(&node.tag) || link_href.is_some();
        if block {
            flush(state, out);
            state.style = style;
            state.href = link_href.map(|h| h.to_string());
            if node.tag == "li" {
                // ASCII "* " rather than a real bullet glyph (U+2022):
                // this font only has the basic-Latin/Latin-1-supplement
                // unicode ranges enabled (see kernel/Cargo.toml), so a
                // real bullet character would just silently fail to
                // raster and draw nothing.
                state.text.push_str("* ");
            }
        }
        for child in &node.children {
            walk(child, rules, style, path, out, state);
        }
        if block {
            flush(state, out);
            state.href = None;
        }
    }
    path.pop();
}

/// Finds every `<tr>` anywhere within a `<table>` (recursing through
/// `<thead>`/`<tbody>`/`<tfoot>` wrappers, which real HTML almost always
/// has and which aren't block boundaries of their own for this purpose)
/// and emits one `LayoutItem::TableRow` per row. A nested `<table>`
/// inside a cell is walked for its own rows too (real Wikipedia infoboxes
/// nest tables sometimes) — not correctly nested as its own sub-grid,
/// just flattened into the same row stream, which is a real fidelity
/// gap but far better than crashing or silently dropping the content.
fn walk_table_rows<'a>(node: &'a Node, rules: &RuleIndex, inherited: ComputedStyle, path: &mut Vec<&'a Node>, out: &mut Vec<LayoutItem>) {
    if node.tag == "tr" {
        let cells: Vec<String> = node
            .children
            .iter()
            .filter(|c| c.tag == "td" || c.tag == "th")
            .map(cell_text)
            .collect();
        if !cells.is_empty() {
            path.push(node);
            let style = resolve_style(path, rules, inherited);
            path.pop();
            out.push(LayoutItem::TableRow { cells, style });
        }
        return;
    }
    for child in &node.children {
        if child.tag.is_empty() || is_never_rendered(&child.tag) {
            continue;
        }
        path.push(child);
        let style = resolve_style(path, rules, inherited);
        walk_table_rows(child, rules, style, path, out);
        path.pop();
    }
}

/// Concatenates all text anywhere within `node`'s subtree into one
/// whitespace-collapsed string — a cell's own plain-text content,
/// ignoring any nested tags' individual styling (the same "whole
/// element, one style" tradeoff `LayoutItem::Text` already documents).
fn cell_text(node: &Node) -> String {
    let mut out = String::new();
    fn walk_text(node: &Node, out: &mut String) {
        if node.tag.is_empty() {
            for ch in node.text.chars() {
                if ch.is_whitespace() {
                    if !out.ends_with(' ') && !out.is_empty() {
                        out.push(' ');
                    }
                } else {
                    out.push(ch);
                }
            }
            return;
        }
        if is_never_rendered(&node.tag) {
            return;
        }
        for child in &node.children {
            walk_text(child, out);
        }
    }
    walk_text(node, &mut out);
    out.trim().to_string()
}

/// Collects the text content of every `<style>` element anywhere in the
/// tree — inline page CSS, which gets parsed and merged with whatever
/// external stylesheet the caller fetched separately.
pub fn collect_inline_css(node: &Node, out: &mut String) {
    if node.tag == "style" {
        for child in &node.children {
            if child.tag.is_empty() {
                out.push_str(&child.text);
                out.push('\n');
            }
        }
        return;
    }
    for child in &node.children {
        collect_inline_css(child, out);
    }
}

/// Resolves the whole page's background color: finds `<body>` (falling
/// back to `<html>`, then the document root if neither exists) and
/// resolves its own style. Real browsers propagate a `<body>` background
/// to the whole viewport even though `background` itself doesn't
/// inherit — this is that one special case, done once for the whole
/// page (there's no per-element background box rendering yet — see the
/// module doc comment) rather than as a general inheritance rule.
/// Falls back to a light default (`#eaeaea`) when nothing sets one,
/// matching a real browser's own default canvas rather than this
/// kernel's usual dark-terminal aesthetic — pages routinely rely on that
/// browser default (e.g. setting no `color` at all, only a light
/// `background`) the same way `example.com`'s own CSS does.
pub fn page_background(root: &Node, rules: &RuleIndex) -> Color {
    fn find<'a>(node: &'a Node, tag: &str) -> Option<&'a Node> {
        if node.tag == tag {
            return Some(node);
        }
        for child in &node.children {
            if let Some(found) = find(child, tag) {
                return Some(found);
            }
        }
        None
    }
    fn build_path<'a>(node: &'a Node, target: *const Node, path: &mut Vec<&'a Node>) -> bool {
        path.push(node);
        if core::ptr::eq(node, target) {
            return true;
        }
        for child in &node.children {
            if build_path(child, target, path) {
                return true;
            }
        }
        path.pop();
        false
    }

    let target = find(root, "body").or_else(|| find(root, "html")).unwrap_or(root);
    let mut path = Vec::new();
    build_path(root, target as *const Node, &mut path);
    resolve_style(&path, rules, default_style()).background.unwrap_or(Color(0xea, 0xea, 0xea))
}

/// Resolves the content column width from `<body>`'s (falling back to
/// `<html>`'s) own `width`/`max-width`, against `viewport_width` — real
/// pages routinely set something like `width:60vw;margin:0 auto` to get
/// a centered, readable column instead of full-bleed text edge-to-edge
/// (`example.com`'s own CSS does exactly this). Returns `None` if
/// neither sets a width, meaning "use the full viewport".
pub fn page_content_width(root: &Node, rules: &RuleIndex, viewport_width: usize) -> Option<usize> {
    fn find<'a>(node: &'a Node, tag: &str) -> Option<&'a Node> {
        if node.tag == tag {
            return Some(node);
        }
        for child in &node.children {
            if let Some(found) = find(child, tag) {
                return Some(found);
            }
        }
        None
    }
    fn build_path<'a>(node: &'a Node, target: *const Node, path: &mut Vec<&'a Node>) -> bool {
        path.push(node);
        if core::ptr::eq(node, target) {
            return true;
        }
        for child in &node.children {
            if build_path(child, target, path) {
                return true;
            }
        }
        path.pop();
        false
    }

    let target = find(root, "body").or_else(|| find(root, "html"))?;
    let mut path = Vec::new();
    build_path(root, target as *const Node, &mut path);

    let mut width_decl: Option<String> = None;
    for rule in rules.candidates(&target.tag) {
        if rule.selectors.iter().any(|sel| selector_matches(&path, sel)) {
            for decl in &rule.declarations {
                if decl.property == "width" || decl.property == "max-width" {
                    width_decl = Some(decl.value.clone());
                }
            }
        }
    }
    let value = width_decl?;
    let v = value.trim();
    let px: f32 = if let Some(n) = v.strip_suffix("vw") {
        n.trim().parse::<f32>().ok()? / 100.0 * viewport_width as f32
    } else if let Some(n) = v.strip_suffix("px") {
        n.trim().parse().ok()?
    } else if let Some(n) = v.strip_suffix('%') {
        n.trim().parse::<f32>().ok()? / 100.0 * viewport_width as f32
    } else {
        return None;
    };
    Some((px as usize).clamp(200, viewport_width))
}

/// The UA default rules followed by `page_css` — later (page) rules win
/// per `resolve_style`'s "later rule wins" cascade simplification.
pub fn build_rules(page_css: &str) -> RuleIndex {
    let mut rules = ua_default_rules();
    rules.extend(crate::css::parse(page_css));
    RuleIndex::build(rules)
}

pub fn font_weight(style: &ComputedStyle) -> FontWeight {
    if style.bold {
        FontWeight::Bold
    } else {
        FontWeight::Regular
    }
}
