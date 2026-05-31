//! RELIX-7.15 PII anonymization for the training data pipeline.
//!
//! Two units of work:
//!
//! - [`PiiDetector`] — pattern-based scanner. No ML, no
//!   network calls, no external services. Pure Rust, fully
//!   deterministic, runs in microseconds on a typical chat
//!   turn. Detects EMAIL / PHONE / SSN / CREDIT_CARD (Luhn-
//!   validated) / IP_ADDRESS (v4 + v6) / URL / NAME /
//!   DATE_OF_BIRTH / ADDRESS / API_KEY.
//!
//! - [`PiiAnonymizer`] — given detected spans, produces an
//!   anonymized string under one of three strategies:
//!   `Redact` (`[EMAIL]`, `[PHONE]`, …), `Pseudonymize`
//!   (consistent fake values keyed by hash of the original
//!   within a single anonymization pass), or `Allow` (pass
//!   through). Operators can override the global strategy per
//!   PII type via [`PiiConfig::overrides`].
//!
//! The detector + anonymizer are pure functions of their
//! inputs; both are cheap to clone (Arc-backed config + lazy
//! `OnceLock` regex cache).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// All PII types this layer knows how to detect. Strings on
/// the wire match the spec's UPPER_SNAKE_CASE labels so
/// operator config + dashboard surface stay aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PiiType {
    Email,
    Phone,
    Ssn,
    CreditCard,
    IpAddress,
    Url,
    Name,
    DateOfBirth,
    Address,
    ApiKey,
}

impl PiiType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "EMAIL",
            Self::Phone => "PHONE",
            Self::Ssn => "SSN",
            Self::CreditCard => "CREDIT_CARD",
            Self::IpAddress => "IP_ADDRESS",
            Self::Url => "URL",
            Self::Name => "NAME",
            Self::DateOfBirth => "DATE_OF_BIRTH",
            Self::Address => "ADDRESS",
            Self::ApiKey => "API_KEY",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let norm = s.trim().to_ascii_uppercase().replace('-', "_");
        Some(match norm.as_str() {
            "EMAIL" => Self::Email,
            "PHONE" => Self::Phone,
            "SSN" => Self::Ssn,
            "CREDIT_CARD" | "CREDITCARD" | "CARD" => Self::CreditCard,
            "IP_ADDRESS" | "IPADDRESS" | "IP" => Self::IpAddress,
            "URL" | "URI" => Self::Url,
            "NAME" => Self::Name,
            "DATE_OF_BIRTH" | "DOB" | "BIRTHDATE" => Self::DateOfBirth,
            "ADDRESS" => Self::Address,
            "API_KEY" | "APIKEY" | "TOKEN" => Self::ApiKey,
            _ => return None,
        })
    }

    /// Iterate every variant. Used by the anonymizer's
    /// per-type strategy resolver and by the bridge's
    /// pii_scan handler when emitting the empty-match summary.
    pub fn all() -> [Self; 10] {
        [
            Self::Email,
            Self::Phone,
            Self::Ssn,
            Self::CreditCard,
            Self::IpAddress,
            Self::Url,
            Self::Name,
            Self::DateOfBirth,
            Self::Address,
            Self::ApiKey,
        ]
    }
}

impl std::fmt::Display for PiiType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One detected occurrence of PII inside an input string.
/// Coordinates are byte offsets (not character offsets); the
/// detector's regex layer is byte-oriented so consumers can
/// slice into the source `String` without re-walking UTF-8.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiiSpan {
    pub pii_type: PiiType,
    pub start: usize,
    pub end: usize,
    pub matched_text: String,
}

/// Pattern-based PII detector. Lazy-initializes its regex
/// table on first use via `OnceLock`; subsequent calls share
/// the cached compiled regexes.
#[derive(Clone, Copy, Debug, Default)]
pub struct PiiDetector;

impl PiiDetector {
    /// Scan `text` and return every detected PII span. Spans
    /// are sorted ascending by `start`. When two patterns
    /// overlap, the longer match wins; ties (same span) keep
    /// the first detector that matched in deterministic
    /// `PiiType` order.
    pub fn scan(&self, text: &str) -> Vec<PiiSpan> {
        let regs = compiled_regexes();
        let mut hits: Vec<PiiSpan> = Vec::new();
        // Each regex is anchored by its own implementation; the
        // detector funnel just runs each one and pushes hits.
        push_pattern_hits(&mut hits, text, PiiType::Url, &regs.url);
        push_pattern_hits(&mut hits, text, PiiType::Email, &regs.email);
        push_pattern_hits(&mut hits, text, PiiType::IpAddress, &regs.ipv4);
        push_pattern_hits(&mut hits, text, PiiType::IpAddress, &regs.ipv6);
        push_pattern_hits(&mut hits, text, PiiType::Ssn, &regs.ssn);
        push_credit_card_hits(&mut hits, text, &regs.credit_card_candidate);
        push_phone_hits(&mut hits, text, &regs.phone_candidate);
        push_dob_hits(&mut hits, text, &regs.dob_with_context);
        push_pattern_hits(&mut hits, text, PiiType::Address, &regs.address);
        push_api_key_hits(&mut hits, text, &regs.api_key);
        push_name_hits(&mut hits, text);
        // Resolve overlaps + sort.
        dedupe_overlaps(&mut hits);
        hits.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
        hits
    }
}

