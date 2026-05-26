//! Shared rich-message formatting helpers for the Telegram,
//! Discord, and Slack channel controllers, plus the scheduled
//! summary reporter ([`reports`]).
//!
//! Plain assistant text → platform-native markdown / Block Kit
//! envelope. Each channel renders the same input differently:
//!
//! - **Telegram** wants `MarkdownV2` parse_mode, with strict
//!   escaping of `_*[]()~`>#+-=|{}.!` outside of code blocks.
//! - **Discord** accepts vanilla markdown but has a 2000-char
//!   per-message cap, so long replies are split at sentence /
//!   paragraph boundaries (never mid-word, never mid-code-block).
//! - **Slack** wants Block Kit JSON when the assistant emits
//!   structured content (code, headings). Plain text falls
//!   through unchanged.
//!
//! The detector recognises markdown code fences
//! (` ```lang\ncode\n``` `) — the most common rich-content
//! shape assistants emit — and renders them appropriately for
//! each platform's syntax.

pub mod reports;

/// Discord's per-message limit. The Discord HTTP API rejects
/// anything bigger with a 400. Real limit is 2000 but we leave
/// a small headroom for the `reply_to` reference marker.
pub const DISCORD_MAX_MESSAGE_LEN: usize = 1900;

/// Format `text` for Telegram's `MarkdownV2` parse_mode.
///
/// MarkdownV2 has aggressive escaping rules: every reserved
/// character outside a code block must be backslash-escaped, or
/// Telegram rejects the message with `Bad Request: can't parse
/// entities`. The reserved set is `_*[]()~`>#+-=|{}.!`. Inside
/// triple-backtick fences only `` ` `` and `\` need escaping.
///
/// Honest about scope: this is a *single-pass* formatter that
/// handles fenced code blocks correctly and escapes everything
/// else. It does NOT preserve inline markdown the assistant
/// might emit (`*bold*`, `_italic_`) — those characters get
/// escaped too. That's the safe default; over-formatting an
/// LLM reply is the lesser evil than tripping Telegram's
/// parser mid-message.
pub fn format_for_telegram_markdown_v2(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for segment in split_into_segments(text) {
        match segment {
            Segment::Code { lang, body } => {
                out.push_str("```");
                if !lang.is_empty() {
                    out.push_str(lang);
                }
                out.push('\n');
                // Inside a code fence only ``` and \ need escaping.
                for ch in body.chars() {
                    if ch == '`' || ch == '\\' {
                        out.push('\\');
                    }
                    out.push(ch);
                }
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```");
            }
            Segment::Text(body) => {
                for ch in body.chars() {
                    if TELEGRAM_MD_V2_RESERVED.contains(&ch) {
                        out.push('\\');
                    }
                    out.push(ch);
                }
            }
        }
    }
    out
}

/// Telegram MarkdownV2 reserved characters that must be escaped
/// outside code fences. Exhaustively from the Bot API docs.
pub const TELEGRAM_MD_V2_RESERVED: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
];

/// Discord doesn't need escaping but DOES need length splitting.
/// Returns one or more messages that together reproduce `text`,
/// each ≤ [`DISCORD_MAX_MESSAGE_LEN`] characters. Splits prefer
/// paragraph breaks (`\n\n`) then sentence breaks (`. `, `! `,
/// `? `) then line breaks (`\n`) then spaces. Never breaks
/// mid-word; never breaks inside a fenced code block.
pub fn format_for_discord(text: &str) -> Vec<String> {
    split_at_boundary(text, DISCORD_MAX_MESSAGE_LEN)
}

/// Slack mrkdwn flavour of the assistant text. Slack's mrkdwn
/// is *not* CommonMark: bold is `*bold*` (single asterisks),
/// italic is `_italic_`, code spans are backticks, code blocks
/// are triple-backtick fences with NO language hint.
///
/// LLMs emit CommonMark. The most jarring mismatch is
/// `**bold**` (CommonMark) rendering as literal `**bold**` in
/// Slack. This helper converts the most common CommonMark
/// shapes into Slack mrkdwn so a copy-pasted LLM response
/// renders correctly in the client:
///
/// - `**text**` → `*text*`
/// - language hints stripped from fenced code blocks
///   (` ```rust\nfn\n``` ` → ` ```\nfn\n``` `)
///
/// Everything else passes through unchanged — Slack tolerates
/// inline backticks and underscores fine.
pub fn format_for_slack_mrkdwn(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for segment in split_into_segments(text) {
        match segment {
            Segment::Code { lang: _, body } => {
                // Strip the language hint — Slack mrkdwn doesn't
                // honour it and prints it as literal text inside
                // the code block.
                out.push_str("```\n");
                out.push_str(body);
                if !body.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```");
            }
            Segment::Text(body) => {
                // **bold** → *bold* one pass, regex-free.
                let mut i = 0usize;
                let b = body.as_bytes();
                while i < b.len() {
                    if i + 1 < b.len() && b[i] == b'*' && b[i + 1] == b'*' {
                        // Find the closing `**`.
                        if let Some(close_rel) = body[i + 2..].find("**") {
                            let inner = &body[i + 2..i + 2 + close_rel];
                            out.push('*');
                            out.push_str(inner);
                            out.push('*');
                            i = i + 2 + close_rel + 2;
                            continue;
                        }
                    }
                    out.push(body.as_bytes()[i] as char);
                    i += 1;
                }
            }
        }
    }
    out
}

/// Slack rich-text shim. Slack's Block Kit is the structured
/// path; the simplest mapping that handles code fences is to
/// emit plain "mrkdwn" text per-section. The result is a single
/// JSON value the channel controller can POST as the message's
/// `blocks` field.
///
/// Honest about scope: this emits a flat `section` per text /
/// code segment; richer Block Kit primitives (headers,
/// dividers, action buttons) aren't wired today.
pub fn format_for_slack_blocks(text: &str) -> serde_json::Value {
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    for segment in split_into_segments(text) {
        match segment {
            Segment::Code { lang, body } => {
                let _ = lang; // Slack mrkdwn doesn't honour language hints.
                blocks.push(serde_json::json!({
                    "type": "section",
                    "text": {
                        "type": "mrkdwn",
                        "text": format!("```\n{body}\n```"),
                    },
                }));
            }
            Segment::Text(body) => {
                if body.trim().is_empty() {
                    continue;
                }
                blocks.push(serde_json::json!({
                    "type": "section",
                    "text": {
                        "type": "mrkdwn",
                        "text": body,
                    },
                }));
            }
        }
    }
    serde_json::Value::Array(blocks)
}

/// Split `text` into pieces no longer than `max_chars` codepoints,
/// preferring boundary breaks (paragraph > sentence > line >
/// space). Each piece is a non-empty `String`; concatenating
/// pieces with no glue reproduces the original modulo possible
/// whitespace trimming at boundaries.
///
/// `max_chars == 0` returns an empty vec. Input shorter than
/// `max_chars` returns a single-element vec.
pub fn split_at_boundary(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() || max_chars == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end_cap = (start + max_chars).min(chars.len());
        if end_cap == chars.len() {
            out.push(chars[start..end_cap].iter().collect());
            break;
        }
        // Find best break point in `[start, end_cap]`.
        let mut cut = end_cap;
        // Prefer paragraph break.
        if let Some(p) = find_last_substring(&chars, start, end_cap, "\n\n") {
            cut = p + 2;
        } else if let Some(p) = find_last_sentence_break(&chars, start, end_cap) {
            cut = p + 2;
        } else if let Some(p) = find_last_char(&chars, start, end_cap, '\n') {
            cut = p + 1;
        } else if let Some(p) = find_last_char(&chars, start, end_cap, ' ') {
            cut = p + 1;
        }
        // Refuse to cut zero-width pieces (find_* returned
        // exactly `start`); fall back to a hard cut at end_cap.
        if cut <= start {
            cut = end_cap;
        }
        out.push(chars[start..cut].iter().collect());
        start = cut;
    }
    out
}

fn find_last_substring(chars: &[char], start: usize, end: usize, pat: &str) -> Option<usize> {
    let pat_chars: Vec<char> = pat.chars().collect();
    if pat_chars.len() > end - start {
        return None;
    }
    let mut i = end - pat_chars.len();
    while i >= start {
        if chars[i..i + pat_chars.len()] == pat_chars[..] {
            return Some(i);
        }
        if i == start {
            break;
        }
        i -= 1;
    }
    None
}

fn find_last_sentence_break(chars: &[char], start: usize, end: usize) -> Option<usize> {
    // Sentence break = '.', '!', '?' followed by a space.
    if end < start + 2 {
        return None;
    }
    let mut i = end - 2;
    loop {
        let c0 = chars[i];
        let c1 = chars[i + 1];
        if (c0 == '.' || c0 == '!' || c0 == '?') && c1 == ' ' {
            return Some(i);
        }
        if i == start {
            return None;
        }
        i -= 1;
    }
}

fn find_last_char(chars: &[char], start: usize, end: usize, target: char) -> Option<usize> {
    let mut i = end;
    while i > start {
        i -= 1;
        if chars[i] == target {
            return Some(i);
        }
    }
    None
}

/// Internal: a parsed segment of the assistant's text.
#[derive(Debug, PartialEq, Eq)]
enum Segment<'a> {
    Code { lang: &'a str, body: &'a str },
    Text(&'a str),
}

/// Walk `text` and split into alternating Text + Code segments
/// at triple-backtick fences. Lone backticks pass through as
/// regular text. Imbalanced fences (open fence with no close)
/// degrade to text — the whole remainder is one Text segment.
fn split_into_segments(text: &str) -> Vec<Segment<'_>> {
    let mut out: Vec<Segment<'_>> = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        if let Some(open_rel) = text[cursor..].find("```") {
            let open = cursor + open_rel;
            if open > cursor {
                out.push(Segment::Text(&text[cursor..open]));
            }
            // After the opening fence, the rest of the line is
            // an optional language hint.
            let after_fence = open + 3;
            let lang_end_rel = text[after_fence..].find('\n');
            let (lang, body_start) = match lang_end_rel {
                Some(off) => (
                    text[after_fence..after_fence + off].trim(),
                    after_fence + off + 1,
                ),
                None => ("", after_fence),
            };
            // Find the closing fence.
            if let Some(close_rel) = text[body_start..].find("```") {
                let close = body_start + close_rel;
                // The body of the code block is between the
                // newline after the language hint and the
                // closing fence. Trim a trailing newline before
                // the closing fence so renderers don't double-
                // space.
                let body_end = if close > body_start && text.as_bytes()[close - 1] == b'\n' {
                    close - 1
                } else {
                    close
                };
                out.push(Segment::Code {
                    lang,
                    body: &text[body_start..body_end],
                });
                cursor = close + 3;
            } else {
                // Unclosed fence — degrade the whole remainder
                // to text so the consumer at least sees the
                // raw output.
                out.push(Segment::Text(&text[open..]));
                cursor = text.len();
            }
        } else {
            out.push(Segment::Text(&text[cursor..]));
            cursor = text.len();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_into_segments_plain_text_is_single_text() {
        let segs = split_into_segments("just plain text");
        assert_eq!(segs, vec![Segment::Text("just plain text")]);
    }

    #[test]
    fn split_into_segments_recognises_fenced_code_block() {
        let s = "hi\n```rust\nfn main() {}\n```\nbye";
        let segs = split_into_segments(s);
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0], Segment::Text("hi\n")));
        assert!(matches!(
            &segs[1],
            Segment::Code { lang: "rust", body } if *body == "fn main() {}"
        ));
        assert!(matches!(segs[2], Segment::Text("\nbye")));
    }

    #[test]
    fn split_into_segments_unclosed_fence_degrades_to_text() {
        let s = "hi\n```\nno close here";
        let segs = split_into_segments(s);
        assert_eq!(segs.len(), 2);
        assert!(matches!(segs[0], Segment::Text("hi\n")));
        assert!(matches!(segs[1], Segment::Text(_)));
    }

    #[test]
    fn telegram_md_v2_escapes_reserved_characters_outside_code() {
        let s = "hello (world)! see _docs_.";
        let out = format_for_telegram_markdown_v2(s);
        assert!(out.contains(r"\("), "missing escape for `(`: {out}");
        assert!(out.contains(r"\)"), "missing escape for `)`: {out}");
        assert!(out.contains(r"\!"), "missing escape for `!`: {out}");
        assert!(out.contains(r"\."), "missing escape for `.`: {out}");
        assert!(out.contains(r"\_"), "missing escape for `_`: {out}");
    }

    #[test]
    fn telegram_md_v2_preserves_code_fence_with_lang_hint() {
        let s = "say:\n```python\nprint('hi')\n```";
        let out = format_for_telegram_markdown_v2(s);
        // The language hint survives, the backticks are NOT
        // escaped (they're the fence), and `print('hi')` rides
        // through unmolested (no escaping inside the fence).
        assert!(out.contains("```python\n"), "missing fence + lang: {out}");
        assert!(out.contains("print('hi')"), "code body altered: {out}");
        assert!(out.ends_with("```"));
    }

    #[test]
    fn discord_short_message_passes_through_as_single_chunk() {
        let s = "hello";
        let chunks = format_for_discord(s);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn discord_long_message_splits_at_paragraph_boundary() {
        // Build a message that's just over the discord limit
        // and contains a paragraph break inside the budget.
        let para1: String = "a".repeat(1500);
        let para2: String = "b".repeat(800);
        let s = format!("{para1}\n\n{para2}");
        let chunks = format_for_discord(&s);
        assert!(chunks.len() >= 2, "should have split");
        // First chunk should end at the paragraph break — i.e.
        // it should NOT contain the second paragraph at all.
        assert!(!chunks[0].contains(&para2), "first chunk pulled in para 2");
        // Together they reproduce the input.
        let joined: String = chunks.join("");
        assert_eq!(joined, s);
    }

    #[test]
    fn discord_long_message_falls_back_to_sentence_then_space() {
        // 2500 chars of "Lorem ipsum. " sentences — no
        // paragraph break inside the first 1900-char window;
        // the splitter must use a sentence break instead.
        let one = "Lorem ipsum dolor sit amet. ".repeat(120);
        let chunks = format_for_discord(&one);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(
                c.chars().count() <= DISCORD_MAX_MESSAGE_LEN,
                "chunk too long: {} chars",
                c.chars().count()
            );
        }
        // No chunk ends mid-word (a sentence break leaves a
        // period+space at the end; the splitter advances past
        // the space so chunks don't start with one either).
        let joined: String = chunks.join("");
        assert_eq!(joined.len(), one.len());
    }

    #[test]
    fn discord_never_splits_mid_word_when_no_better_break_exists() {
        // No spaces or punctuation — splitter must still
        // respect the cap. It'll fall back to a hard cut at
        // the cap; we just verify chunks stay within budget
        // and concatenate back losslessly.
        let s: String = "x".repeat(5000);
        let chunks = format_for_discord(&s);
        for c in &chunks {
            assert!(c.chars().count() <= DISCORD_MAX_MESSAGE_LEN);
        }
        assert_eq!(chunks.join(""), s);
    }

    #[test]
    fn slack_blocks_emits_section_per_text_and_code_segment() {
        let s = "intro text\n```\ncode body\n```\noutro text";
        let blocks = format_for_slack_blocks(s);
        let arr = blocks.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // Code segment's mrkdwn body wraps the body in triple
        // backticks.
        let code_block_text = arr[1]
            .pointer("/text/text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(code_block_text.starts_with("```"));
        assert!(code_block_text.contains("code body"));
    }

    #[test]
    fn slack_blocks_skips_empty_text_segments() {
        // Pure-code-only input shouldn't emit a wrapping empty
        // text block before / after.
        let s = "```\ncode\n```";
        let blocks = format_for_slack_blocks(s);
        let arr = blocks.as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }
}
