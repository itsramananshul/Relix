//! `tool.web.robots_check` — robots.txt sniff for a target URL.
//!
//! Operators who want to *be polite* before crawling a site can ask
//! this capability whether the site's robots.txt would permit a
//! given URL for a given user-agent. The capability does NOT enforce
//! anything — it returns a structured allow/deny answer plus the
//! matched rule so the caller can decide.
//!
//! ## Wire format
//!
//! Argument is a UTF-8 string. Two forms accepted:
//!
//! | Arg | Meaning |
//! |---|---|
//! | `<target_url>` | Check the target URL against robots.txt for `*`. |
//! | `<target_url>\|<user_agent>` | Check for the given user-agent token. |
//!
//! Response body is a `key=value\n` block:
//!
//! ```text
//! target=https://example.com/some/path
//! robots_url=https://example.com/robots.txt
//! user_agent=*
//! allowed=true
//! matched_rule=Allow: /
//! crawl_delay=-
//! source=fetched
//! ```
//!
//! `matched_rule` is `-` when no rule matched (default-allow). `source`
//! is `fetched` when robots.txt was retrieved successfully and
//! `missing` when the fetch returned a non-2xx status (defaults to
//! allow, per RFC 9309 §2.3). `crawl_delay` is `-` when the rule
//! group did not specify one.
//!
//! ## Honesty contract
//!
//! - The capability does not crawl, does not cache, does not retry.
//!   Every call is one HTTP fetch.
//! - On any SSRF rejection / transport / non-UTF-8 robots.txt body
//!   the response is an `ErrorEnvelope` — never a silent default-allow.
//! - The parser implements the RFC 9309 / Google extension subset:
//!   `User-agent`, `Allow`, `Disallow`, `Crawl-delay`. Sitemap lines
//!   and unknown directives are ignored. Wildcards in path patterns
//!   are not interpreted (literal prefix match only) — the parser
//!   honors longest-match-wins on prefix length, and ties go to
//!   `Allow` (Google's tie-break).

use std::sync::Arc;

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};
use reqwest::Url;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::{ToolBackend, WebFetchOutcome};

// ─────────────────────────── Descriptor ───────────────────────────

/// Descriptor for `tool.web.robots_check`. Egress posture identical
/// to `tool.web_fetch` — the only difference is the URL is always
/// `<scheme>://<host>/robots.txt`.
pub fn robots_check_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.web.robots_check");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec![
        "external:network".into(),
        "egress:http".into(),
        "parse:robots".into(),
    ];
    d.policy_attachment_point = "tool.web.robots_check".to_string();
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Fetch <scheme>://<host>/robots.txt and report whether the target URL \
         is allowed for the given user-agent (default '*'). Honors RFC 9309 \
         longest-prefix-match-wins with Allow as tie-break. Missing robots.txt \
         defaults to allowed."
            .into(),
    );
    d.categories = vec!["fetch".into(), "parse".into(), "safety".into()];
    d.environment_requirements = vec!["network:outbound".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

// ─────────────────────────── Register ───────────────────────────

/// Wire `tool.web.robots_check` onto the dispatch bridge. Caller is
/// the tool-node `register()` in `mod.rs`.
pub fn register(bridge: &mut DispatchBridge, backend: Arc<ToolBackend>) {
    bridge.register(
        "tool.web.robots_check",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let backend = backend.clone();
            async move { handle_robots_check(backend, ctx).await }
        })),
    );
}

// ─────────────────────────── Handler ───────────────────────────