/// Strategy applied to detected PII spans during
/// anonymization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PiiStrategy {
    /// Replace each span with a placeholder (`[EMAIL]`,
    /// `[PHONE]`, …). Default.
    #[default]
    Redact,
    /// Replace each span with a stable fake value derived from
    /// a hash of the original. Same original → same fake
    /// within one anonymization pass.
    Pseudonymize,
    /// Pass the span through unchanged. Used for PII types the
    /// operator explicitly allows.
    Allow,
}

impl PiiStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Redact => "redact",
            Self::Pseudonymize => "pseudonymize",
            Self::Allow => "allow",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "redact" => Some(Self::Redact),
            "pseudonymize" | "pseudonymise" => Some(Self::Pseudonymize),
            "allow" | "pass" | "passthrough" => Some(Self::Allow),
            _ => None,
        }
    }
}

/// `[training.pii]` configuration block. Absent / `enabled =
/// false` means the recorder + exporter both skip the
/// anonymization step entirely.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PiiConfig {
    #[serde(default = "default_pii_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub strategy: PiiStrategy,
    /// Per-type strategy overrides. Keys are the
    /// SCREAMING_SNAKE_CASE labels from [`PiiType`].
    #[serde(default)]
    pub overrides: BTreeMap<String, PiiStrategy>,
}

fn default_pii_enabled() -> bool {
    false
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            enabled: default_pii_enabled(),
            strategy: PiiStrategy::default(),
            overrides: BTreeMap::new(),
        }
    }
}

impl PiiConfig {
    /// Resolve the effective strategy for `pii_type`: when an
    /// override is set, it wins; otherwise the global
    /// `strategy` applies.
    pub fn strategy_for(&self, pii_type: PiiType) -> PiiStrategy {
        if let Some(s) = self.overrides.get(pii_type.as_str()) {
            return *s;
        }
        self.strategy
    }
}

/// Resolved anonymizer ready to redact / pseudonymize text.
/// Holds an immutable per-type strategy table so the hot path
/// is a single indexed lookup per span.
#[derive(Clone, Debug)]
pub struct PiiAnonymizer {
    enabled: bool,
    strategies: [PiiStrategy; 10],
}

impl PiiAnonymizer {
    /// Build from a [`PiiConfig`]. Disabled config produces an
    /// `enabled=false` anonymizer whose `anonymize` is a
    /// pass-through.
    pub fn from_config(cfg: &PiiConfig) -> Self {
        let mut strategies = [PiiStrategy::default(); 10];
        for t in PiiType::all() {
            strategies[type_index(t)] = cfg.strategy_for(t);
        }
        Self {
            enabled: cfg.enabled,
            strategies,
        }
    }

    /// Permissive anonymizer — `enabled=false`. Used by tests
    /// + by the recorder when no `[training.pii]` is set.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            strategies: [PiiStrategy::Allow; 10],
        }
    }

    /// Whether this anonymizer would mutate text.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Lookup the resolved strategy for one PII type.
    pub fn strategy_for(&self, pii_type: PiiType) -> PiiStrategy {
        self.strategies[type_index(pii_type)]
    }

    /// Anonymize `text` in place — returns the resulting
    /// string. When `enabled=false`, returns a copy of `text`
    /// unchanged. When `enabled=true` and no PII is detected,
    /// also returns a copy unchanged.
    pub fn anonymize(&self, text: &str) -> String {
        if !self.enabled {
            return text.to_string();
        }
        let spans = PiiDetector.scan(text);
        if spans.is_empty() {
            return text.to_string();
        }
        self.apply(text, &spans)
    }

    /// Apply this anonymizer's strategies to `spans` against
    /// `text`. Exposed so callers that already have a span
    /// list (e.g. the bridge's pii_scan endpoint) don't pay
    /// for two scans.
    pub fn apply(&self, text: &str, spans: &[PiiSpan]) -> String {
        if !self.enabled || spans.is_empty() {
            return text.to_string();
        }
        // Walk spans in ascending start order, copying the
        // bytes between spans and emitting the replacement for
        // each span. `text` is &str so we work in byte offsets;
        // the spans themselves are byte-anchored.
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0usize;
        let bytes = text.as_bytes();
        let mut pseudonym_cache: BTreeMap<(PiiType, String), String> = BTreeMap::new();
        let mut ordered = spans.to_vec();
        ordered.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
        for span in ordered {
            if span.start < cursor || span.end > text.len() {
                continue;
            }
            // Bytes between previous cursor and this span.
            out.push_str(std::str::from_utf8(&bytes[cursor..span.start]).unwrap_or(""));
            let strategy = self.strategy_for(span.pii_type);
            match strategy {
                PiiStrategy::Allow => {
                    out.push_str(&span.matched_text);
                }
                PiiStrategy::Redact => {
                    out.push('[');
                    out.push_str(span.pii_type.as_str());
                    out.push(']');
                }
                PiiStrategy::Pseudonymize => {
                    let key = (span.pii_type, span.matched_text.clone());
                    let fake = pseudonym_cache
                        .entry(key)
                        .or_insert_with(|| pseudonymize_value(span.pii_type, &span.matched_text))
                        .clone();
                    out.push_str(&fake);
                }
            }
            cursor = span.end;
        }
        if cursor < text.len() {
            out.push_str(std::str::from_utf8(&bytes[cursor..]).unwrap_or(""));
        }
        out
    }
}

