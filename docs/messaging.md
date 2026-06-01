# Agent-to-agent messaging

Direct point-to-point mail-drop between two agents. Lives on
the **coordinator** node next to the task ledger. No task is
created per message; the audit trail is one `msg.sent`
chronicle event on a dedicated bookkeeping task. Bodies stay
out of the chronicle.

> This document covers the **coordinator `msg.*` capability** —
> agent-to-agent signalling inside the mesh. For the email
> **channel node** that watches an IMAP inbox and sends SMTP
> replies, see [channels/email.md](channels/email.md).

## Why this exists separately from delegation

Both are agent-to-agent primitives, but they answer different
questions:

| | Delegation | Messaging |
|---|---|---|
| Use case | "Do this subtask for me" | "FYI" / "status?" / coordination |
| Creates a task? | Yes — full lifecycle | No — a row in `agent_messages` |
| Lifecycle hooks | pending → running → completed/failed | delivered → read → expired |
| Caller waits? | Polls `delegate.result` | Polls `msg.inbox` |
| Audit detail | per-task chronicle | one `msg.sent` event per send |
| Throughput | bounded by executor concurrency | unbounded |
| Best for | discrete units of work | lightweight signalling |

If the recipient needs to *do* something and report back
through a structured lifecycle, use delegation. If the
recipient just needs to *know* something, use messaging.

## Lifecycle

```
delivered → read → expired
    │           ↑
    ╰── (auto-expire after ttl_secs from sent_at)
```

- **`delivered`** — the row exists in the recipient's inbox.
- **`read`** — recipient called `msg.read`; `read_at` is now set.
- **`expired`** — either `sent_at + ttl_secs` has passed (caught
  by the 5-minute auto-expire sweeper) OR sender/recipient
  called `msg.delete`. Soft-deleted rows remain in audit but
  disappear from operator inboxes.

Default `ttl_secs = 86400` (24 h). Callers can pass any
positive integer; `0` or empty defaults to 24 h.

## Threads

Every send carries a `thread_id`. When omitted on the first
send in a thread, the coordinator uses the new `message_id` as
the thread id (the message becomes a thread-starter). Replies
pass the original thread_id in `thread_id` and the parent
message id in `reply_to_message_id`.

`msg.thread <thread_id>|<subject_id>` returns the full
oldest-first transcript. The caller must be sender or
recipient on at least one message in the thread — third
parties are denied.

## Sending and reading

### Dashboard

`#/messages` in the Operate sidebar:
- Left card: inbox view for a selected agent. Dropdown
  populates from `/v1/agents`. Include-read toggle exposes
  already-read messages.
- Right card: compose form. From dropdown, To free-form
  subject_id, Subject, Body, optional Thread id.
- Click View on any inbox row to open the thread detail
  card with Mark read / Delete actions.

### CLI

```sh
relix-cli ops msg send \
  --from <subject_id> \
  --to   <subject_id> \
  --subject "Status check" \
  --body   "Are we still on for 4 PM?"

relix-cli ops msg inbox --subject-id <subject_id> [--include-read]
relix-cli ops msg read  --message-id <id> --reader-subject-id <id>
relix-cli ops msg thread --thread-id <id> --subject-id <id>
relix-cli ops msg delete --message-id <id> --subject-id <id>
```

### HTTP

```
POST /v1/messages
{
  "from_subject_id": "<from>",
  "to_subject_id":   "<to>",
  "subject":         "Status check",
  "body":            "Are we still on for 4 PM?",
  "thread_id":       "(optional)",
  "reply_to_message_id": "(optional)",
  "ttl_secs":        86400,
  "origin_surface":  "api"
}
→ { "message_id": "<16-hex>" }
```

```
GET /v1/messages/inbox/<subject_id>?limit=20&include_read=0
→ { "messages": [ { message_id, thread_id, from_subject_id,
       subject, body_preview, sent_at, read_at, status }, ... ],
    "count": N }
```

```
POST /v1/messages/<message_id>/read
{ "reader_subject_id": "<subject>" }     → { "ok": true }

GET  /v1/messages/thread/<thread_id>?subject_id=<subject>
                                          → { "thread_id", "messages": [...] }

DELETE /v1/messages/<message_id>
{ "subject_id": "<subject>" }            → { "ok": true }
```

### Wire capabilities

| Method | Arg | Return |
|---|---|---|
| `msg.send`   | `from\|to\|subject\|body\|thread_id\|reply_to\|ttl_secs\|origin_surface` | `<message_id>\n` |
| `msg.inbox`  | `subject_id\|limit\|include_read\|since_message_id` | tab rows + `count=N\n` |
| `msg.read`   | `message_id\|reader_subject_id` | `ok\n` |
| `msg.thread` | `thread_id\|subject_id` | tab rows (oldest-first) + `count=N\n` |
| `msg.delete` | `message_id\|subject_id` | `ok\n` |

Row format (8 tab-separated columns):
`message_id \t thread_id \t from \t subject \t body_preview \t sent_at \t read_at \t status`.

Body preview is truncated to 80 chars and stripped of tabs /
newlines so each row stays parseable. Read_at is `-1` for
unread rows.

## Auto-expire

A background loop on the coordinator (5-minute tick) flips
every non-expired row whose `sent_at + ttl_secs <= now` to
`status = expired`. Soft-deleted rows are already
`expired` so the sweeper leaves them alone.

The sweeper runs alongside the agent-approval auto-expire
loop. Both fail closed — a sweep error logs WARN and the
loop keeps ticking on the next interval.

## Audit and security

- **No body in the chronicle.** Each successful send writes one
  `msg.sent` event to a coordinator-side `msg-bookkeeping-system`
  task; payload is `from=<short>|to=<short>|thread=<id>`. The
  message body, subject, and recipient list never appear in
  the audit log.
- **Recipient-only reads.** `msg.read` verifies the reader is
  the `to_subject_id`; anyone else gets `INVALID_ARGS`.
- **Participant-only thread access.** `msg.thread` requires the
  caller to be sender or recipient on at least one message in
  the thread.
- **Participant-only delete.** Only sender or recipient may
  soft-delete; a third party calling `msg.delete` gets
  `INVALID_ARGS`.
- **`|` rejected in fields.** All fields except body
  could-in-theory tolerate a pipe, but the wire format uses
  `|` as a separator. The bridge rejects `|` in every field
  on `POST /v1/messages` with a 400 to keep the audit
  trail from carrying ambiguous rows.

## What's deliberately out of scope

- **Push delivery.** Messaging is poll-based. A long-poll /
  SSE feed against `/v1/messages/inbox` would be a separate
  effort.
- **Attachments.** The body column is text. Binary payloads
  belong in a separate object-store capability.
- **Read receipts back to the sender.** Read state is local
  to the recipient's inbox; senders see no notification when
  the recipient calls `msg.read`. Polling `msg.thread` is
  the workaround.
- **Multi-recipient.** Each `msg.send` targets exactly one
  recipient. Broadcasting is the caller's job (send N times
  with the same thread_id).
