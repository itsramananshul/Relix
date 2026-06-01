# Email channel

A `node_type = "email"` controller watches an IMAP inbox for inbound
email, runs each permitted message through the canonical chat flow
(memory recent → ai.chat → memory write × 2), and sends the reply via
SMTP. It coexists on the mesh with the Telegram, Discord, and Slack
channels — an independent peer with its own identity bundle, port, and
config block.

## What it does

For every inbound email:

1. Derives a stable `session_id` from RFC 5322 threading headers:
   `email-thread:<References[0]>` if present, else
   `email-thread:<In-Reply-To>`, else `email-thread:<Message-ID>`.
   This keeps multi-turn conversations in the same memory session.
2. Records the message in a bounded in-memory ring (default 200,
   configurable via `[email] messages_ring_capacity`) so the dashboard
   can render the recent-messages widget without touching AI / memory
   peers.
3. Enforces `allowed_senders` — case-insensitive bare addr-spec
   comparison (`<name@host>` envelope is stripped). Empty list = allow
   all.
4. Dispatches `ai.chat` with up to 10 recent turns from memory.
5. Sends the reply via the pooled SMTP connection. Threading headers
   (`In-Reply-To`, `References`) are set automatically so mail clients
   thread the conversation correctly.

## Setup

### 1. SMTP credentials

You need an SMTP relay your server can reach. Common choices:
Mailgun, Postmark, SendGrid, or your own postfix. Obtain:

- Hostname (e.g. `smtp.mailgun.org`), port (typically `587` for
  STARTTLS or `465` for implicit TLS).
- Username and password (or an OAuth2 token if your provider uses
  XOAUTH2).

### 2. IMAP credentials

The same mailbox (or a dedicated one) needs IMAP access. Most hosted
providers use port `993` with implicit TLS. STARTTLS and plain-text
IMAP are **not** supported — only implicit TLS (`imap_port = 993` is
the hardcoded default).

### 3. Environment variables

```
RELIX_EMAIL_SMTP_PASSWORD=<smtp-password>
RELIX_EMAIL_IMAP_PASSWORD=<imap-password>
```

Reference these by name in the config (never put the values directly
in the TOML):

```toml
[email]
smtp_password_env = "RELIX_EMAIL_SMTP_PASSWORD"
imap_password_env = "RELIX_EMAIL_IMAP_PASSWORD"
```

### 4. Configure

```toml
[controller]
name        = "local-email"
node_type   = "email"
listen_port = 19718

[email]
smtp_host    = "smtp.mailgun.org"
smtp_port    = 587
smtp_username = "postmaster@mg.example.com"
smtp_password_env = "RELIX_EMAIL_SMTP_PASSWORD"
smtp_from    = "agent@example.com"
smtp_tls     = "starttls"     # starttls | tls | implicit | smtps | none | plain | insecure

imap_host    = "imap.mailgun.org"
imap_port    = 993            # only implicit TLS; no STARTTLS/plain fallback
imap_username = "postmaster@mg.example.com"
imap_password_env = "RELIX_EMAIL_IMAP_PASSWORD"
imap_folder  = "INBOX"        # must be non-empty

messages_ring_capacity = 200
allowed_senders = []          # empty = allow everyone

[email.memory_peer]
addr = "/ip4/127.0.0.1/tcp/19711"

[email.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
deadline_secs = 60

[email.coord_peer]
addr = "/ip4/127.0.0.1/tcp/19714"
```

### 5. Verify

```
GET http://127.0.0.1:19791/v1/email/status
```

Returns connection status for both SMTP and IMAP. Send a test email
to the configured mailbox; `GET /v1/email/status` should show
`imap=connected` and `messages_seen` incrementing.

## IMAP inbound

The controller prefers IMAP IDLE push (RFC 2177) when the server
advertises `CAPABILITY IDLE`. It reconnects after the standard 28-minute
refresh tick. Servers that do not support IDLE fall back to polling
every `imap_poll_interval_secs` (default 60 s).

Only `UNSEEN` messages in `imap_folder` are fetched. After a message
is processed:

- It is marked `\Seen`.
- If `imap_processed_folder` is set (non-empty), the message is also
  moved to that folder.

Messages larger than `imap_max_message_bytes` (default 10 MiB) are
bounced and never dispatched. Spam / junk folders are blocked: if
`imap_folder` is named `spam`, `junk`, `junk e-mail`, `junk email`,
`trash`, `deleted items`, or contains the word "spam" or "junk", the
controller refuses to watch it.