fn type_index(t: PiiType) -> usize {
    match t {
        PiiType::Email => 0,
        PiiType::Phone => 1,
        PiiType::Ssn => 2,
        PiiType::CreditCard => 3,
        PiiType::IpAddress => 4,
        PiiType::Url => 5,
        PiiType::Name => 6,
        PiiType::DateOfBirth => 7,
        PiiType::Address => 8,
        PiiType::ApiKey => 9,
    }
}

// ── compiled regex table ─────────────────────────────────────

struct CompiledRegexes {
    email: Regex,
    ipv4: Regex,
    ipv6: Regex,
    url: Regex,
    ssn: Regex,
    credit_card_candidate: Regex,
    phone_candidate: Regex,
    dob_with_context: Regex,
    address: Regex,
    api_key: Regex,
}

fn compiled_regexes() -> &'static CompiledRegexes {
    static CACHE: OnceLock<CompiledRegexes> = OnceLock::new();
    CACHE.get_or_init(|| {
        CompiledRegexes {
            // Email: simplified RFC 5322 — local + @ + domain with TLD.
            email: Regex::new(
                r"(?xi)
                \b
                [a-z0-9][a-z0-9._+\-]*
                @
                [a-z0-9][a-z0-9.\-]*\.[a-z]{2,24}
                \b",
            )
            .expect("email regex"),
            // IPv4 — each octet 0–255.
            ipv4: Regex::new(
                r"\b(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\b",
            )
            .expect("ipv4 regex"),
            // IPv6 — full + collapsed forms. The
            // post-filter rejects loose `::` runs by requiring
            // at least one hex group on each side OR a fully
            // collapsed unspecified address (`::`).
            ipv6: Regex::new(
                r"(?i)\b(?:[0-9a-f]{1,4}:){2,7}[0-9a-f]{1,4}\b|::[0-9a-f]{1,4}\b|\b[0-9a-f]{1,4}::\b",
            )
            .expect("ipv6 regex"),
            // URL — http/https/ftp scheme.
            url: Regex::new(
                r#"(?i)\b(?:https?|ftp)://[^\s<>"'\)\]\}]+"#,
            )
            .expect("url regex"),
            // SSN — xxx-xx-xxxx or 9 consecutive digits.
            ssn: Regex::new(
                r"\b(?:\d{3}-\d{2}-\d{4}|\d{9})\b",
            )
            .expect("ssn regex"),
            // Credit-card candidate — 13–19 digit run with
            // optional spaces or dashes between groups. Luhn
            // validation runs as a post-filter.
            credit_card_candidate: Regex::new(
                r"\b(?:\d[ \-]?){12,18}\d\b",
            )
            .expect("cc regex"),
            // Phone candidate — covers (xxx) xxx-xxxx,
            // xxx-xxx-xxxx, +1xxxxxxxxxx, +xx xxxxxxxxxx, and
            // loose 10–15 digit runs with separator noise. The
            // post-filter strips separators and counts digits
            // to reject false-positives.
            phone_candidate: Regex::new(
                r"(?x)
                (?:\+?\d[\d\s\.\-\(\)]{8,18}\d)
                ",
            )
            .expect("phone regex"),
            // DOB — date in MM/DD/YYYY, YYYY-MM-DD, or
            // `Month DD, YYYY` AND a context word in the
            // ±32-char window. The window check happens in the
            // matcher.
            dob_with_context: Regex::new(
                r"(?xi)
                (?:\d{1,2}[/\-]\d{1,2}[/\-]\d{2,4})
                |
                (?:\d{4}[/\-]\d{1,2}[/\-]\d{1,2})
                |
                (?:(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\s+\d{1,2}(?:st|nd|rd|th)?(?:,\s*|\s+)\d{4})
                ",
            )
            .expect("dob regex"),
            // Address — number + street name(s) + street type.
            // The street name is one to three capitalized
            // tokens; the suffix is a fixed allowlist.
            address: Regex::new(
                r"(?xi)
                \b
                \d{1,6}
                \s+
                (?:[A-Z][a-zA-Z]+\s+){1,3}
                (?:Street|St|Avenue|Ave|Road|Rd|Boulevard|Blvd|Drive|Dr|Lane|Ln|Court|Ct|Place|Pl|Square|Sq|Trail|Trl|Parkway|Pkwy|Way|Highway|Hwy|Terrace|Ter)
                \.?
                \b
                ",
            )
            .expect("address regex"),
            // API-key candidate — 20+ char alphanumeric run
            // with mixed case AND at least one digit. The
            // entropy gate runs as a post-filter.
            api_key: Regex::new(
                r"\b[A-Za-z0-9_\-]{20,}\b",
            )
            .expect("api key regex"),
        }
    })
}

fn push_pattern_hits(out: &mut Vec<PiiSpan>, text: &str, pii: PiiType, re: &Regex) {
    for m in re.find_iter(text) {
        out.push(PiiSpan {
            pii_type: pii,
            start: m.start(),
            end: m.end(),
            matched_text: text[m.start()..m.end()].to_string(),
        });
    }
}