async fn handle_robots_check(backend: Arc<ToolBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid_args(format!("tool.web.robots_check arg utf8: {e}")),
    };
    let (target_str, user_agent) = match raw.rsplit_once('|') {
        Some((url, ua)) => (url.trim(), ua.trim()),
        None => (raw.trim(), "*"),
    };
    let user_agent = if user_agent.is_empty() {
        "*"
    } else {
        user_agent
    };
    if target_str.is_empty() {
        return invalid_args(
            "tool.web.robots_check: url required (arg: `<target_url>` or \
             `<target_url>|<user_agent>`)"
                .into(),
        );
    }

    let target = match Url::parse(target_str) {
        Ok(u) => u,
        Err(e) => return invalid_args(format!("tool.web.robots_check url parse: {e}")),
    };
    if target.scheme() != "http" && target.scheme() != "https" {
        return invalid_args(format!(
            "tool.web.robots_check: only http/https supported (got '{}')",
            target.scheme()
        ));
    }
    let host = match target.host_str() {
        Some(h) => h,
        None => return invalid_args("tool.web.robots_check: target url has no host".into()),
    };
    let robots_url = match target.port() {
        Some(p) => format!("{}://{}:{}/robots.txt", target.scheme(), host, p),
        None => format!("{}://{}/robots.txt", target.scheme(), host),
    };

    let outcome = backend.fetch(&robots_url, 256 * 1024).await;
    let (body, source) = match outcome {
        WebFetchOutcome::Ok { body, .. } => (body, "fetched"),
        WebFetchOutcome::HttpStatus { status, .. } if status == 404 || status == 410 => {
            (String::new(), "missing")
        }
        WebFetchOutcome::Rejected(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::POLICY_DENIED,
                cause: format!("tool.web.robots_check ssrf-rejected: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
        WebFetchOutcome::TooLarge {
            declared_bytes,
            cap,
        } => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!(
                    "tool.web.robots_check robots.txt too large: declared={declared_bytes}B cap={cap}B"
                ),
                retry_hint: 2,
                retry_after: None,
            });
        }
        WebFetchOutcome::HttpStatus { status, final_url } => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("tool.web.robots_check http {status} for {final_url}"),
                retry_hint: 1,
                retry_after: None,
            });
        }
        WebFetchOutcome::ContentTypeRejected {
            content_type,
            final_url,
        } => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!(
                    "tool.web.robots_check unexpected content-type '{content_type}' for {final_url}"
                ),
                retry_hint: 2,
                retry_after: None,
            });
        }
        WebFetchOutcome::NotUtf8 { final_url } => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.web.robots_check robots.txt not utf-8 for {final_url}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
        WebFetchOutcome::Transport(c) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::TRANSPORT,
                cause: format!("tool.web.robots_check transport: {c}"),
                retry_hint: 1,
                retry_after: None,
            });
        }
    };

    let parsed = parse_robots_txt(&body);
    let path_with_query = match target.query() {
        Some(q) => format!("{}?{q}", target.path()),
        None => target.path().to_string(),
    };
    let decision = decide(&parsed, user_agent, &path_with_query);

    let rendered = render_decision(target_str, &robots_url, user_agent, &decision, source);
    HandlerOutcome::Ok(rendered.into_bytes())
}

fn invalid_args(msg: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg,
        retry_hint: 2,
        retry_after: None,
    })
}

// ─────────────────────────── Parser ───────────────────────────

/// A single allow/disallow rule within a user-agent group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotsRule {
    /// True for `Allow:`, false for `Disallow:`.
    pub allow: bool,
    /// Path prefix exactly as it appeared after the directive
    /// (whitespace-trimmed). Empty pattern (e.g. `Disallow:`)
    /// is interpreted per RFC 9309 §2.2.2 as "no restriction"
    /// (matches nothing).
    pub pattern: String,
}

/// One user-agent group. Multiple `User-agent:` lines before the
/// first `Allow`/`Disallow` collapse into a single group with
/// multiple agents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RobotsGroup {
    pub agents: Vec<String>,
    pub rules: Vec<RobotsRule>,
    pub crawl_delay_secs: Option<u64>,
}

/// Parsed robots.txt — a list of groups in source order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedRobots {
    pub groups: Vec<RobotsGroup>,
}