Attachments are written to
`<system_tmp>/relix-email-att/<uid>/<n>-<filename>` for the duration
of the message handler; they are available via `email.send` paths for
agent flows that need to relay them.

## SMTP outbound

The controller uses **lettre** with a pooled SMTP connection
(max idle connections configurable via `smtp_pool_max`, default 8).

Outbound limits:

- Hard size cap: **26 MiB** per message (`MAX_MESSAGE_BYTES`). The
  call returns `SmtpError::OversizeAttachment` before hitting the wire.
- Transient failure retry: up to `smtp_max_retries` (default 3).
  Permanent 5xx responses are never retried.

Every outbound message carries `X-Mailer: Relix`. If the caller does
not supply a `Message-ID`, lettre generates a globally unique one.

Attachment sourcing in `email.send`:

- `bytes_base64` — base64-decoded in-process. Suitable for small
  attachments; runs synchronously on the dispatch task.
- `path` — `std::fs::read` synchronously. Attachments without either
  source are silently dropped with a warning logged.

## DKIM signing

Optional outbound DKIM signing follows RFC 6376, RSA-SHA256,
`relaxed/relaxed` canonicalization. To enable, set **all three** of:

```toml
[email]
dkim_private_key_path = "/etc/relix/dkim.pem"   # PKCS#1 or PKCS#8
dkim_selector         = "2024"                   # s= DNS tag
dkim_domain           = "example.com"            # d= DNS tag
```

If **any** of the three fields is absent or empty, DKIM signing is
silently disabled — a warning is logged at boot but sends proceed
unsigned. There is no partial-config error.