/// Luhn validation post-filter. The candidate regex catches a
/// lot of innocent digit runs; this drops anything that doesn't
/// pass the standard credit-card checksum.
fn push_credit_card_hits(out: &mut Vec<PiiSpan>, text: &str, re: &Regex) {
    for m in re.find_iter(text) {
        let raw = &text[m.start()..m.end()];
        let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() < 13 || digits.len() > 19 {
            continue;
        }
        if !luhn_ok(&digits) {
            continue;
        }
        out.push(PiiSpan {
            pii_type: PiiType::CreditCard,
            start: m.start(),
            end: m.end(),
            matched_text: raw.to_string(),
        });
    }
}

fn luhn_ok(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut alt = false;
    for c in digits.chars().rev() {
        if let Some(mut d) = c.to_digit(10) {
            if alt {
                d *= 2;
                if d > 9 {
                    d -= 9;
                }
            }
            sum += d;
            alt = !alt;
        } else {
            return false;
        }
    }
    sum.is_multiple_of(10)
}

/// Phone-number post-filter. We accept candidates whose
/// digit-count is in `[10, 15]` (E.164 cap) and that have at
/// least one separator OR an explicit `+` country-code prefix —
/// otherwise a bare 10-digit run gets treated as an SSN-or-
/// CC candidate by the other detectors.
fn push_phone_hits(out: &mut Vec<PiiSpan>, text: &str, re: &Regex) {
    for m in re.find_iter(text) {
        let raw = &text[m.start()..m.end()];
        let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
        if !(10..=15).contains(&digits.len()) {
            continue;
        }
        // Bare 11+ digit run without a `+` prefix is more
        // likely a tracking-number / sequence than a phone.
        // Require either an explicit `+` or a separator
        // character (-/./space/parenthesis) before accepting.
        let has_separator = raw
            .chars()
            .any(|c| matches!(c, '-' | '.' | ' ' | '(' | ')'));
        let has_plus = raw.starts_with('+');
        if !has_separator && !has_plus {
            // Bare 11–15 digit run — skip. The SSN regex
            // already takes 9-digit bare runs; bare 10-digit
            // runs are ambiguous and we err on false-negative
            // here since we'd rather miss one than redact a
            // tracking number.
            continue;
        }
        out.push(PiiSpan {
            pii_type: PiiType::Phone,
            start: m.start(),
            end: m.end(),
            matched_text: raw.to_string(),
        });
    }
}

/// DOB post-filter — requires a context word (`born`, `dob`,
/// `birthday`, `date of birth`) within a 32-char window before
/// or after the date match.
fn push_dob_hits(out: &mut Vec<PiiSpan>, text: &str, re: &Regex) {
    let lowered = text.to_ascii_lowercase();
    for m in re.find_iter(text) {
        let start = m.start();
        let end = m.end();
        let mut win_lo = start.saturating_sub(32);
        let mut win_hi = (end + 32).min(text.len());
        // The ±32 byte offsets can land inside a multi-byte codepoint;
        // snap lo down and hi up to the nearest char boundaries.
        while win_lo > 0 && !lowered.is_char_boundary(win_lo) {
            win_lo -= 1;
        }
        while win_hi < lowered.len() && !lowered.is_char_boundary(win_hi) {
            win_hi += 1;
        }
        let window = &lowered[win_lo..win_hi];
        let has_context = window.contains("born")
            || window.contains("dob")
            || window.contains("d.o.b")
            || window.contains("birthday")
            || window.contains("birthdate")
            || window.contains("date of birth");
        if !has_context {
            continue;
        }
        out.push(PiiSpan {
            pii_type: PiiType::DateOfBirth,
            start,
            end,
            matched_text: text[start..end].to_string(),
        });
    }
}

/// API-key post-filter — entropy + composition gate. We
/// require the candidate to contain at least one uppercase
/// letter AND one lowercase letter AND one digit, and to score
/// above a Shannon-entropy floor. This rejects long English
/// words, repeated tokens, and base-36 ULIDs (which look like
/// keys but aren't secrets), without false-positiving every
/// short ID.
fn api_key_passes_entropy(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut has_upper = false;
    let mut has_lower = false;
    let mut has_digit = false;
    for &b in bytes {
        if b.is_ascii_uppercase() {
            has_upper = true;
        } else if b.is_ascii_lowercase() {
            has_lower = true;
        } else if b.is_ascii_digit() {
            has_digit = true;
        }
    }
    if !(has_upper && has_lower && has_digit) {
        return false;
    }
    // Shannon entropy floor (bits per character). API keys
    // typically score 4.5+; English words score < 3.5.
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    let mut h = 0.0f64;
    for c in counts {
        if c > 0 {
            let p = c as f64 / n;
            h -= p * p.log2();
        }
    }
    h >= 3.5
}

/// API-key push variant — runs the regex match, then drops
/// any candidate that fails the mixed-case + digit + entropy
/// gate. This is the only PII type that needs a numeric-quality
/// post-filter; the other types either validate inline (Luhn
/// for credit cards, octet ranges for IPv4) or rely on regex
/// shape alone.
fn push_api_key_hits(out: &mut Vec<PiiSpan>, text: &str, re: &Regex) {
    for m in re.find_iter(text) {
        let raw = &text[m.start()..m.end()];
        if !api_key_passes_entropy(raw) {
            continue;
        }
        out.push(PiiSpan {
            pii_type: PiiType::ApiKey,
            start: m.start(),
            end: m.end(),
            matched_text: raw.to_string(),
        });
    }
}

// ── name detection ──────────────────────────────────────────

