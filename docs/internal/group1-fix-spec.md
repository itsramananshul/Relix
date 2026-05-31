# Group 1 Fix Spec — web bridge trusting client-controlled values

Two phases, all in crates/relix-web-bridge. Do PHASE 1A completely and commit it BEFORE starting PHASE 1B.

## PHASE 1A — caller-supplied identity (first)

messaging.rs ~lines 120, 180, 212: from_subject_id, reader_subject_id, subject_id are read from the request body, letting any authenticated caller spoof any sender and read or delete any other user's messages. Fix: derive the caller's subject identity from the authenticated bearer/session context, never from the body. If a body subject field disagrees with the authenticated identity, reject with 403 — never silently override. A caller may only act as themselves unless explicit policy grants more.

Then sweep EVERY handler in crates/relix-web-bridge for any identity field read from the body or query string — grep subject_id, from_, reader_, owner_, sender_, user_id, agent_id, tenant_id and similar. Each must come from verified context, not the wire. Fix every instance, not just messaging.rs.

1A done when printed in transcript:
1. Test: caller authed as subject A cannot send as B (body from_subject_id=B → 403 or forced to A), cannot read/delete B's messages. Passing.
2. Test: legitimate case still works — A sends as A, reads A's own messages. Admitted.
3. Grep over the crate showing every prior body/query identity read now derives from auth context; each remaining body-read classified non-identity-safe with a one-line reason.
4. Commit 1A before 1B. Message: "fix(security): derive caller identity from auth context, never the request body". One commit, Anshul Raman sole author, no co-author trailers, no Claude attribution.

## PHASE 1B — unsafe outbound values (after 1A committed)

1. validate.rs ~line 54: validate_url has no SSRF check; loopback, 169.254 metadata, RFC-1918 all admitted; also reached via openai.rs detect_url_in_message with no operator opt-in. Fix: reject loopback, link-local 169.254.0.0/16, RFC-1918 (10/8, 172.16/12, 192.168/16), metadata IPs. Resolve DNS first and re-check the RESOLVED IP, not just the literal, so a hostname pointing at an internal IP is also blocked. Require explicit operator opt-in for outbound fetch from message content.
2. email.rs ~line 303: SendAttachment.path forwarded verbatim (/etc/shadow exfil). Fix: reject absolute paths and any path containing "..", resolve against a fixed attachment root, confirm canonical resolved path stays inside that root.
3. config_api.rs ~line 438: bot_token PUT with no format check; CRLF splices into outbound URL → request-splitting against api.telegram.org. Fix: validate against ^\d+:[A-Za-z0-9_-]+$ at PUT, reject otherwise. Sweep config_api.rs for sibling config fields interpolated into outbound URLs and apply the same anti-CRLF/format validation.

1B done when printed in transcript:
5. Test: validate_url rejects loopback, a 169.254 metadata address, an RFC-1918 address, AND a hostname that resolves to an internal IP, while allowing a normal public URL. Passing.
6. Test: attachment path /etc/shadow and one containing ".." rejected, legitimate in-root attachment accepted. Passing.
7. Test: bot_token with CRLF or out-of-charset bytes rejected at PUT, well-formed token accepted. Passing.
8. Grep over config_api.rs confirming every config field interpolated into an outbound URL is format-validated.
9. cargo test for relix-web-bridge exits 0 and all pre-existing bridge tests pass. Shown.
10. Commit 1B. Message: "fix(security): validate URLs, attachment paths, and config tokens before outbound use". One commit, Anshul Raman sole author, no co-author trailers, no Claude attribution.

## Constraints
Do not break legitimate paths — criteria 2, 5(public URL), 6(in-root), 7(valid token) prove this. Fail closed on untrusted input but normal authenticated callers must still work. Do not touch crates outside relix-web-bridge. No unrelated refactors. Two separate commits, 1A before 1B.
