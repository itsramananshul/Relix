//! `tool.web_extract` — deterministic HTML parser.
//!
//! Operates **only** on caller-supplied HTML bytes. There is no network
//! I/O in this module: a caller (typically a SOL flow) is expected to
//! have already obtained the HTML via `tool.web_fetch` (which has its
//! own SSRF + DNS-pin guards) or by some other means, then passes the
//! bytes to `tool.web_extract` for parsing.
//!
//! Keeping fetch and extract as separate capabilities preserves the
//! same separation `tool.web_fetch` already enforces: extract has no
//! credentials, no network surface, no SSRF concerns. Parsing
//! arbitrary HTML cannot trigger an outbound dial. JavaScript is
//! **not** executed — `<script>` content is dropped.
//!
//! ## Wire format (SIMP-016 alpha — UTF-8 strings)
//!
//! Arg: `<mode>|<html>` where `<html>` may contain `|`. Modes:
//!
//! | Mode | Returns |
//! |---|---|
//! | `text`  | Visible text only (scripts/styles/comments removed; entities decoded; whitespace collapsed). |
//! | `title` | Single line: contents of `<title>`. Empty if absent. |
//! | `links` | One absolute-or-relative URL per line, deduplicated, in document order. |
//! | `meta`  | One `name\tcontent` per line. Both `name=` and `property=` (OpenGraph) attributes are recognised. |
//! | `markdown` (PH-WEB-MARKDOWN) | HTML → Markdown structural conversion (headings, paragraphs, links, lists, code, blockquotes, hr, emphasis). Scripts / styles dropped, entities decoded. |
//! | `all`   | Multi-line `key=value` block: `title=`, `link_count=`, `meta_count=`, then `text=` followed by the text body. Suitable for one-shot inspection from a CLI or another flow. |
//!
//! ## Limits
//!
//! Input size is capped at `[tool] extract_max_input_bytes` (default
//! 1 MiB). Larger inputs are rejected as `invalid_args` — the SOL flow
//! that called us should have capped at fetch time anyway via
//! `tool.web_fetch`'s `|<N>` suffix.
//!
//! ## Limitations honestly
//!
//! - Not a real HTML5 parser. A small state-machine handles
//!   `<script>` / `<style>` / `<!-- ... -->` skipping, tag stripping,
//!   and entity decoding. Malformed or adversarial HTML may produce
//!   weird text. It will not panic and it will not execute anything.
//! - Entity decoding covers the named-entity short list (`&amp;`,
//!   `&lt;`, `&gt;`, `&quot;`, `&apos;`, `&nbsp;`, `&copy;`, `&reg;`,
//!   `&trade;`, `&hellip;`, `&mdash;`, `&ndash;`, `&middot;`) plus
//!   numeric forms (`&#NNN;`, `&#xHEX;`). Other named entities pass
//!   through unchanged.
//! - Link extraction looks at `<a href=...>` only. `<img src>`,
//!   `<link rel=stylesheet>`, etc. are intentionally out of scope.
//! - Meta extraction reads `<meta name="X" content="Y">` and
//!   `<meta property="X" content="Y">`. `http-equiv` is skipped.

use std::sync::Arc;

use serde::Deserialize;

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Per-node config knob for `tool.web_extract`. Lives in `[tool]`
/// alongside the existing web_fetch knobs.
#[derive(Clone, Debug, Deserialize)]
pub struct WebExtractConfig {
    /// Maximum input bytes accepted. Defaults to 1 MiB.
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: usize,
}

impl Default for WebExtractConfig {
    fn default() -> Self {
        Self {
            max_input_bytes: default_max_input_bytes(),
        }
    }
}

fn default_max_input_bytes() -> usize {
    1024 * 1024
}

/// Capability descriptor — pure parser, no network surface.
pub fn capability_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.web_extract");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::Idempotent; // pure function of the input
    d.cost_class = CostClass::Cheap; // CPU-only, bounded by input size
    d.sensitivity_tags = vec!["parse:html".into()];
    d.policy_attachment_point = "tool.web_extract".to_string();
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Extract title / text / links / meta from HTML bytes. No network access; \
         scripts and styles are stripped."
            .into(),
    );
    d.categories = vec!["parse".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

/// Register the capability on the dispatch bridge.
pub fn register(bridge: &mut DispatchBridge, cfg: Arc<WebExtractConfig>) {
    let cfg_for_handler = cfg.clone();
    bridge.register(
        "tool.web_extract",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let cfg = cfg_for_handler.clone();
            async move { handle(&cfg, &ctx) }
        })),
    );
}