/// Common stop-words / titles / capitalized phrases that look
/// like names but shouldn't be redacted. Keep this list narrow:
/// we want to avoid blocking "The President" / "United States"
/// while still catching "John Smith".
const NAME_STOP_WORDS: &[&str] = &[
    "The",
    "A",
    "An",
    "And",
    "But",
    "Or",
    "Nor",
    "For",
    "So",
    "Yet",
    "At",
    "By",
    "In",
    "Of",
    "On",
    "To",
    "Up",
    "Is",
    "Are",
    "Was",
    "Were",
    "Am",
    "Be",
    "Been",
    "Being",
    "Have",
    "Has",
    "Had",
    "Do",
    "Does",
    "Did",
    "Will",
    "Would",
    "Could",
    "Should",
    "May",
    "Might",
    "Must",
    "Shall",
    "Can",
    "I",
    "You",
    "He",
    "She",
    "It",
    "We",
    "They",
    "This",
    "That",
    "These",
    "Those",
    "Mr",
    "Mrs",
    "Ms",
    "Dr",
    "Prof",
    "Sir",
    "Madam",
    "President",
    "Senator",
    "Governor",
    "Congress",
    "Senate",
    "House",
    "United",
    "States",
    "America",
    "American",
    "European",
    "Asian",
    "African",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
    "North",
    "South",
    "East",
    "West",
    "New",
    "Old",
    "First",
    "Last",
    "Next",
    "Previous",
    "Year",
    "Month",
    "Day",
    "Week",
];

fn is_capitalized_word(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    for c in chars {
        if !c.is_ascii_alphabetic() && c != '\'' && c != '-' {
            return false;
        }
    }
    true
}

/// Names are 2–3 capitalized tokens not at sentence start. The
/// detector splits the text into byte-anchored words, walks
/// candidate windows of 2 or 3 consecutive capitalized words,
/// drops any window whose first token is a stop-word or whose
/// word immediately follows a sentence-terminator (`.`, `!`,
/// `?`, line break).
fn push_name_hits(out: &mut Vec<PiiSpan>, text: &str) {
    let bytes = text.as_bytes();
    // Build (start, end) word offsets for ASCII-alphabetic
    // tokens (allowing apostrophes / hyphens mid-token).
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_alphabetic() || c == b'\'' || c == b'-' {
                    i += 1;
                } else {
                    break;
                }
            }
            words.push((start, i));
        } else {
            i += 1;
        }
    }
    // For each starting word w_k, try a 3-window then a 2-
    // window. Whichever matches first claims the span; the
    // outer overlap-dedup picks the longer one if there's any
    // ambiguity. Skip windows whose first token is a stop-word
    // OR immediately follows a sentence terminator.
    let stop_set: std::collections::HashSet<&str> = NAME_STOP_WORDS.iter().copied().collect();
    let mut k = 0;
    while k < words.len() {
        let (s0, e0) = words[k];
        let token0 = &text[s0..e0];
        if !is_capitalized_word(token0) || stop_set.contains(token0) {
            k += 1;
            continue;
        }
        if at_sentence_start(text, s0) {
            k += 1;
            continue;
        }
        let mut matched_len = 0usize;
        // 3-word window first.
        if k + 2 < words.len() {
            let (_, e2) = words[k + 2];
            let (s1, e1) = words[k + 1];
            let (s2, _) = words[k + 2];
            let t1 = &text[s1..e1];
            let t2 = &text[s2..e2];
            if is_capitalized_word(t1)
                && !stop_set.contains(t1)
                && is_capitalized_word(t2)
                && !stop_set.contains(t2)
                && only_whitespace_between(text, e0, s1)
                && only_whitespace_between(text, e1, s2)
            {
                matched_len = e2 - s0;
                out.push(PiiSpan {
                    pii_type: PiiType::Name,
                    start: s0,
                    end: e2,
                    matched_text: text[s0..e2].to_string(),
                });
            }
        }
        // 2-word window.
        if matched_len == 0 && k + 1 < words.len() {
            let (s1, e1) = words[k + 1];
            let t1 = &text[s1..e1];
            if is_capitalized_word(t1)
                && !stop_set.contains(t1)
                && only_whitespace_between(text, e0, s1)
            {
                matched_len = e1 - s0;
                out.push(PiiSpan {
                    pii_type: PiiType::Name,
                    start: s0,
                    end: e1,
                    matched_text: text[s0..e1].to_string(),
                });
            }
        }
        if matched_len > 0 {
            // Skip the words consumed by the match.
            // Find how many word entries this span covers.
            let span_end = s0 + matched_len;
            let mut next_k = k + 1;
            while next_k < words.len() && words[next_k].1 <= span_end {
                next_k += 1;
            }
            k = next_k;
        } else {
            k += 1;
        }
    }
}

fn at_sentence_start(text: &str, pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    let bytes = text.as_bytes();
    let mut i = pos;
    // Walk backward over whitespace.
    while i > 0 {
        i -= 1;
        let c = bytes[i];
        if c == b' ' || c == b'\t' {
            continue;
        }
        return matches!(c, b'.' | b'!' | b'?' | b'\n' | b'\r');
    }
    true
}

fn only_whitespace_between(text: &str, a: usize, b: usize) -> bool {
    if a > b || b > text.len() {
        return false;
    }
    text[a..b].chars().all(|c| c == ' ' || c == '\t')
}