/// Parse a robots.txt body into groups. Tolerant: unknown directives
/// are ignored; case-insensitive on directive names and agent
/// matching; comments after `#` are stripped.
pub fn parse_robots_txt(body: &str) -> ParsedRobots {
    let mut out = ParsedRobots::default();
    let mut current = RobotsGroup::default();
    let mut last_was_rule = false;

    for raw_line in body.lines() {
        let line = match raw_line.split_once('#') {
            Some((before, _)) => before,
            None => raw_line,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        let (directive, value) = match line.split_once(':') {
            Some((d, v)) => (d.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => continue,
        };
        match directive.as_str() {
            "user-agent" => {
                // A new agent declaration after a rule starts a new group.
                if last_was_rule {
                    out.groups.push(std::mem::take(&mut current));
                }
                current.agents.push(value);
                last_was_rule = false;
            }
            "allow" | "disallow" => {
                if current.agents.is_empty() {
                    // Orphan rule before any user-agent — treat as global.
                    current.agents.push("*".into());
                }
                current.rules.push(RobotsRule {
                    allow: directive == "allow",
                    pattern: value,
                });
                last_was_rule = true;
            }
            "crawl-delay" => {
                if let Ok(n) = value.parse::<u64>() {
                    current.crawl_delay_secs = Some(n);
                }
                last_was_rule = true;
            }
            _ => { /* sitemap, host, etc. — ignored */ }
        }
    }
    if !current.agents.is_empty() || !current.rules.is_empty() {
        out.groups.push(current);
    }
    out
}

// ─────────────────────────── Decision ───────────────────────────

/// Outcome of a check against a parsed robots.txt for one URL+UA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotsDecision {
    pub allowed: bool,
    /// Human-readable matched rule (e.g. `"Allow: /search"`) or `-`
    /// when no rule matched the path.
    pub matched_rule: String,
    /// `Crawl-delay` from the matched group, if any.
    pub crawl_delay_secs: Option<u64>,
}

/// Apply RFC 9309 longest-match-wins resolution. Returns a synthetic
/// default-allow decision when no group matches the user-agent (the
/// site has rules but none target this UA) — same posture as a
/// missing robots.txt.
pub fn decide(parsed: &ParsedRobots, user_agent: &str, path: &str) -> RobotsDecision {
    if parsed.groups.is_empty() {
        return RobotsDecision {
            allowed: true,
            matched_rule: "-".into(),
            crawl_delay_secs: None,
        };
    }
    let group = match pick_group(parsed, user_agent) {
        Some(g) => g,
        None => {
            return RobotsDecision {
                allowed: true,
                matched_rule: "-".into(),
                crawl_delay_secs: None,
            };
        }
    };
    let mut best: Option<(usize, bool, &str)> = None;
    for r in &group.rules {
        if r.pattern.is_empty() {
            // Empty pattern: per spec, "Disallow:" alone is a
            // null rule. Skip — never matches.
            continue;
        }
        if path.starts_with(&r.pattern) {
            let len = r.pattern.len();
            best = match best {
                None => Some((len, r.allow, &r.pattern)),
                Some((blen, _, _)) if len > blen => Some((len, r.allow, &r.pattern)),
                // Tie-break: Allow wins over Disallow (Google
                // convention; RFC 9309 leaves tie undefined).
                Some((blen, false, _)) if len == blen && r.allow => Some((len, true, &r.pattern)),
                other => other,
            };
        }
    }
    match best {
        Some((_, allow, pat)) => RobotsDecision {
            allowed: allow,
            matched_rule: format!("{}: {}", if allow { "Allow" } else { "Disallow" }, pat),
            crawl_delay_secs: group.crawl_delay_secs,
        },
        None => RobotsDecision {
            allowed: true,
            matched_rule: "-".into(),
            crawl_delay_secs: group.crawl_delay_secs,
        },
    }
}

/// Pick the most-specific group for a UA, per RFC 9309 §2.2.1: case
/// insensitive substring match on `user_agent` against each agent
/// token; longest matching token wins; `*` is the fallback.
fn pick_group<'a>(parsed: &'a ParsedRobots, user_agent: &str) -> Option<&'a RobotsGroup> {
    let ua_lower = user_agent.to_ascii_lowercase();
    let mut best: Option<(usize, &RobotsGroup)> = None;
    let mut star: Option<&RobotsGroup> = None;
    for g in &parsed.groups {
        for token in &g.agents {
            let tok = token.trim().to_ascii_lowercase();
            if tok == "*" {
                star = star.or(Some(g));
                continue;
            }
            if !tok.is_empty() && ua_lower.contains(&tok) {
                let len = tok.len();
                best = match best {
                    None => Some((len, g)),
                    Some((blen, _)) if len > blen => Some((len, g)),
                    other => other,
                };
            }
        }
    }
    best.map(|(_, g)| g).or(star)
}