fn handle(cfg: &WebExtractConfig, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.web_extract arg utf8: {e}")),
    };
    // splitn(2) so `html` can contain `|`.
    let mut parts = raw.splitn(2, '|');
    let mode_str = parts.next().unwrap_or("").trim();
    let html = parts.next().unwrap_or("");
    if mode_str.is_empty() {
        return invalid(
            "tool.web_extract: mode required (text/title/links/meta/markdown/all)".into(),
        );
    }
    let mode = match Mode::parse(mode_str) {
        Some(m) => m,
        None => {
            return invalid(format!(
                "tool.web_extract: unknown mode '{mode_str}' (text/title/links/meta/markdown/all)"
            ));
        }
    };
    if html.len() > cfg.max_input_bytes {
        return invalid(format!(
            "tool.web_extract: input {} bytes exceeds cap {}",
            html.len(),
            cfg.max_input_bytes
        ));
    }
    // PH-WEB-MARKDOWN: markdown mode uses a different state
    // machine; skip the general extractor for that case.
    let body = match mode {
        Mode::Markdown => extract_markdown(html),
        _ => {
            let extracted = extract(html);
            match mode {
                Mode::Text => extracted.text,
                Mode::Title => extracted.title.unwrap_or_default(),
                Mode::Links => extracted.links.join("\n"),
                Mode::Meta => extracted
                    .meta
                    .into_iter()
                    .map(|(k, v)| format!("{k}\t{v}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Mode::All => render_all(&extracted),
                Mode::Markdown => unreachable!("handled above"),
            }
        }
    };
    HandlerOutcome::Ok(body.into_bytes())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Title,
    Links,
    Meta,
    All,
    Markdown,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "title" => Some(Self::Title),
            "links" => Some(Self::Links),
            "meta" => Some(Self::Meta),
            "all" => Some(Self::All),
            "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }
}

/// Parsed HTML view. Public so other modules / tests can use the
/// extractor directly.
///
/// SEC PART 1: every field carries text pulled from an
/// attacker-controllable web page. In-process consumers that
/// feed these values into an LLM prompt MUST wrap them via
/// `relix_core::types::UntrustedText::new(value).wrap_for_prompt()`
/// (or route through `ai.perception_extract` for two-stage
/// isolation). The boundary is enforced at prompt-
/// construction time.
#[derive(Debug, Clone, Default)]
pub struct Extracted {
    pub title: Option<String>,
    pub text: String,
    pub links: Vec<String>,
    pub meta: Vec<(String, String)>,
}

/// Extract everything in one pass. The hand-rolled state machine walks
/// the input once; each mode just projects from the resulting
/// [`Extracted`].
//
// The match-arm guards plus inner-if pattern is intentional: keeping
// each match arm independent makes the parser easy to read top-to-bottom
// at the cost of clippy nits about collapsing them. We accept the nits.
#[allow(clippy::collapsible_if, clippy::collapsible_match)]
pub fn extract(html: &str) -> Extracted {
    let bytes = html.as_bytes();
    let mut i = 0;
    let n = bytes.len();

    let mut text_buf = String::with_capacity(html.len() / 2);
    let mut title: Option<String> = None;
    let mut links_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut links: Vec<String> = Vec::new();
    let mut meta: Vec<(String, String)> = Vec::new();

    let mut in_script = false;
    let mut in_style = false;
    let mut last_space = true; // collapse leading whitespace

    while i < n {
        let b = bytes[i];

        // Comment: `<!-- ... -->`
        if b == b'<' && i + 4 <= n && &bytes[i..i + 4] == b"<!--" {
            if let Some(end) = find_subslice(&bytes[i + 4..], b"-->") {
                i = i + 4 + end + 3;
            } else {
                i = n;
            }
            continue;
        }

        // CDATA: `<![CDATA[ ... ]]>`. Not legal in HTML5 outside
        // foreign content (SVG/MathML) but appears in scraped pages.
        // Treat the whole section as ignored markup so the body
        // (which may include `<script>` tags whose payload would
        // otherwise leak as text — see Track 6 hardening tests)
        // never gets parsed as inline content.
        if b == b'<' && i + 9 <= n && &bytes[i..i + 9] == b"<![CDATA[" {
            if let Some(end) = find_subslice(&bytes[i + 9..], b"]]>") {
                i = i + 9 + end + 3;
            } else {
                i = n;
            }
            continue;
        }

        if b == b'<' && i + 1 < n {
            // Tag start. Identify tag name (next ASCII letters), maybe
            // with leading `/`.
            let is_close = bytes[i + 1] == b'/';
            let name_start = if is_close { i + 2 } else { i + 1 };
            let name_end = name_start
                + bytes[name_start..]
                    .iter()
                    .take_while(|&&c| c.is_ascii_alphanumeric() || c == b':')
                    .count();
            let tag_name_lower = ascii_lower_str(&bytes[name_start..name_end]);

            // Find end of opening tag.
            let tag_end = match memchr_byte(&bytes[i..], b'>') {
                Some(off) => i + off,
                None => {
                    // Malformed — bail.
                    break;
                }
            };
            let tag_inner = &bytes[name_end..tag_end]; // attributes (and trailing `/`)

            // Toggle script/style based on open/close.
            match tag_name_lower.as_str() {
                "script" => in_script = !is_close,
                "style" => in_style = !is_close,
                _ => {}
            }

            if !is_close {
                match tag_name_lower.as_str() {
                    "title" => {
                        let body_start = tag_end + 1;
                        if let Some(close_rel) = find_subslice_ci(&bytes[body_start..], b"</title>")
                        {
                            let raw = &bytes[body_start..body_start + close_rel];
                            let s = decode_entities(&collapse_ws(
                                std::str::from_utf8(raw).unwrap_or(""),
                            ));
                            title = Some(s.trim().to_string());
                            i = body_start + close_rel + b"</title>".len();
                            continue;
                        }
                    }
                    "a" => {
                        if let Some(href) = read_attr(tag_inner, b"href") {
                            let href = decode_entities(href.trim());
                            if !href.is_empty() && links_seen.insert(href.clone()) {
                                links.push(href);
                            }
                        }
                    }
                    "meta" => {
                        let name = read_attr(tag_inner, b"name")
                            .or_else(|| read_attr(tag_inner, b"property"));
                        let content = read_attr(tag_inner, b"content");
                        if let (Some(n), Some(c)) = (name, content) {
                            let n = decode_entities(n.trim());
                            let c = decode_entities(c.trim());
                            if !n.is_empty() {
                                meta.push((n, c));
                            }
                        }
                    }
                    // Block-level tags become whitespace boundaries so
                    // adjacent text doesn't run together.
                    "p" | "br" | "div" | "li" | "tr" | "td" | "th" | "h1" | "h2" | "h3" | "h4"
                    | "h5" | "h6" | "section" | "article" | "header" | "footer" | "nav"
                    | "main" | "aside" | "blockquote" | "pre" | "hr" => {
                        if !last_space && !in_script && !in_style {
                            text_buf.push(' ');
                            last_space = true;
                        }
                    }
                    _ => {}
                }
            } else {
                // Closing tag for block-level — same whitespace
                // boundary insertion.
                match tag_name_lower.as_str() {
                    "p" | "div" | "li" | "tr" | "td" | "th" | "h1" | "h2" | "h3" | "h4" | "h5"
                    | "h6" | "section" | "article" | "header" | "footer" | "nav" | "main"
                    | "aside" | "blockquote" | "pre" => {
                        if !last_space && !in_script && !in_style {
                            text_buf.push(' ');
                            last_space = true;
                        }
                    }
                    _ => {}
                }
            }

            i = tag_end + 1;
            continue;
        }

        // Plain text character.
        if !in_script && !in_style {
            if b.is_ascii_whitespace() {
                if !last_space {
                    text_buf.push(' ');
                    last_space = true;
                }
                i += 1;
            } else {
                // Decode entity if this is an `&...;`.
                if b == b'&' {
                    if let Some((decoded, consumed)) = decode_one_entity(&bytes[i..]) {
                        // entity may decode to multiple chars; append
                        // and re-evaluate whitespace state from the
                        // last appended char.
                        text_buf.push_str(&decoded);
                        last_space = decoded.chars().last().is_some_and(|c| c.is_whitespace());
                        i += consumed;
                        continue;
                    }
                }
                // Plain ASCII byte or UTF-8 lead — push verbatim.
                // We rely on the input being valid UTF-8 (str::as_bytes).
                let ch_start = i;
                let ch_len = utf8_char_len(b);
                let end = (ch_start + ch_len).min(n);
                text_buf.push_str(std::str::from_utf8(&bytes[ch_start..end]).unwrap_or(""));
                last_space = false;
                i = end;
            }
        } else {
            // Inside script/style — skip everything until the matching
            // close tag is found by the outer loop.
            i += 1;
        }
    }

    let text = text_buf.trim().to_string();
    Extracted {
        title,
        text,
        links,
        meta,
    }
}

/// PH-WEB-MARKDOWN: convert an HTML fragment to Markdown.
///
/// Hand-rolled single-pass walker. Maintains an inline buffer
/// per block; flushes the buffer with the appropriate Markdown
/// prefix when a block boundary is encountered. Block elements:
/// `<h1>`..`<h6>`, `<p>`, `<pre>`, `<blockquote>`, `<ul>`/`<ol>`,
/// `<li>`, `<hr>`. Inline elements: `<a>`, `<strong>`/`<b>`,
/// `<em>`/`<i>`, `<code>`, `<br>`, `<img>`.
///
/// **Limitations:**
/// - Not an HTML5 parser. Malformed input may produce odd
///   Markdown; will not panic.
/// - Tables, definition lists, footnotes are NOT rendered (the
///   text content survives, but without table structure).
/// - Nested lists are emitted as flat lists with two-space
///   indent per level — works in most Markdown renderers.
/// - Code blocks use triple-backtick fences with no language
///   tag (would need `<code class="language-X">` extraction).
#[allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::too_many_lines
)]
pub fn extract_markdown(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    let mut out = String::with_capacity(html.len());

    // Inline buffer accumulates text content for the current
    // block; flushed by `flush_block` when a block boundary
    // arrives. Tracks its own whitespace state.
    let mut inline_buf = String::new();
    let mut inline_last_space = true;

    // The kind of block we're currently building. Switched at
    // each opening block-level tag.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Block {
        Paragraph,
        Heading(u8),
        Pre,
    }
    let mut block = Block::Paragraph;

    // Nesting stacks.
    #[derive(Debug, Clone, Copy)]
    enum ListKind {
        Ul,
        Ol(u32),
    }
    let mut list_stack: Vec<ListKind> = Vec::new();
    let mut blockquote_depth: usize = 0;

    // Pending link href — when an <a> opens, store the href;
    // when it closes, wrap inline_buf's last span with [..](href).
    let mut pending_href: Option<(String, usize)> = None; // (href, inline_buf len at open)

    // Format / strip state for inline markers we have to emit
    // verbatim into inline_buf.
    let mut emphasis_open = 0u8; // <em>/<i>
    let mut strong_open = 0u8; // <strong>/<b>
    let mut code_inline_open = 0u8;

    let mut in_script = false;
    let mut in_style = false;

    // Helper closure-shaped flush: emit the current inline buffer
    // as a block according to `block`, applying blockquote /
    // list / heading prefixes. Resets the inline buffer.
    let flush_block = |out: &mut String,
                       inline_buf: &mut String,
                       inline_last_space: &mut bool,
                       block: &Block,
                       list_stack: &[ListKind],
                       blockquote_depth: usize| {
        let s = inline_buf.trim().to_string();
        if s.is_empty() && !matches!(block, Block::Pre) {
            return;
        }
        let bq: String = std::iter::repeat_n("> ", blockquote_depth).collect();
        let list_indent: String = std::iter::repeat_n("  ", list_stack.len()).collect();
        match block {
            Block::Heading(level) => {
                let hashes: String = std::iter::repeat_n('#', *level as usize).collect();
                out.push_str(&bq);
                out.push_str(&list_indent);
                out.push_str(&hashes);
                out.push(' ');
                out.push_str(&s);
                out.push_str("\n\n");
            }
            Block::Pre => {
                out.push_str("```\n");
                out.push_str(&s);
                if !s.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n\n");
            }
            Block::Paragraph => {
                if !s.is_empty() {
                    out.push_str(&bq);
                    out.push_str(&list_indent);
                    out.push_str(&s);
                    out.push_str("\n\n");
                }
            }
        }
        inline_buf.clear();
        *inline_last_space = true;
    };

    while i < n {
        let b = bytes[i];

        if b == b'<' && i + 4 <= n && &bytes[i..i + 4] == b"<!--" {
            if let Some(end) = find_subslice(&bytes[i + 4..], b"-->") {
                i = i + 4 + end + 3;
            } else {
                i = n;
            }
            continue;
        }

        if b == b'<' && i + 1 < n {
            let is_close = bytes[i + 1] == b'/';
            let name_start = if is_close { i + 2 } else { i + 1 };
            let name_end = name_start
                + bytes[name_start..]
                    .iter()
                    .take_while(|&&c| c.is_ascii_alphanumeric() || c == b':')
                    .count();
            let tag_name_lower = ascii_lower_str(&bytes[name_start..name_end]);
            let tag_end = match memchr_byte(&bytes[i..], b'>') {
                Some(off) => i + off,
                None => break,
            };
            let tag_inner = &bytes[name_end..tag_end];

            match tag_name_lower.as_str() {
                "script" => in_script = !is_close,
                "style" => in_style = !is_close,
                _ => {}
            }

            if in_script || in_style {
                i = tag_end + 1;
                continue;
            }

            if !is_close {
                match tag_name_lower.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        let level = tag_name_lower.as_bytes()[1] - b'0';
                        block = Block::Heading(level);
                    }
                    "p" | "div" | "section" | "article" | "header" | "footer" | "nav" | "main"
                    | "aside" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        block = Block::Paragraph;
                    }
                    "pre" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        block = Block::Pre;
                    }
                    "blockquote" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        blockquote_depth += 1;
                        block = Block::Paragraph;
                    }
                    "ul" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        list_stack.push(ListKind::Ul);
                        block = Block::Paragraph;
                    }
                    "ol" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        list_stack.push(ListKind::Ol(1));
                        block = Block::Paragraph;
                    }
                    "li" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        // Emit the bullet/number for this list
                        // item directly into the output, then
                        // build the rest of the line in the
                        // inline buffer. Flush at </li>.
                        let depth = list_stack.len().saturating_sub(1);
                        let list_indent: String = std::iter::repeat_n("  ", depth).collect();
                        let bq: String = std::iter::repeat_n("> ", blockquote_depth).collect();
                        out.push_str(&bq);
                        out.push_str(&list_indent);
                        match list_stack.last_mut() {
                            Some(ListKind::Ul) | None => out.push_str("- "),
                            Some(ListKind::Ol(n_ref)) => {
                                out.push_str(&format!("{}. ", *n_ref));
                                *n_ref += 1;
                            }
                        }
                        block = Block::Paragraph;
                    }
                    "br" => {
                        // Soft break — two spaces + newline.
                        inline_buf.push_str("  \n");
                        inline_last_space = true;
                    }
                    "hr" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        let bq: String = std::iter::repeat_n("> ", blockquote_depth).collect();
                        out.push_str(&bq);
                        out.push_str("---\n\n");
                    }
                    "title" => {
                        // Skip <title>...</title> entirely — it's
                        // metadata, not body text.
                        let body_start = tag_end + 1;
                        if let Some(close_rel) = find_subslice_ci(&bytes[body_start..], b"</title>")
                        {
                            i = body_start + close_rel + b"</title>".len();
                            continue;
                        }
                    }
                    "a" => {
                        if let Some(href) = read_attr(tag_inner, b"href") {
                            let href = decode_entities(href.trim());
                            if !href.is_empty() {
                                inline_buf.push('[');
                                pending_href = Some((href, inline_buf.len()));
                            }
                        }
                    }
                    "img" => {
                        if let Some(src) = read_attr(tag_inner, b"src") {
                            let alt = read_attr(tag_inner, b"alt").unwrap_or("");
                            let src = decode_entities(src.trim());
                            let alt = decode_entities(alt.trim());
                            inline_buf.push_str(&format!("![{alt}]({src})"));
                            inline_last_space = false;
                        }
                    }
                    "strong" | "b" => {
                        inline_buf.push_str("**");
                        strong_open = strong_open.saturating_add(1);
                        inline_last_space = false;
                    }
                    "em" | "i" => {
                        inline_buf.push('*');
                        emphasis_open = emphasis_open.saturating_add(1);
                        inline_last_space = false;
                    }
                    "code" if !matches!(block, Block::Pre) => {
                        inline_buf.push('`');
                        code_inline_open = code_inline_open.saturating_add(1);
                        inline_last_space = false;
                    }
                    _ => {}
                }
            } else {
                match tag_name_lower.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "div" | "section"
                    | "article" | "header" | "footer" | "nav" | "main" | "aside" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        block = Block::Paragraph;
                    }
                    "pre" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        block = Block::Paragraph;
                    }
                    "blockquote" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        blockquote_depth = blockquote_depth.saturating_sub(1);
                    }
                    "ul" | "ol" => {
                        flush_block(
                            &mut out,
                            &mut inline_buf,
                            &mut inline_last_space,
                            &block,
                            &list_stack,
                            blockquote_depth,
                        );
                        list_stack.pop();
                    }
                    "li" => {
                        // Flush the line, but NOT with the
                        // list-item prefix this time — that was
                        // already emitted at <li>. Emit the inline
                        // buffer trimmed + newline.
                        let s = inline_buf.trim().to_string();
                        out.push_str(&s);
                        out.push('\n');
                        inline_buf.clear();
                        inline_last_space = true;
                        block = Block::Paragraph;
                    }
                    "a" => {
                        if let Some((href, _start)) = pending_href.take() {
                            inline_buf.push_str(&format!("]({href})"));
                            inline_last_space = false;
                        }
                    }
                    "strong" | "b" => {
                        if strong_open > 0 {
                            inline_buf.push_str("**");
                            strong_open -= 1;
                            inline_last_space = false;
                        }
                    }
                    "em" | "i" => {
                        if emphasis_open > 0 {
                            inline_buf.push('*');
                            emphasis_open -= 1;
                            inline_last_space = false;
                        }
                    }
                    "code" if !matches!(block, Block::Pre) => {
                        if code_inline_open > 0 {
                            inline_buf.push('`');
                            code_inline_open -= 1;
                            inline_last_space = false;
                        }
                    }
                    _ => {}
                }
            }

            i = tag_end + 1;
            continue;
        }

        // Plain text.
        if !in_script && !in_style {
            if b.is_ascii_whitespace() {
                if matches!(block, Block::Pre) {
                    // Inside <pre>, preserve whitespace verbatim.
                    let ch_len = utf8_char_len(b);
                    let end = (i + ch_len).min(n);
                    inline_buf.push_str(std::str::from_utf8(&bytes[i..end]).unwrap_or(""));
                    i = end;
                    inline_last_space = b == b' ' || b == b'\t';
                    continue;
                }
                if !inline_last_space {
                    inline_buf.push(' ');
                    inline_last_space = true;
                }
                i += 1;
            } else {
                if b == b'&' {
                    if let Some((decoded, consumed)) = decode_one_entity(&bytes[i..]) {
                        inline_buf.push_str(&decoded);
                        inline_last_space =
                            decoded.chars().last().is_some_and(|c| c.is_whitespace());
                        i += consumed;
                        continue;
                    }
                }
                let ch_len = utf8_char_len(b);
                let end = (i + ch_len).min(n);
                inline_buf.push_str(std::str::from_utf8(&bytes[i..end]).unwrap_or(""));
                inline_last_space = false;
                i = end;
            }
        } else {
            i += 1;
        }
    }

    // Final flush.
    flush_block(
        &mut out,
        &mut inline_buf,
        &mut inline_last_space,
        &block,
        &list_stack,
        blockquote_depth,
    );

    // Collapse runs of more than two blank lines.
    let mut tidied = String::with_capacity(out.len());
    let mut blank_run = 0usize;
    for line in out.lines() {
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                tidied.push('\n');
            }
        } else {
            blank_run = 0;
            tidied.push_str(line);
            tidied.push('\n');
        }
    }
    tidied.trim_end().to_string()
}