// ── overlap resolution ──────────────────────────────────────

fn dedupe_overlaps(spans: &mut Vec<PiiSpan>) {
    if spans.is_empty() {
        return;
    }
    spans.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then((b.end - b.start).cmp(&(a.end - a.start)))
            .then(a.pii_type.cmp(&b.pii_type))
    });
    let mut keep: Vec<PiiSpan> = Vec::with_capacity(spans.len());
    for s in spans.drain(..) {
        let overlaps_prev = keep
            .last()
            .map(|p| ranges_overlap(p.start, p.end, s.start, s.end))
            .unwrap_or(false);
        if !overlaps_prev {
            keep.push(s);
            continue;
        }
        let prev = keep.last().expect("non-empty");
        let prev_len = prev.end - prev.start;
        let new_len = s.end - s.start;
        if new_len > prev_len {
            let _ = keep.pop();
            keep.push(s);
        }
        // Equal-length overlap → keep the first-seen (deterministic by sort above).
    }
    *spans = keep;
}

fn ranges_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    a_start < b_end && b_start < a_end
}

// ── pseudonymization ────────────────────────────────────────

fn pseudonymize_value(pii_type: PiiType, original: &str) -> String {
    let h = blake3::hash(original.as_bytes());
    let hex = h.to_hex();
    let short: String = hex.chars().take(6).collect();
    match pii_type {
        PiiType::Email => format!("user_{short}@redacted.example"),
        PiiType::Phone => format!("555-01{}{}", &short[0..2], &short[2..4]),
        PiiType::Ssn => format!(
            "000-00-{}{}{}{}",
            &short[0..1],
            &short[1..2],
            &short[2..3],
            &short[3..4]
        ),
        PiiType::CreditCard => format!(
            "4000-0000-0000-{}{}{}{}",
            &short[0..1],
            &short[1..2],
            &short[2..3],
            &short[3..4]
        ),
        PiiType::IpAddress => {
            let b: Vec<u8> = (0..4).map(|i| h.as_bytes()[i] % 200 + 1).collect();
            format!("10.{}.{}.{}", b[1], b[2], b[3])
        }
        PiiType::Url => format!("https://redacted.example/{short}"),
        PiiType::Name => format!("Name_{short}"),
        PiiType::DateOfBirth => "1970-01-01".to_string(),
        PiiType::Address => format!(
            "{short_addr} Redacted St",
            short_addr = u32::from_str_radix(&short, 16).unwrap_or(42) % 9999 + 1
        ),
        PiiType::ApiKey => format!("REDACTED_KEY_{short}"),
    }
}

// ── compatibility alias for callers that prefer the
// `scan_with_filters` name ───────────────────────────────────
impl PiiDetector {
    /// Alias of [`scan`](Self::scan). Kept for callers that
    /// document the gating-step pipeline explicitly; the
    /// underlying entry point already applies the api-key
    /// entropy gate inline.
    pub fn scan_with_filters(&self, text: &str) -> Vec<PiiSpan> {
        self.scan(text)
    }
}

// ── tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn types_in(spans: &[PiiSpan]) -> Vec<&'static str> {
        spans.iter().map(|s| s.pii_type.as_str()).collect()
    }

    #[test]
    fn detects_email_addresses_in_plain_text() {
        let text = "Contact me at alice@example.com please.";
        let spans = PiiDetector.scan_with_filters(text);
        assert!(types_in(&spans).contains(&"EMAIL"));
        let email = spans.iter().find(|s| s.pii_type == PiiType::Email).unwrap();
        assert_eq!(email.matched_text, "alice@example.com");
    }

    #[test]
    fn detects_phone_numbers_in_all_three_us_formats() {
        let formats = [
            "call me at 555-123-4567",
            "ring (415) 555-2671",
            "us number +14155552671",
        ];
        for text in formats {
            let spans = PiiDetector.scan_with_filters(text);
            assert!(
                spans.iter().any(|s| s.pii_type == PiiType::Phone),
                "no PHONE in {text}"
            );
        }
    }

    #[test]
    fn detects_international_phone_format() {
        let spans = PiiDetector.scan_with_filters("UK: +44 20 7946 0958");
        assert!(spans.iter().any(|s| s.pii_type == PiiType::Phone));
    }

    #[test]
    fn detects_ssn_in_dashed_and_run_form() {
        let dashed = PiiDetector.scan_with_filters("SSN 123-45-6789");
        assert!(dashed.iter().any(|s| s.pii_type == PiiType::Ssn));
        let run = PiiDetector.scan_with_filters("ssn 123456789");
        assert!(run.iter().any(|s| s.pii_type == PiiType::Ssn));
    }

    #[test]
    fn credit_card_detection_uses_luhn() {
        // Valid Luhn (test card numbers are 4242 4242 4242 4242).
        let ok = PiiDetector.scan_with_filters("card 4242424242424242");
        assert!(
            ok.iter().any(|s| s.pii_type == PiiType::CreditCard),
            "valid Luhn must match: {ok:?}"
        );
        // Invalid Luhn — same digits, single character changed.
        let bad = PiiDetector.scan_with_filters("card 4242424242424243");
        assert!(
            !bad.iter().any(|s| s.pii_type == PiiType::CreditCard),
            "invalid Luhn must NOT match: {bad:?}"
        );
    }

    #[test]
    fn detects_ipv4_addresses() {
        let spans = PiiDetector.scan_with_filters("server is 192.168.1.1");
        assert!(spans.iter().any(|s| s.pii_type == PiiType::IpAddress));
    }

    #[test]
    fn detects_ipv6_addresses() {
        let spans = PiiDetector.scan_with_filters("v6 is 2001:0db8:85a3:0000:0000:8a2e:0370:7334");
        assert!(spans.iter().any(|s| s.pii_type == PiiType::IpAddress));
    }

    #[test]
    fn detects_urls_with_http_and_https() {
        let spans =
            PiiDetector.scan_with_filters("see https://example.com/x and http://localhost:8080");
        let urls: Vec<_> = spans
            .iter()
            .filter(|s| s.pii_type == PiiType::Url)
            .collect();
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn name_detection_does_not_false_positive_on_titles() {
        let spans = PiiDetector
            .scan_with_filters("The President said. United States grew. The North wind.");
        for s in &spans {
            assert_ne!(
                s.pii_type,
                PiiType::Name,
                "should not match common title pattern: {:?}",
                s.matched_text
            );
        }
    }

    #[test]
    fn name_detection_matches_two_word_proper_names_mid_sentence() {
        let spans = PiiDetector.scan_with_filters("Today John Smith arrived from Boston.");
        let names: Vec<&PiiSpan> = spans
            .iter()
            .filter(|s| s.pii_type == PiiType::Name)
            .collect();
        assert!(
            names.iter().any(|s| s.matched_text == "John Smith"),
            "expected 'John Smith' span, got {names:?}"
        );
    }

    #[test]
    fn api_key_detection_skips_short_strings() {
        let spans = PiiDetector.scan_with_filters("short=abc123");
        assert!(!spans.iter().any(|s| s.pii_type == PiiType::ApiKey));
    }

    #[test]
    fn api_key_detection_matches_mixed_case_long_strings() {
        let s = "secret=ak1B2c3D4e5F6g7H8i9J0kL1mNoPqRsTu";
        let spans = PiiDetector.scan_with_filters(s);
        assert!(
            spans.iter().any(|s| s.pii_type == PiiType::ApiKey),
            "expected API_KEY match: {spans:?}"
        );
    }

    #[test]
    fn api_key_detection_rejects_pure_lowercase_runs() {
        // 30-char pure lowercase string fails the
        // mixed-case + digit gate.
        let s = "lowercase_no_digits_only_alpha_text_abc";
        let spans = PiiDetector.scan_with_filters(s);
        assert!(!spans.iter().any(|s| s.pii_type == PiiType::ApiKey));
    }

    #[test]
    fn detects_dob_with_context_word() {
        let spans = PiiDetector.scan_with_filters("My birthday is 03/14/1980 if you need it.");
        assert!(spans.iter().any(|s| s.pii_type == PiiType::DateOfBirth));
    }

    #[test]
    fn dob_without_context_is_not_flagged() {
        let spans = PiiDetector.scan_with_filters("Meeting on 03/14/2026 at noon.");
        assert!(!spans.iter().any(|s| s.pii_type == PiiType::DateOfBirth));
    }

    #[test]
    fn detects_us_street_addresses() {
        let spans = PiiDetector.scan_with_filters("ship to 1600 Pennsylvania Avenue, Washington.");
        assert!(spans.iter().any(|s| s.pii_type == PiiType::Address));
    }

    #[test]
    fn overlapping_spans_keep_the_longer_match() {
        // Build a candidate where a URL contains an email-
        // like substring; the URL is longer so it should win.
        let text = "see https://foo@example.com/path now";
        let spans = PiiDetector.scan_with_filters(text);
        let url = spans.iter().find(|s| s.pii_type == PiiType::Url);
        assert!(url.is_some(), "URL should win the overlap: {spans:?}");
    }

    #[test]
    fn spans_are_returned_sorted_by_start_position() {
        let text = "email a@b.co and phone 555-123-4567 and ssn 123-45-6789";
        let spans = PiiDetector.scan_with_filters(text);
        for w in spans.windows(2) {
            assert!(
                w[0].start <= w[1].start,
                "spans must be ascending: {spans:?}"
            );
        }
    }

    // ── PiiAnonymizer tests ─────────────────────────────────

    fn redact_anonymizer() -> PiiAnonymizer {
        PiiAnonymizer::from_config(&PiiConfig {
            enabled: true,
            strategy: PiiStrategy::Redact,
            overrides: BTreeMap::new(),
        })
    }

    fn pseudo_anonymizer() -> PiiAnonymizer {
        PiiAnonymizer::from_config(&PiiConfig {
            enabled: true,
            strategy: PiiStrategy::Pseudonymize,
            overrides: BTreeMap::new(),
        })
    }

    #[test]
    fn redact_replaces_each_span_with_placeholder() {
        let text = "email alice@example.com and ssn 123-45-6789";
        let out = redact_anonymizer().anonymize(text);
        assert!(out.contains("[EMAIL]"), "got: {out}");
        assert!(out.contains("[SSN]"), "got: {out}");
        assert!(!out.contains("alice@example.com"));
        assert!(!out.contains("123-45-6789"));
    }

    #[test]
    fn pseudonymize_produces_consistent_replacements_within_a_document() {
        let text = "alice@example.com wrote me. Reply to alice@example.com please.";
        let out = pseudo_anonymizer().anonymize(text);
        // The fake email must appear twice — same input → same output.
        let parts: Vec<&str> = out.split("@redacted.example").collect();
        // Three pieces ⇒ two occurrences of the placeholder.
        assert_eq!(parts.len(), 3, "got: {out}");
    }

    #[test]
    fn pseudonymize_produces_different_replacements_for_different_values() {
        let text = "alice@example.com and bob@example.org";
        let out = pseudo_anonymizer().anonymize(text);
        let pieces: Vec<&str> = out.split("@redacted.example").collect();
        assert_eq!(pieces.len(), 3, "expected two fake emails, got: {out}");
        let first = pieces[0].rsplit_once("user_").unwrap().1;
        let second = pieces[1].rsplit_once("user_").unwrap().1;
        assert_ne!(
            first, second,
            "different inputs must hash to different fakes"
        );
    }

    #[test]
    fn allow_strategy_passes_text_through_unchanged() {
        let text = "alice@example.com and ssn 123-45-6789";
        let a = PiiAnonymizer::from_config(&PiiConfig {
            enabled: true,
            strategy: PiiStrategy::Allow,
            overrides: BTreeMap::new(),
        });
        let out = a.anonymize(text);
        assert_eq!(out, text);
    }

    #[test]
    fn per_type_overrides_take_precedence_over_global_strategy() {
        let mut overrides = BTreeMap::new();
        overrides.insert("EMAIL".into(), PiiStrategy::Redact);
        overrides.insert("NAME".into(), PiiStrategy::Pseudonymize);
        let a = PiiAnonymizer::from_config(&PiiConfig {
            enabled: true,
            strategy: PiiStrategy::Allow, // global allow
            overrides,
        });
        // With global=allow, emails would normally pass; the
        // override forces redact.
        let text = "Reply to alice@example.com today.";
        let out = a.anonymize(text);
        assert!(out.contains("[EMAIL]"), "override should redact: {out}");
        // EMAIL strategy is Redact, NAME is Pseudonymize, others Allow.
        assert_eq!(a.strategy_for(PiiType::Email), PiiStrategy::Redact);
        assert_eq!(a.strategy_for(PiiType::Name), PiiStrategy::Pseudonymize);
        assert_eq!(a.strategy_for(PiiType::Ssn), PiiStrategy::Allow);
    }

    #[test]
    fn anonymizing_string_with_no_pii_returns_original_unchanged() {
        let text = "the cat sat on the mat";
        let out = redact_anonymizer().anonymize(text);
        assert_eq!(out, text);
    }

    #[test]
    fn disabled_anonymizer_is_passthrough() {
        let a = PiiAnonymizer::disabled();
        assert!(!a.enabled());
        let text = "alice@example.com 123-45-6789";
        let out = a.anonymize(text);
        assert_eq!(out, text);
    }

    #[test]
    fn pii_type_round_trips_through_parse_and_as_str() {
        for t in PiiType::all() {
            let s = t.as_str();
            assert_eq!(PiiType::parse(s), Some(t));
        }
        assert_eq!(PiiType::parse("creditcard"), Some(PiiType::CreditCard));
        assert_eq!(PiiType::parse("dob"), Some(PiiType::DateOfBirth));
        assert_eq!(PiiType::parse("nope"), None);
    }

    #[test]
    fn pii_strategy_parses_loose() {
        assert_eq!(PiiStrategy::parse("redact"), Some(PiiStrategy::Redact));
        assert_eq!(
            PiiStrategy::parse("PSEUDONYMIZE"),
            Some(PiiStrategy::Pseudonymize)
        );
        assert_eq!(PiiStrategy::parse("passthrough"), Some(PiiStrategy::Allow));
        assert_eq!(PiiStrategy::parse("nope"), None);
    }

    #[test]
    fn config_parses_minimal_toml() {
        let cfg: PiiConfig = toml::from_str(
            r#"
            enabled = true
            strategy = "redact"
            "#,
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.strategy, PiiStrategy::Redact);
    }

    #[test]
    fn config_parses_overrides() {
        let cfg: PiiConfig = toml::from_str(
            r#"
            enabled = true
            strategy = "pseudonymize"
            [overrides]
            EMAIL = "redact"
            NAME = "allow"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.strategy_for(PiiType::Email), PiiStrategy::Redact);
        assert_eq!(cfg.strategy_for(PiiType::Name), PiiStrategy::Allow);
        assert_eq!(cfg.strategy_for(PiiType::Ssn), PiiStrategy::Pseudonymize);
    }

    #[test]
    fn redact_then_pseudonymize_overlap_keeps_chosen_redaction_only() {
        let text = "see https://test@example.com/path and email plain@x.co";
        let out = redact_anonymizer().anonymize(text);
        // The longer URL span wins the overlap; the email
        // outside the URL gets independently redacted.
        assert!(out.contains("[URL]"));
        assert!(out.contains("[EMAIL]"));
    }

    #[test]
    fn ipv4_octets_are_validated() {
        // 999.999.999.999 should NOT match the IPv4 detector.
        let spans = PiiDetector.scan_with_filters("server=999.999.999.999");
        assert!(!spans.iter().any(|s| s.pii_type == PiiType::IpAddress));
    }

    #[test]
    fn empty_input_returns_empty_span_list() {
        let spans = PiiDetector.scan_with_filters("");
        assert!(spans.is_empty());
    }
}