Supported key formats: `-----BEGIN RSA PRIVATE KEY-----` (PKCS#1) and
`-----BEGIN PRIVATE KEY-----` (PKCS#8). Ed25519 is not supported.

Headers signed by default: `from`, `to`, `subject`, `date`,
`message-id`. Headers absent from the message are silently excluded
from the `h=` tag per RFC 6376.

> **Note:** `crates/relix-runtime/src/nodes/email/test-dkim-key.pem`
> is a 1024-bit RSA key included only for unit tests. It must not be
> used in production.

## OAuth2

If your provider uses XOAUTH2, you can configure either the SMTP or
IMAP path (or both) via direct token env vars:

```toml
smtp_oauth2_token_env = "MY_SMTP_OAUTH2_TOKEN"   # wins over smtp_password_env
imap_oauth2_token_env = "MY_IMAP_OAUTH2_TOKEN"   # mutually exclusive with imap_password_env
```

Full token-refresh flow (all four fields required together, or all
absent):

```toml
oauth2_client_id_env      = "MY_OAUTH_CLIENT_ID"
oauth2_client_secret_env  = "MY_OAUTH_CLIENT_SECRET"
oauth2_refresh_token_env  = "MY_OAUTH_REFRESH_TOKEN"
oauth2_token_endpoint     = "https://oauth2.example.com/token"
```

## Templates

The `email.send_template` capability renders a named template and
sends it. Template resolution order:

1. Operator template directory (env `RELIX_EMAIL_TEMPLATES_DIR`) —
   files named `<name>.toml` with `subject`, `body`, and optional
   `html` fields. Path-traversal names (containing `/`, `\\`, `..`,
   or NUL) are silently rejected and fall through to built-ins.
2. Built-in templates: `welcome`, `reset_password`, `task_completed`,
   `task_failed`.

Variable substitution uses `{{var}}` syntax. Unknown variables pass
through literally.

## Capabilities

| Capability | Direction | Notes |
|---|---|---|
| `email.status` | read-only | SMTP + IMAP connection status, counters |
| `email.messages_recent` | read-only | Ring contents, newest-first |
| `email.send` | mutating | Plain/HTML email with optional attachments |
| `email.send_template` | mutating | Template render + send |
| `email.approval_send` | mutating | Approval notification delivery |

## Wire shape

`email.status` returns:
```
smtp=<status>|imap=<status>|from=<addr>|smtp_host=<host>|imap_host=<host>|imap_folder=<f>|messages_seen=<u64>|messages_sent=<u64>|last_send_at=<i64>|last_poll_at=<i64>|last_message_at=<i64>|smtp_error=<str>|imap_error=<str>\n
```
`status` values: `connected`, `disconnected`, `error`. Timestamps are
unix-seconds; `-1` means "never". Error strings have `\n\r\t|`
replaced with spaces.

`email.messages_recent` returns one tab-separated row per message
(newest-first):
```
ts\tmessage_id\tfrom\tsubject\tsession_id\tpreview\n
```
Preview is truncated to 200 chars; tabs, newlines, and `|` are
replaced with spaces.

## `email.send` JSON shape

```json
{
  "to": ["alice@example.com"],
  "cc": [],
  "bcc": [],
  "reply_to": null,
  "subject": "Hello",
  "body": "Plain text body",
  "html": null,
  "in_reply_to": null,
  "references": null,
  "attachments": [
    {
      "path": null,
      "bytes_base64": "<base64-encoded bytes>",
      "filename": "report.pdf",
      "content_type": "application/octet-stream"
    }
  ]
}
```

`content_type` defaults to `"application/octet-stream"`. On success:
`{"message_id": "<generated-id>"}`.

## `email.send_template` JSON shape

```json
{
  "template_name": "welcome",
  "to": ["alice@example.com"],
  "cc": [],
  "bcc": [],
  "reply_to": null,
  "in_reply_to": null,
  "references": null,
  "variables": {"name": "Alice"}
}
```

On success: `{"message_id": "<id>", "template": "<name>"}`.

## HTTP surfaces

```
POST /v1/email/send
POST /v1/email/send_template
GET  /v1/email/status
```

### Inbound reply webhook

When using an inbound email parsing provider (Mailgun, SendGrid,
Postmark), point the webhook at:

```
POST /v1/channels/email/reply
```

The bridge extracts an approve / deny decision from the subject line.
Mailgun HMAC verification is optional: set
`RELIX_BRIDGE_MAILGUN_SIGNING_KEY` to enable it (unset = warning
logged, request accepted without verification).

## Configuration reference

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `true` | Master switch. |
| `smtp_host` | String | — | Required. |
| `smtp_port` | u16 | `587` | |
| `smtp_username` | String | `""` | |
| `smtp_password_env` | String | `""` | Env var name. Empty = no password auth. |
| `smtp_oauth2_token_env` | String | `""` | XOAUTH2. Wins over `smtp_password_env`. |
| `smtp_from` | String | — | `From:` address. Required. |
| `smtp_tls` | String | `"starttls"` | `starttls` / `tls` / `implicit` / `smtps` / `none` / `plain` / `insecure` |
| `smtp_max_retries` | u32 | `3` | Transient-failure retries; 5xx never retries. |
| `smtp_pool_max` | u32 | `8` | lettre pool max idle connections. |
| `dkim_private_key_path` | PathBuf | `""` | PEM file. All three DKIM fields must be set to enable. |
| `dkim_selector` | String | `""` | `s=` DNS tag. |
| `dkim_domain` | String | `""` | `d=` DNS tag. |
| `imap_host` | String | — | Required. |
| `imap_port` | u16 | `993` | Implicit TLS only; no STARTTLS or plain fallback. |
| `imap_username` | String | `""` | |
| `imap_password_env` | String | `""` | Env var name. |
| `imap_oauth2_token_env` | String | `""` | XOAUTH2. Mutually exclusive with `imap_password_env`. |
| `imap_folder` | String | `"INBOX"` | Must be non-empty. |
| `imap_processed_folder` | String | `""` | Move-to folder after dispatch. Empty = mark `\Seen` only. |
| `imap_poll_interval_secs` | u64 | `60` | Fallback poll when server has no IDLE. |
| `imap_max_message_bytes` | u64 | `10485760` | Oversized messages bounced, never processed. |
| `oauth2_client_id_env` | String | `""` | All four OAuth2 fields required together or all absent. |
| `oauth2_client_secret_env` | String | `""` | |
| `oauth2_refresh_token_env` | String | `""` | |
| `oauth2_token_endpoint` | String | `""` | |
| `messages_ring_capacity` | usize | `200` | |
| `allowed_senders` | Vec\<String\> | `[]` | Case-insensitive bare addr-spec. Empty = allow all. |
| `operator_address` | String | `""` | Reserved for approval notifications. |

## Security notes

- SMTP / IMAP passwords and tokens live only in env vars — never in
  the config file.
- `allowed_senders` comparison strips the display-name envelope and
  lowercases the addr-spec before matching.
- The controller dials memory / ai / coordinator peers with its own
  signed identity bundle. Per-call admission (identity → policy →
  handler → audit) runs on every dispatch — email traffic gets no
  bypass.

## See also

- [index.md](index.md) — overview of all channels.
- [`../channel-node-architecture.md`](../channel-node-architecture.md) —
  the design contract every channel implements.
- [`../configuration.md`](../configuration.md) — full env-var
  reference for the mesh boot script.