fn render_decision(
    target: &str,
    robots_url: &str,
    user_agent: &str,
    d: &RobotsDecision,
    source: &str,
) -> String {
    let cd = d
        .crawl_delay_secs
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".into());
    format!(
        "target={target}\n\
         robots_url={robots_url}\n\
         user_agent={user_agent}\n\
         allowed={allowed}\n\
         matched_rule={rule}\n\
         crawl_delay={cd}\n\
         source={source}\n",
        allowed = d.allowed,
        rule = d.matched_rule,
    )
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_shape() {
        let d = robots_check_descriptor();
        assert_eq!(d.method_name, "tool.web.robots_check");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.idempotency, Idempotency::AtMostOnce));
        assert!(matches!(d.cost_class, CostClass::ExternalPaid));
        assert!(d.sensitivity_tags.iter().any(|t| t == "external:network"));
        assert!(d.sensitivity_tags.iter().any(|t| t == "parse:robots"));
        assert!(d.requires_groups.iter().any(|g| g == "chat-users"));
    }

    #[test]
    fn parse_empty_body() {
        let p = parse_robots_txt("");
        assert!(p.groups.is_empty());
    }

    #[test]
    fn parse_single_group() {
        let p = parse_robots_txt(
            "User-agent: *\n\
             Disallow: /private/\n\
             Allow: /private/public/\n\
             Crawl-delay: 5\n",
        );
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].agents, vec!["*"]);
        assert_eq!(p.groups[0].rules.len(), 2);
        assert!(!p.groups[0].rules[0].allow);
        assert_eq!(p.groups[0].rules[0].pattern, "/private/");
        assert!(p.groups[0].rules[1].allow);
        assert_eq!(p.groups[0].crawl_delay_secs, Some(5));
    }

    #[test]
    fn parse_multiple_agents_one_group() {
        let p = parse_robots_txt(
            "User-agent: GoogleBot\n\
             User-agent: BingBot\n\
             Disallow: /admin\n",
        );
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].agents.len(), 2);
        assert_eq!(p.groups[0].rules.len(), 1);
    }

    #[test]
    fn parse_groups_split_on_new_user_agent_after_rule() {
        let p = parse_robots_txt(
            "User-agent: *\n\
             Disallow: /one\n\
             User-agent: BadBot\n\
             Disallow: /\n",
        );
        assert_eq!(p.groups.len(), 2);
        assert_eq!(p.groups[0].agents, vec!["*"]);
        assert_eq!(p.groups[1].agents, vec!["BadBot"]);
        assert_eq!(p.groups[1].rules[0].pattern, "/");
    }

    #[test]
    fn parse_ignores_unknown_directives_and_comments() {
        let p = parse_robots_txt(
            "# hello world\n\
             Sitemap: https://example.com/sitemap.xml\n\
             User-agent: *\n\
             Disallow: /tmp/  # don't crawl tmp\n\
             Host: example.com\n",
        );
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.groups[0].rules.len(), 1);
        assert_eq!(p.groups[0].rules[0].pattern, "/tmp/");
    }

    #[test]
    fn decide_no_rules_means_allow() {
        let p = ParsedRobots::default();
        let d = decide(&p, "Anything", "/whatever");
        assert!(d.allowed);
        assert_eq!(d.matched_rule, "-");
        assert!(d.crawl_delay_secs.is_none());
    }

    #[test]
    fn decide_longest_prefix_wins() {
        let p = parse_robots_txt(
            "User-agent: *\n\
             Disallow: /a/\n\
             Allow: /a/b/\n",
        );
        let d = decide(&p, "*", "/a/b/page");
        assert!(d.allowed, "longer Allow should beat shorter Disallow");
        assert_eq!(d.matched_rule, "Allow: /a/b/");
    }

    #[test]
    fn decide_allow_wins_on_tie() {
        let p = parse_robots_txt(
            "User-agent: *\n\
             Disallow: /x\n\
             Allow: /x\n",
        );
        let d = decide(&p, "*", "/x");
        assert!(d.allowed, "Allow must win a same-length tie");
    }

    #[test]
    fn decide_ua_specific_group_overrides_star() {
        let p = parse_robots_txt(
            "User-agent: *\n\
             Allow: /\n\
             User-agent: GoogleBot\n\
             Disallow: /\n",
        );
        let d = decide(&p, "GoogleBot", "/anything");
        assert!(!d.allowed);
        assert_eq!(d.matched_rule, "Disallow: /");
    }

    #[test]
    fn decide_ua_fallback_to_star_when_no_specific_group() {
        let p = parse_robots_txt(
            "User-agent: *\n\
             Disallow: /no\n\
             User-agent: GoogleBot\n\
             Disallow: /\n",
        );
        let d = decide(&p, "RelixCrawler", "/no/page");
        assert!(!d.allowed);
        assert_eq!(d.matched_rule, "Disallow: /no");
    }

    #[test]
    fn decide_no_matching_rule_means_allow_with_group_crawl_delay() {
        let p = parse_robots_txt(
            "User-agent: *\n\
             Disallow: /private/\n\
             Crawl-delay: 7\n",
        );
        let d = decide(&p, "*", "/public/page");
        assert!(d.allowed);
        assert_eq!(d.matched_rule, "-");
        assert_eq!(d.crawl_delay_secs, Some(7));
    }

    #[test]
    fn decide_empty_disallow_is_null_rule() {
        // RFC 9309 §2.2.2: `Disallow:` (with empty value) is a
        // null rule. Should be allowed because nothing matches.
        let p = parse_robots_txt(
            "User-agent: *\n\
             Disallow:\n",
        );
        let d = decide(&p, "*", "/anything");
        assert!(d.allowed);
        assert_eq!(d.matched_rule, "-");
    }

    #[test]
    fn render_decision_shape() {
        let d = RobotsDecision {
            allowed: false,
            matched_rule: "Disallow: /admin".into(),
            crawl_delay_secs: Some(2),
        };
        let s = render_decision(
            "https://example.com/admin/x",
            "https://example.com/robots.txt",
            "MyBot",
            &d,
            "fetched",
        );
        assert!(s.contains("target=https://example.com/admin/x\n"));
        assert!(s.contains("robots_url=https://example.com/robots.txt\n"));
        assert!(s.contains("user_agent=MyBot\n"));
        assert!(s.contains("allowed=false\n"));
        assert!(s.contains("matched_rule=Disallow: /admin\n"));
        assert!(s.contains("crawl_delay=2\n"));
        assert!(s.contains("source=fetched\n"));
    }

    /// PH-RISK-PIN-ALL: tool.web.robots_check makes an outbound
    /// HTTP request (SSRF-gated) — Medium tier, same as the
    /// rest of the network-touching web tools.
    #[test]
    fn web_robots_descriptor_has_medium_risk() {
        let d = robots_check_descriptor();
        assert_ne!(d.risk_level, RiskLevel::Unknown);
        assert_eq!(d.risk_level, RiskLevel::Medium);
    }
}