fn render_all(e: &Extracted) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "title={}", e.title.clone().unwrap_or_default());
    let _ = writeln!(s, "link_count={}", e.links.len());
    let _ = writeln!(s, "meta_count={}", e.meta.len());
    for (k, v) in &e.meta {
        let _ = writeln!(s, "meta:{k}={v}");
    }
    for u in &e.links {
        let _ = writeln!(s, "link={u}");
    }
    let _ = writeln!(s, "text=");
    s.push_str(&e.text);
    s
}

// ──────────────────────── Parsing helpers ──────────────────────────────────

fn memchr_byte(buf: &[u8], target: u8) -> Option<usize> {
    buf.iter().position(|&b| b == target)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn find_subslice_ci(hay: &[u8], needle_lower: &[u8]) -> Option<usize> {
    if needle_lower.is_empty() || needle_lower.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle_lower.len()).find(|&i| {
        hay[i..i + needle_lower.len()]
            .iter()
            .zip(needle_lower.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

fn ascii_lower_str(b: &[u8]) -> String {
    b.iter().map(|c| c.to_ascii_lowercase() as char).collect()
}

fn utf8_char_len(b: u8) -> usize {
    // ASCII and continuation bytes both length-1 here. Continuation
    // bytes shouldn't appear at the start of a char in valid UTF-8
    // (and the caller always uses this on a known leading byte), but
    // we coalesce the two cases so clippy doesn't trip on
    // if_same_then_else.
    if b < 0xC0 {
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

/// Collapse runs of ASCII whitespace into single spaces, trimming
/// nothing. Used for the title's inner text.
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out
}

/// Read attribute value from tag-attribute bytes. Case-insensitive on
/// the attribute name. Returns the *raw* (entity-encoded) value as
/// `&str`. Callers decode entities as needed.
fn read_attr<'a>(tag_inner: &'a [u8], name_lower: &[u8]) -> Option<&'a str> {
    // Walk attributes: <ws>name(=("value"|'value'|bareword))?
    let mut i = 0;
    let n = tag_inner.len();
    while i < n {
        // Skip whitespace.
        while i < n && tag_inner[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        // Read attribute name.
        let name_start = i;
        while i < n
            && !tag_inner[i].is_ascii_whitespace()
            && tag_inner[i] != b'='
            && tag_inner[i] != b'>'
            && tag_inner[i] != b'/'
        {
            i += 1;
        }
        let attr_name = &tag_inner[name_start..i];
        let name_matches = attr_name.len() == name_lower.len()
            && attr_name
                .iter()
                .zip(name_lower.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b));
        // Look for `=value`.
        let mut value: Option<&str> = None;
        if i < n && tag_inner[i] == b'=' {
            i += 1;
            // Skip whitespace after =.
            while i < n && tag_inner[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < n {
                if tag_inner[i] == b'"' || tag_inner[i] == b'\'' {
                    let q = tag_inner[i];
                    i += 1;
                    let v_start = i;
                    while i < n && tag_inner[i] != q {
                        i += 1;
                    }
                    if i <= n {
                        value = Some(std::str::from_utf8(&tag_inner[v_start..i]).unwrap_or(""));
                    }
                    if i < n {
                        i += 1;
                    }
                } else {
                    // Bareword value: until whitespace or `>`.
                    let v_start = i;
                    while i < n
                        && !tag_inner[i].is_ascii_whitespace()
                        && tag_inner[i] != b'>'
                        && tag_inner[i] != b'/'
                    {
                        i += 1;
                    }
                    value = Some(std::str::from_utf8(&tag_inner[v_start..i]).unwrap_or(""));
                }
            }
        }
        if name_matches {
            return value;
        }
    }
    None
}

/// Decode one HTML entity starting at position 0 of `bytes`. Returns
/// `(decoded_text, bytes_consumed)` or `None` if this isn't a
/// well-formed entity.
fn decode_one_entity(bytes: &[u8]) -> Option<(String, usize)> {
    if bytes.is_empty() || bytes[0] != b'&' {
        return None;
    }
    // Find the closing `;` within a sensible window.
    let max_lookahead = bytes.len().min(16);
    let end = (1..max_lookahead).find(|&i| bytes[i] == b';')?;
    let body = &bytes[1..end];
    let consumed = end + 1;
    // Numeric: &#NNN; or &#xHEX;
    if !body.is_empty() && body[0] == b'#' {
        let rest = &body[1..];
        let n: Option<u32> = if !rest.is_empty() && (rest[0] == b'x' || rest[0] == b'X') {
            let hex = std::str::from_utf8(&rest[1..]).ok()?;
            u32::from_str_radix(hex, 16).ok()
        } else {
            std::str::from_utf8(rest).ok()?.parse::<u32>().ok()
        };
        let c = n.and_then(char::from_u32)?;
        return Some((c.to_string(), consumed));
    }
    // Named entities — short list of the common ones.
    let s = std::str::from_utf8(body).ok()?;
    let decoded = match s {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{00A0}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "middot" => "·",
        "laquo" => "«",
        "raquo" => "»",
        "rsquo" => "’",
        "lsquo" => "‘",
        "rdquo" => "”",
        "ldquo" => "“",
        _ => return None,
    };
    Some((decoded.to_string(), consumed))
}

/// Public for tests + other modules that want the same decoder.
pub fn decode_entities(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some((dec, n)) = decode_one_entity(&bytes[i..])
        {
            out.push_str(&dec);
            i += n;
            continue;
        }
        let len = utf8_char_len(bytes[i]).max(1);
        let end = (i + len).min(bytes.len());
        out.push_str(std::str::from_utf8(&bytes[i..end]).unwrap_or(""));
        i = end;
    }
    out
}

// ──────────────────────── Error helpers ────────────────────────────────────

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

// ──────────────────────── Tests ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_extracted_case_insensitive() {
        let html = "<HTML><HEAD><Title>  Hello &amp; World  </Title></HEAD></HTML>";
        let e = extract(html);
        assert_eq!(e.title.as_deref(), Some("Hello & World"));
    }

    #[test]
    fn text_strips_tags_and_decodes_entities() {
        let html = "<p>Hello <b>bold</b> &amp; &lt;world&gt;</p>";
        let e = extract(html);
        assert_eq!(e.text, "Hello bold & <world>");
    }

    #[test]
    fn script_and_style_content_excluded_from_text() {
        let html = "<p>start</p><script>alert('xss')</script><style>p{}</style><p>end</p>";
        let e = extract(html);
        assert!(!e.text.contains("xss"));
        assert!(!e.text.contains("alert"));
        assert!(!e.text.contains("p{}"));
        assert!(e.text.contains("start"));
        assert!(e.text.contains("end"));
    }

    #[test]
    fn comments_are_skipped() {
        let html = "<p>before<!-- this is a <script>fake</script> comment -->after</p>";
        let e = extract(html);
        assert_eq!(e.text, "beforeafter");
    }

    #[test]
    fn links_extracted_in_order_deduplicated() {
        let html = r#"
            <a href="https://example.com/a">A</a>
            <a href="https://example.com/b">B</a>
            <a href="https://example.com/a">A again</a>
        "#;
        let e = extract(html);
        assert_eq!(
            e.links,
            vec![
                "https://example.com/a".to_string(),
                "https://example.com/b".to_string()
            ]
        );
    }

    #[test]
    fn meta_name_and_property_both_recognised() {
        let html = r#"
            <meta name="description" content="A test page">
            <meta property="og:title" content="OG title &amp; co">
            <meta http-equiv="refresh" content="0">
        "#;
        let e = extract(html);
        assert_eq!(e.meta.len(), 2);
        assert_eq!(
            e.meta[0],
            ("description".to_string(), "A test page".to_string())
        );
        assert_eq!(
            e.meta[1],
            ("og:title".to_string(), "OG title & co".to_string())
        );
    }

    #[test]
    fn numeric_entities_decoded() {
        assert_eq!(decode_entities("&#65;&#x41;"), "AA");
        assert_eq!(decode_entities("&#8230;"), "…");
    }

    #[test]
    fn malformed_html_does_not_panic() {
        // Unterminated tag, mismatched script.
        let _ = extract("<p>hello<broken<script>alert(");
        // Bare angle brackets.
        let _ = extract("<<<>>>");
        // Empty.
        let _ = extract("");
    }

    #[test]
    fn handler_rejects_oversize_input() {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        let cfg = WebExtractConfig {
            max_input_bytes: 10,
        };
        let html = "x".repeat(20);
        let arg = format!("text|{html}");
        let ctx = InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: arg.into_bytes(),
            tenant_id: None,
        };
        match handle(&cfg, &ctx) {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
                assert!(e.cause.contains("exceeds cap"));
            }
            HandlerOutcome::Ok(b) => {
                panic!("expected error, got ok with {} bytes", b.len())
            }
        }
    }

    #[test]
    fn handler_unknown_mode_rejected() {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        let cfg = WebExtractConfig::default();
        let ctx = InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: b"nope|<p>x</p>".to_vec(),
            tenant_id: None,
        };
        match handle(&cfg, &ctx) {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(b) => {
                panic!("expected invalid_args, got ok with {} bytes", b.len())
            }
        }
    }

    #[test]
    fn descriptor_is_idempotent_cheap_parse() {
        let d = capability_descriptor();
        assert_eq!(d.method_name, "tool.web_extract");
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "parse:html"));
    }

    #[test]
    fn render_all_contains_expected_keys() {
        let html = r#"<html><head><title>T</title><meta name="x" content="y"></head>
                       <body><p>Body text</p><a href="https://e.com/a">A</a></body></html>"#;
        let e = extract(html);
        let rendered = render_all(&e);
        assert!(rendered.contains("title=T"));
        assert!(rendered.contains("link_count=1"));
        assert!(rendered.contains("meta_count=1"));
        assert!(rendered.contains("meta:x=y"));
        assert!(rendered.contains("link=https://e.com/a"));
        assert!(rendered.contains("text="));
        assert!(rendered.contains("Body text"));
    }

    #[test]
    fn block_tags_produce_whitespace_boundaries() {
        // Adjacent <p> contents should not run together.
        let html = "<p>one</p><p>two</p>";
        let e = extract(html);
        assert_eq!(e.text, "one two");
    }

    // ── Track 6 hardening: parser doesn't panic on hostile inputs ──

    #[test]
    fn deeply_nested_tags_do_not_overflow_stack() {
        // 2000 nested <div>s. A naive recursive parser would blow
        // the stack; the alpha parser is iterative.
        let depth = 2000;
        let mut html = String::with_capacity(depth * 11);
        for _ in 0..depth {
            html.push_str("<div>");
        }
        html.push_str("payload");
        for _ in 0..depth {
            html.push_str("</div>");
        }
        let e = extract(&html);
        assert!(e.text.contains("payload"));
    }

    #[test]
    fn cdata_sections_are_skipped_entirely() {
        // CDATA is not legal in HTML5 outside foreign content
        // (SVG/MathML) but appears in scraped pages. The parser
        // skips the whole `<![CDATA[ ... ]]>` block — its body
        // never becomes text and any `<script>` inside is not
        // re-parsed as inline content.
        let html = "<p>before</p><![CDATA[ raw text ]]><p>after</p>";
        let e = extract(html);
        assert!(e.text.contains("before"));
        assert!(e.text.contains("after"));
        assert!(
            !e.text.contains("raw text"),
            "CDATA body leaked: {:?}",
            e.text
        );
    }

    #[test]
    fn script_inside_cdata_does_not_leak_payload() {
        // Hardening: even when a script tag is wrapped inside CDATA,
        // none of its payload should escape into the extracted text.
        // (The CDATA wrapper itself is skipped; this is belt-and-
        // suspenders that the operator-facing contract holds under
        // weird inputs.)
        let html = "<![CDATA[ <script>alert('xss')</script> ]]>";
        let e = extract(html);
        assert!(
            !e.text.contains("alert"),
            "script body leaked from inside CDATA: {:?}",
            e.text
        );
    }

    #[test]
    fn cdata_inside_real_content_doesnt_break_surrounding_text() {
        let html = "<p>A</p><![CDATA[ ignored <span>also ignored</span> ]]><p>B</p>";
        let e = extract(html);
        assert_eq!(e.text, "A B");
    }

    #[test]
    fn malformed_meta_tag_does_not_panic_or_corrupt_links() {
        let html = "<meta name=\"a content=\"unterminated><a href=\"https://ok\">x</a>";
        let e = extract(html);
        // Should not crash. May or may not extract the meta, but the
        // link should still be findable since it's well-formed.
        assert!(
            e.links.iter().any(|l| l.contains("https://ok")),
            "links: {:?}",
            e.links
        );
    }

    #[test]
    fn extremely_long_attribute_value_handled_cleanly() {
        // 1 MB attribute value. Tests the parser doesn't read it into
        // an unbounded buffer that defeats max_input_bytes downstream.
        let val = "x".repeat(1_000_000);
        let html = format!("<a href=\"https://e.com/?q={val}\">link</a>");
        let e = extract(&html);
        // Parser succeeds (it's the handler's job to bound input size,
        // not the parser's). The link IS captured.
        assert_eq!(e.links.len(), 1);
        assert!(e.links[0].starts_with("https://e.com/?q=x"));
    }

    #[test]
    fn self_closing_void_tags_dont_open_block_context() {
        // <br/> and <img/> should not produce text content of their
        // own; subsequent <p> still produces clean boundaries.
        let html = "<p>line1</p><br/><img src=\"x\"/><p>line2</p>";
        let e = extract(html);
        assert_eq!(e.text, "line1 line2");
    }

    #[test]
    fn html_entity_double_decoding_is_avoided() {
        // `&amp;lt;` should decode ONCE to `&lt;`, not twice to `<`.
        let html = "<p>&amp;lt;tag&amp;gt;</p>";
        let e = extract(html);
        assert!(
            e.text.contains("&lt;tag&gt;"),
            "double-decode regression: {:?}",
            e.text
        );
    }

    // ── PH-WEB-MARKDOWN: markdown mode ─────────────────────────────

    #[test]
    fn markdown_heading_emits_hash_prefix() {
        let md = extract_markdown("<h1>Title</h1>");
        assert_eq!(md, "# Title");
    }

    #[test]
    fn markdown_multiple_headings_keep_level() {
        let md = extract_markdown("<h2>A</h2><h3>B</h3>");
        assert!(md.contains("## A"));
        assert!(md.contains("### B"));
    }

    #[test]
    fn markdown_paragraph_emits_blank_line_separator() {
        let md = extract_markdown("<p>first</p><p>second</p>");
        assert!(md.contains("first\n\nsecond"), "got: {md:?}");
    }

    #[test]
    fn markdown_link_uses_bracket_paren_syntax() {
        let md = extract_markdown(r#"<p>see <a href="https://example.com">here</a></p>"#);
        assert!(md.contains("[here](https://example.com)"), "got: {md:?}");
    }

    #[test]
    fn markdown_strong_em_inline() {
        let md = extract_markdown("<p>this is <strong>bold</strong> and <em>italic</em></p>");
        assert!(md.contains("**bold**"));
        assert!(md.contains("*italic*"));
    }

    #[test]
    fn markdown_inline_code_uses_backticks() {
        let md = extract_markdown("<p>call <code>foo()</code> here</p>");
        assert!(md.contains("`foo()`"), "got: {md:?}");
    }

    #[test]
    fn markdown_pre_emits_fenced_code_block() {
        let md = extract_markdown("<pre>let x = 1;\nlet y = 2;</pre>");
        assert!(md.contains("```\nlet x = 1;\nlet y = 2;"), "got: {md:?}");
        assert!(md.contains("```"));
    }

    #[test]
    fn markdown_unordered_list_dashes() {
        let md = extract_markdown("<ul><li>one</li><li>two</li></ul>");
        assert!(md.contains("- one"), "got: {md:?}");
        assert!(md.contains("- two"), "got: {md:?}");
    }

    #[test]
    fn markdown_ordered_list_numbers() {
        let md = extract_markdown("<ol><li>one</li><li>two</li><li>three</li></ol>");
        assert!(md.contains("1. one"));
        assert!(md.contains("2. two"));
        assert!(md.contains("3. three"));
    }

    #[test]
    fn markdown_blockquote_prefix() {
        let md = extract_markdown("<blockquote><p>quoted</p></blockquote>");
        assert!(md.contains("> quoted"), "got: {md:?}");
    }

    #[test]
    fn markdown_hr_emits_three_dashes() {
        let md = extract_markdown("<p>a</p><hr><p>b</p>");
        assert!(md.contains("---"), "got: {md:?}");
    }

    #[test]
    fn markdown_img_emits_image_syntax() {
        let md = extract_markdown(r#"<p><img src="https://example.com/x.png" alt="x" /></p>"#);
        assert!(
            md.contains("![x](https://example.com/x.png)"),
            "got: {md:?}"
        );
    }

    #[test]
    fn markdown_strips_script_and_style() {
        let md = extract_markdown(
            "<script>alert('x')</script><style>p{color:red}</style><p>visible</p>",
        );
        assert!(!md.contains("alert"));
        assert!(!md.contains("color:red"));
        assert!(md.contains("visible"));
    }

    #[test]
    fn markdown_decodes_entities() {
        let md = extract_markdown("<p>5 &amp; 10 &lt; 100</p>");
        assert!(md.contains("5 & 10 < 100"), "got: {md:?}");
    }

    #[test]
    fn markdown_skips_title_metadata() {
        let md = extract_markdown("<title>page-title</title><h1>Body</h1><p>content</p>");
        assert!(!md.contains("page-title"));
        assert!(md.contains("Body"));
    }

    #[test]
    fn markdown_mode_through_handler() {
        let cfg = WebExtractConfig::default();
        // Build a minimal InvocationCtx.
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        let ctx = InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: b"markdown|<h1>Hello</h1><p>world</p>".to_vec(),
            tenant_id: None,
        };
        let out = match handle(&cfg, &ctx) {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert!(out.contains("# Hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn markdown_mode_listed_in_error_when_unknown() {
        let cfg = WebExtractConfig::default();
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        let ctx = InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: b"bogus|<p>x</p>".to_vec(),
            tenant_id: None,
        };
        match handle(&cfg, &ctx) {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("markdown")),
            _ => panic!("expected Err"),
        }
    }

    /// PH-RISK-PIN-ALL: tool.web_extract is a pure parser
    /// over caller-supplied bytes — no network surface, no
    /// host I/O. Safe tier.
    #[test]
    fn web_extract_descriptor_has_safe_risk() {
        let d = capability_descriptor();
        assert_ne!(d.risk_level, RiskLevel::Unknown);
        assert_eq!(d.risk_level, RiskLevel::Safe);
    }
}
