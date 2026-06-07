import { useCallback, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import {
  api,
  primeDriver,
  subscribePrimeStatus,
  type PrimeNextStep,
  type StatusStreamConn,
} from "../api";

// ── Prime Assistant proposal shapes (from /v1/spine/prime/propose) ──────────
interface CrewSlot { role?: string; have?: boolean; agent_name?: string }
interface HireSuggestion { role?: string; title?: string; reason?: string }
interface ProposedBrief { key?: string; title?: string; role?: string; depends_on?: string[] }
interface ProposalPlan {
  intent?: string;
  summary?: string;
  mandate_title?: string;
  roles?: string[];
  crew?: CrewSlot[];
  hires?: HireSuggestion[];
  briefs?: ProposedBrief[];
  risks?: string[];
  next_actions?: string[];
  ai_used?: boolean;
  /** "deterministic_only" | "llm_used" | "fallback" | "unavailable" */
  ai_mode?: string;
  ai_status?: string;
  ai_reason?: string;
}
interface ProposalResponse { proposal_id?: string; status?: string; proposal?: ProposalPlan }
interface ApproveResponse {
  proposal_id?: string;
  mandate_id?: string;
  created_briefs?: string[];
  assigned_briefs?: string[];
  hire_requests?: string[];
  already_approved?: boolean;
}
interface CompanionResponse {
  action?: string;
  reply?: string;
  result?: unknown;
  /** "llm_used" | "fallback" | "unavailable" (only present for mode:"ai") */
  ai_mode?: string;
  ai_used?: boolean;
  ai_reason?: string;
}
// Start-to-Shift (POST /v1/spine/prime/start).
interface StartedShift { brief_id?: string; run_id?: string; rig?: string; status?: string }
interface SkippedBrief { brief_id?: string; reason?: string }
interface StartResponse {
  proposal_id?: string;
  mandate_id?: string;
  started?: StartedShift[];
  skipped?: SkippedBrief[];
}

// Live Shift Room (GET /v1/spine/prime/proposals/:id/status) — the command
// center for one Prime work session. Polled after start (no per-session SSE).
interface StatusBlocker { brief_id?: string; title?: string; status?: string }
interface StatusRun {
  run_id?: string;
  status?: string;
  rig?: string;
  trigger?: string;
  review?: string;
  apply_status?: string;
  refusal_reason?: string;
  summary?: string;
}
interface StatusBrief {
  brief_id?: string;
  title?: string;
  board_status?: string;
  assignee?: string | null;
  rig?: string | null;
  start_readiness?: string;
  blockers?: StatusBlocker[];
  needs_review?: boolean;
  latest_run?: StatusRun | null;
  next_action?: string;
  exists?: boolean;
}
interface StatusCounts {
  total_briefs?: number;
  running?: number;
  done?: number;
  blocked?: number;
  needs_review?: number;
  refused?: number;
  failed?: number;
  ready?: number;
  unassigned?: number;
  not_ready?: number;
  missing?: number;
}
interface SessionStatus {
  proposal_id?: string;
  status?: string;
  mandate_id?: string | null;
  mandate_title?: string | null;
  briefs?: StatusBrief[];
  counts?: StatusCounts;
  recommended_next_actions?: string[];
}

// Tone for a Brief's Start-readiness / live state badge.
const READINESS_TONE: Record<string, string> = {
  ready: "todo",
  running: "in_progress",
  needs_review: "in_review",
  done: "done",
  blocked: "blocked",
  unassigned: "backlog",
  refused: "blocked",
  failed: "blocked",
  not_ready: "backlog",
  missing: "blocked",
};

// A chat-log entry is plain text, a Prime proposal card, an approval result,
// a Start-to-Shift result, or a single companion-action result.
type Entry =
  | { role: "user" | "assistant"; kind: "text"; text: string }
  | { role: "assistant"; kind: "companion"; data: CompanionResponse }
  | { role: "assistant"; kind: "proposal"; data: ProposalResponse; done?: boolean }
  | { role: "assistant"; kind: "approved"; data: ApproveResponse; done?: boolean }
  | { role: "assistant"; kind: "started"; data: StartResponse };

// ── Persistent local chat session (dashboard-design §13) ────────────────────
// The chat log is kept in the browser under a versioned key so a refresh does
// NOT wipe the conversation. This is LOCAL UI HISTORY ONLY — it is not the
// server/audit record (every governed action still lands in the Chronicle).
// We persist only safe UI data already shown on the page (ids, titles, counts,
// replies) — never tokens/passwords/secrets — and keep a hard cap so the store
// can't grow without bound. A corrupt/foreign value resets cleanly.
const CHAT_STORAGE_KEY = "relix.chat.v1";
const CHAT_MAX_ENTRIES = 50;
const GREETING: Entry = {
  role: "assistant",
  kind: "text",
  text:
    "I'm Prime, your company planner. Describe a goal and I'll propose a governed plan — Mandate, crew, hires, and Briefs — for you to approve. Nothing is created or run until you approve.",
};

function isEntry(e: unknown): e is Entry {
  if (!e || typeof e !== "object") return false;
  const o = e as Record<string, unknown>;
  return (o.role === "user" || o.role === "assistant") && typeof o.kind === "string";
}

// Load the persisted log; reset cleanly to the greeting on any parse/shape error.
function loadLog(): Entry[] {
  try {
    const raw = typeof localStorage !== "undefined" ? localStorage.getItem(CHAT_STORAGE_KEY) : null;
    if (!raw) return [GREETING];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [GREETING];
    const clean = parsed.filter(isEntry).slice(-CHAT_MAX_ENTRIES);
    return clean.length > 0 ? clean : [GREETING];
  } catch {
    try {
      localStorage.removeItem(CHAT_STORAGE_KEY);
    } catch {
      /* storage unavailable — fall through to the in-memory greeting */
    }
    return [GREETING];
  }
}

// Quick-action chips (dashboard-design §13: "command discoverability"). Safe
// READ commands (`send:"command"`) dispatch straight to the governed companion
// route; WRITE templates only POPULATE the input so the operator edits the
// real ids/text before sending — the chat never fires a write blind.
type Chip = { label: string; text: string; send?: "command" };
const QUICK_CHIPS: Chip[] = [
  { label: "What needs attention", text: "what needs attention", send: "command" },
  { label: "What is blocked", text: "what is blocked", send: "command" },
  { label: "What is running", text: "what is running", send: "command" },
  { label: "Who is on the crew", text: "who is on the crew", send: "command" },
  { label: "Create brief", text: "create brief: " },
  { label: "Plan package", text: "plan package <brief_id>: <goal> => child: <step>; child high: <step>" },
];

// Inferred, SAFE route hand-off for a companion action (dashboard-design §13:
// "the chat hands off to the board"). Maps the validated action name to the
// surface where that work is seen — never a mutation.
const COMPANION_ROUTES: Record<string, { to: string; label: string }> = {
  attention: { to: "/", label: "Open The Desk →" },
  blocked: { to: "/briefs", label: "Open board →" },
  running: { to: "/runs", label: "Open Shifts →" },
  roster: { to: "/agents", label: "Open Crew →" },
  overdue: { to: "/briefs", label: "Open board →" },
  board: { to: "/briefs", label: "Open board →" },
  search: { to: "/briefs", label: "Open board →" },
  create_brief: { to: "/briefs", label: "Open board →" },
  create_mandate: { to: "/mandates", label: "Open Mandates →" },
  move: { to: "/briefs", label: "Open board →" },
  assign: { to: "/briefs", label: "Open board →" },
  pin: { to: "/briefs", label: "Open board →" },
  comment: { to: "/briefs", label: "Open board →" },
  plan_package: { to: "/briefs", label: "Open board →" },
};

const INTENT_TONE: Record<string, string> = {
  build: "done",
  fix: "blocked",
  research: "in_progress",
  generic: "todo",
};

export function Chat() {
  // Hydrate from the persisted local session so a refresh keeps the conversation.
  const [log, setLog] = useState<Entry[]>(loadLog);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  // Opt into the model-assisted Prime planner (company-model §12.5A). Off by
  // default — the plan stays rule-based unless the operator asks for AI, and
  // even then it falls back deterministically if no model is reachable.
  const [useAi, setUseAi] = useState(false);
  const logRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // Persist the (capped) log after every change. Best-effort — a storage
  // failure (private mode / quota) must never break the chat.
  useEffect(() => {
    try {
      localStorage.setItem(CHAT_STORAGE_KEY, JSON.stringify(log.slice(-CHAT_MAX_ENTRIES)));
    } catch {
      /* storage unavailable — the chat still works in-memory for this session */
    }
  }, [log]);

  function scroll() {
    requestAnimationFrame(() => logRef.current?.scrollTo(0, logRef.current.scrollHeight));
  }

  // Wipe the LOCAL history and restore the initial Prime greeting. Confirmed,
  // because it discards the visible conversation (not the server Chronicle).
  function clearChat() {
    if (busy) return;
    if (!window.confirm("Clear this local chat history? Governed actions stay in the Chronicle.")) return;
    setLog([GREETING]);
    scroll();
  }

  // A chip either dispatches a safe read command directly, or populates the
  // input with a write template for the operator to edit first.
  function onChip(c: Chip) {
    if (busy) return;
    if (c.send === "command") {
      send(c.text);
    } else {
      setText(c.text);
      inputRef.current?.focus();
    }
  }

  // "Plan with Prime" — propose a governed plan (creates nothing). An explicit
  // `override` (from a chip) is used verbatim and leaves the operator's draft.
  async function plan(override?: string) {
    const fromInput = override === undefined;
    const message = (override ?? text).trim();
    if (!message || busy) return;
    if (fromInput) setText("");
    setLog((l) => [...l, { role: "user", kind: "text", text: message }]);
    setBusy(true);
    try {
      const body = useAi ? { message, mode: "ai" } : { message };
      const res = await api.post<ProposalResponse>("/v1/spine/prime/propose", body);
      setLog((l) => [...l, { role: "assistant", kind: "proposal", data: res }]);
    } catch (e) {
      setLog((l) => [
        ...l,
        { role: "assistant", kind: "text", text: "⚠ " + (e instanceof Error ? e.message : "propose failed") },
      ]);
    } finally {
      setBusy(false);
      scroll();
    }
  }

  // Approve a proposal — the ONLY path that creates the Mandate + Briefs.
  async function approve(proposalId: string, idx: number) {
    setBusy(true);
    try {
      const res = await api.post<ApproveResponse>("/v1/spine/prime/approve", { proposal_id: proposalId });
      setLog((l) => {
        const next = [...l];
        const p = next[idx];
        if (p && p.kind === "proposal") next[idx] = { ...p, done: true };
        next.push({ role: "assistant", kind: "approved", data: res });
        return next;
      });
    } catch (e) {
      setLog((l) => [
        ...l,
        { role: "assistant", kind: "text", text: "⚠ " + (e instanceof Error ? e.message : "approve failed") },
      ]);
    } finally {
      setBusy(false);
      scroll();
    }
  }

  // Start the work — turn the approved Mandate's READY Briefs into real Shifts
  // (the same governed run path as a manual Brief run). Nothing here is
  // created; it only RUNS Briefs that are already assigned + ready.
  async function start(proposalId: string, idx: number) {
    setBusy(true);
    try {
      const res = await api.post<StartResponse>("/v1/spine/prime/start", { proposal_id: proposalId });
      setLog((l) => {
        const next = [...l];
        const p = next[idx];
        if (p && p.kind === "approved") next[idx] = { ...p, done: true };
        next.push({ role: "assistant", kind: "started", data: res });
        return next;
      });
    } catch (e) {
      setLog((l) => [
        ...l,
        { role: "assistant", kind: "text", text: "⚠ " + (e instanceof Error ? e.message : "start failed") },
      ]);
    } finally {
      setBusy(false);
      scroll();
    }
  }

  // Quick companion command (single governed action). With "Use AI" checked the
  // model SELECTS the action (validated server-side into the same governed path);
  // otherwise the deterministic parser chooses it. Either way it's one action.
  async function send(override?: string) {
    const fromInput = override === undefined;
    const message = (override ?? text).trim();
    if (!message || busy) return;
    if (fromInput) setText("");
    setLog((l) => [...l, { role: "user", kind: "text", text: message }]);
    setBusy(true);
    try {
      const body = useAi ? { message, mode: "ai" } : { message };
      const res = await api.post<CompanionResponse>("/v1/spine/companion", body);
      // Render a compact result card (action chip + reply + provenance + route
      // hand-off + raw disclosure) rather than dumping JSON into the bubble.
      setLog((l) => [...l, { role: "assistant", kind: "companion", data: res }]);
    } catch (e) {
      setLog((l) => [
        ...l,
        { role: "assistant", kind: "text", text: "⚠ " + (e instanceof Error ? e.message : "failed") },
      ]);
    } finally {
      setBusy(false);
      scroll();
    }
  }

  return (
    <div className="chat" style={{ height: "calc(100vh - 96px)" }}>
      <div className="chat-bar">
        <span className="muted" style={{ fontSize: 11 }} title="Kept in this browser only — not the server/audit record. Governed actions land in the Chronicle.">
          Local history · this browser only
        </span>
        <div className="spacer" />
        <button className="btn ghost sm" onClick={clearChat} disabled={busy} title="Clear this local conversation and restore the Prime greeting (does not touch the Chronicle)">
          Clear chat
        </button>
      </div>
      <div className="chat-log" ref={logRef}>
        {log.map((m, i) => {
          if (m.kind === "text") {
            return (
              <div key={i} className={"msg " + m.role} style={{ whiteSpace: "pre-wrap" }}>
                {m.text}
              </div>
            );
          }
          if (m.kind === "companion") {
            return <CompanionCard key={i} data={m.data} />;
          }
          if (m.kind === "proposal") {
            return <ProposalCard key={i} entry={m} onApprove={() => m.data.proposal_id && approve(m.data.proposal_id, i)} busy={busy} />;
          }
          if (m.kind === "approved") {
            return <ApprovedCard key={i} entry={m} onStart={() => m.data.proposal_id && start(m.data.proposal_id, i)} busy={busy} />;
          }
          return <StartedCard key={i} data={m.data} />;
        })}
        {busy && <div className="msg assistant muted">…thinking</div>}
      </div>
      {/* Command discoverability — read chips fire safely; write templates fill the input. */}
      <div className="chat-chips">
        {QUICK_CHIPS.map((c) => (
          <button
            key={c.label}
            className="chip"
            disabled={busy}
            onClick={() => onChip(c)}
            title={c.send === "command" ? "Run this read command" : "Fill the input with a template to edit, then send"}
          >
            {c.label}
          </button>
        ))}
      </div>
      <div className="chat-input">
        <input
          ref={inputRef}
          className="input"
          placeholder="Describe what you want to build, or type a quick command…"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key !== "Enter") return;
            // Enter = Plan with Prime (the established default); Ctrl/⌘+Enter = Command.
            if (e.ctrlKey || e.metaKey) {
              e.preventDefault();
              send();
            } else {
              plan();
            }
          }}
        />
        <label
          className="muted"
          style={{ fontSize: 11, display: "flex", alignItems: "center", gap: 4, whiteSpace: "nowrap" }}
          title="Draft the plan with a language model. Validated + crew-matched server-side; falls back to the rule-based plan if no model is reachable."
        >
          <input type="checkbox" checked={useAi} onChange={(e) => setUseAi(e.target.checked)} disabled={busy} />
          Use AI
        </label>
        <button className="btn" onClick={() => plan()} disabled={busy || !text.trim()} title="Propose a governed plan (Enter) — creates nothing until you approve">
          Plan with Prime
        </button>
        <button className="btn ghost" onClick={() => send()} disabled={busy || !text.trim()} title="Run a single quick command (Ctrl/⌘+Enter)">
          Command
        </button>
      </div>
    </div>
  );
}

// Compact companion-action result (dashboard-design §13). Renders an action
// chip, the reply text, an honest AI-provenance chip when present, a safe
// route hand-off to the board, and the raw result behind a disclosure — never
// a raw JSON dump in the bubble.
function CompanionCard({ data }: { data: CompanionResponse }) {
  const action = data.action;
  const reply = data.reply || (action ? `Done: ${action}` : "OK.");
  const route = action ? COMPANION_ROUTES[action] : undefined;
  const hasResult = data.result !== undefined && data.result !== null;
  return (
    <div className="panel-msg">
      <div className="panel">
        <div className="panel-head">
          {action && <span className="badge todo" style={{ fontSize: 9 }}>{action}</span>}
          <span className="panel-title">Companion</span>
          <div className="spacer" style={{ flex: 1 }} />
          <CompanionAiBadge mode={data.ai_mode} aiUsed={data.ai_used} reason={data.ai_reason} />
        </div>
        <div style={{ fontSize: 13, marginTop: 8, whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{reply}</div>
        {route && (
          <div className="chat-links">
            <Link to={route.to} className="link" style={{ fontSize: 12 }}>{route.label}</Link>
          </div>
        )}
        {hasResult && (
          <details className="chat-raw">
            <summary>Raw result</summary>
            <pre>{JSON.stringify(data.result, null, 2)}</pre>
          </details>
        )}
      </div>
    </div>
  );
}

// Honest provenance for a companion action. The `ai_*` fields are present ONLY
// for `mode:"ai"`; the deterministic path emits none, so this renders nothing.
function CompanionAiBadge({ mode, aiUsed, reason }: { mode?: string; aiUsed?: boolean; reason?: string }) {
  const m = mode ?? (aiUsed ? "llm_used" : "");
  const META: Record<string, { tone: string; label: string }> = {
    llm_used: { tone: "done", label: "AI-selected" },
    fallback: { tone: "backlog", label: "AI → rule-based" },
    unavailable: { tone: "backlog", label: "AI unavailable → rule-based" },
  };
  const meta = META[m];
  if (!meta) return null;
  return (
    <span className={"badge " + meta.tone} style={{ fontSize: 9 }} title={reason}>
      {meta.label}
    </span>
  );
}

function ProposalCard({ entry, onApprove, busy }: { entry: { data: ProposalResponse; done?: boolean }; onApprove: () => void; busy: boolean }) {
  const p = entry.data.proposal ?? {};
  const missing = (p.hires ?? []).length;
  return (
    <div className="panel-msg">
      <div className="panel">
        <div className="panel-head">
          <span className={"badge " + (INTENT_TONE[p.intent ?? ""] ?? "todo")} style={{ fontSize: 9 }}>{p.intent ?? "plan"}</span>
          <span className="panel-title">{p.summary ?? "Proposed plan"}</span>
          <div className="spacer" style={{ flex: 1 }} />
          <AiStatusBadge mode={p.ai_mode} aiUsed={p.ai_used} status={p.ai_status} reason={p.ai_reason} />
        </div>
        <div className="muted" style={{ fontSize: 11, marginTop: 4 }}>
          Mandate: <strong>{p.mandate_title}</strong>
        </div>

        {/* Crew + hires */}
        <div className="row wrap" style={{ gap: 6, marginTop: 8 }}>
          {(p.crew ?? []).map((c, i) => (
            <span key={i} className={"badge " + (c.have ? "done" : "backlog")} style={{ fontSize: 9 }} title={c.have ? `filled by ${c.agent_name}` : "needs a hire"}>
              {c.role}{c.have ? ` · ${c.agent_name}` : " · missing"}
            </span>
          ))}
        </div>

        {/* Brief breakdown */}
        <div style={{ marginTop: 8 }}>
          <div className="muted" style={{ fontSize: 11, marginBottom: 2 }}>Proposed Briefs ({(p.briefs ?? []).length})</div>
          {(p.briefs ?? []).map((b, i) => (
            <div key={i} style={{ fontSize: 12, padding: "1px 0" }}>
              • {b.title}
              {(b.depends_on?.length ?? 0) > 0 && <span className="muted" style={{ fontSize: 10 }}> (after {b.depends_on!.length} track{b.depends_on!.length === 1 ? "" : "s"})</span>}
            </div>
          ))}
        </div>

        {/* Governance: hires need Clearance */}
        {missing > 0 && (
          <div className="banner info" style={{ fontSize: 11, marginTop: 8 }}>
            ⚠ {missing} role(s) need hiring. Approving files <strong>pending</strong> hire requests that still need a <strong>Clearance</strong> to activate — no agent runs until then.
          </div>
        )}

        {/* Risks */}
        {(p.risks ?? []).length > 0 && (
          <div style={{ marginTop: 6 }}>
            {(p.risks ?? []).map((r, i) => (
              <div key={i} className="muted" style={{ fontSize: 11 }}>• {r}</div>
            ))}
          </div>
        )}

        {/* Actions */}
        <div className="panel-section row" style={{ gap: 8 }}>
          {entry.done ? (
            <span className="badge done">approved ✓</span>
          ) : (
            <button className="btn" onClick={onApprove} disabled={busy} title="Create the Mandate + Briefs + pending hire requests. Nothing runs automatically.">
              Approve &amp; create
            </button>
          )}
          <span className="muted" style={{ fontSize: 11 }}>Nothing runs automatically — approve, then <strong>Start the work</strong>.</span>
        </div>
      </div>
    </div>
  );
}

// Compact, honest provenance for a Prime plan. One small badge keyed on the
// machine `ai_mode`; the full `ai_status` (and any fallback reason) is on hover
// so the card stays uncluttered.
function AiStatusBadge({ mode, aiUsed, status, reason }: { mode?: string; aiUsed?: boolean; status?: string; reason?: string }) {
  const m = mode ?? (aiUsed ? "llm_used" : "deterministic_only");
  const META: Record<string, { tone: string; label: string }> = {
    llm_used: { tone: "done", label: "AI-drafted · validated" },
    fallback: { tone: "backlog", label: "AI rejected → rule-based" },
    unavailable: { tone: "backlog", label: "AI unavailable → rule-based" },
    deterministic_only: { tone: "todo", label: "rule-based" },
  };
  const meta = META[m] ?? META.deterministic_only;
  const title = (status ?? "") + (reason ? `\n\nReason: ${reason}` : "");
  // Rendered inline in the proposal's .panel-head flex row, so it must be an
  // inline badge (no block wrapper / top margin) to stay aligned + right-anchored.
  return (
    <span className={"badge " + meta.tone} style={{ fontSize: 9 }} title={title}>
      {meta.label}
    </span>
  );
}

function ApprovedCard({ entry, onStart, busy }: { entry: { data: ApproveResponse; done?: boolean }; onStart: () => void; busy: boolean }) {
  const data = entry.data;
  const assigned = (data.assigned_briefs ?? []).length;
  return (
    <div className="panel-msg">
      <div className="panel">
        <div className="panel-head">
          <span className="badge done">created</span>
          <span className="panel-title">{data.already_approved ? "Already approved" : "Plan approved"}</span>
        </div>
        <div style={{ fontSize: 12, marginTop: 6 }}>
          <div>Mandate <span className="mono">{(data.mandate_id ?? "").slice(0, 14)}</span> · <Link to="/mandates" className="link">open Mandates →</Link></div>
          <div style={{ marginTop: 2 }}>{(data.created_briefs ?? []).length} Brief(s) created · {assigned} assigned to existing crew · <Link to="/briefs" className="link">view board →</Link></div>
          {(data.hire_requests ?? []).length > 0 && (
            <div style={{ marginTop: 2 }}>
              {(data.hire_requests ?? []).length} pending hire request(s) — <Link to="/mandates" className="link">approve Clearances →</Link>
            </div>
          )}
        </div>
        {/* Start-to-Shift: run the ready Briefs now (same governed path as a
            manual run). Unassigned / blocked Briefs are reported, not run. */}
        <div className="row" style={{ gap: 8, marginTop: 10, alignItems: "center" }}>
          {entry.done ? (
            <span className="badge done">started ✓</span>
          ) : (
            <button className="btn" onClick={onStart} disabled={busy} title="Start the ready Briefs — turns each into a real Shift through the same governed run path as a manual run. Unassigned or blocked Briefs are reported, not run.">
              Start the work
            </button>
          )}
          <span className="muted" style={{ fontSize: 11 }}>
            {assigned > 0
              ? `${assigned} assigned Brief(s) are ready to run.`
              : "No assigned Briefs yet — greenlight Clearances first, then start."}
          </span>
        </div>

        {/* Live Shift Room — the command center for this work session. It polls
            the session status (and refreshes on run events) so the operator
            sees what started, ran, finished, blocked, or needs review. */}
        {data.proposal_id && <ShiftRoom proposalId={data.proposal_id} />}
      </div>
    </div>
  );
}

// ── Live Shift Room ─────────────────────────────────────────────────────────
// Given an approved proposal id, render the live status of every created Brief
// with its latest Shift + a concrete next action. PREFERS the dedicated status
// stream (server pushes the snapshot + every change); FALLS BACK to polling the
// status snapshot whenever the stream isn't carrying us (connecting / retrying /
// unsupported). The header badge is honest: it only says "live" when the stream
// is actually connected.
function ShiftRoom({ proposalId }: { proposalId: string }) {
  const [status, setStatus] = useState<SessionStatus | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [acting, setActing] = useState<string | null>(null);
  const [conn, setConn] = useState<StatusStreamConn>("connecting");
  const [nextStep, setNextStep] = useState<PrimeNextStep | null>(null);
  const [advancing, setAdvancing] = useState(false);

  // The guided-driver next step — READ-ONLY; refreshed alongside status.
  const loadNextStep = useCallback(async () => {
    try {
      setNextStep(await primeDriver.nextStep(proposalId));
    } catch {
      // The driver is an optional overlay — a failure here must not blank the
      // Shift Room. Leave the prior next step (or none).
    }
  }, [proposalId]);

  const refresh = useCallback(async () => {
    try {
      const s = await api.get<SessionStatus>(`/v1/spine/prime/proposals/${proposalId}/status`);
      setStatus(s);
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "status failed");
    }
    await loadNextStep();
  }, [proposalId, loadNextStep]);

  // Run ONE safe governed step the driver offered, then re-read. A stale request
  // (409) or a governance refusal just re-reads so the operator sees the truth —
  // the driver never fakes a success.
  async function advanceStep() {
    if (!nextStep?.can_advance || !nextStep.advance_action) return;
    setAdvancing(true);
    try {
      await primeDriver.advance(proposalId, nextStep.advance_action);
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "advance failed");
    } finally {
      await refresh();
      setAdvancing(false);
    }
  }

  // One immediate fetch for instant data + a fallback if the stream never
  // connects (e.g. SSE unsupported behind a proxy).
  useEffect(() => {
    refresh();
  }, [refresh]);

  // Prefer the dedicated status stream: the server pushes the initial snapshot
  // and every change. A terminal `not_found` surfaces an honest message.
  useEffect(
    () =>
      subscribePrimeStatus(
        proposalId,
        (s) => {
          setStatus(s as SessionStatus);
          setErr(null);
          // Keep the driver's next step in lock-step with each pushed change.
          loadNextStep();
        },
        (c) => setConn(c),
        () => setErr("This work session is no longer available."),
      ),
    [proposalId],
  );

  // Poll fallback ONLY while the stream isn't live — covers connecting,
  // reconnecting, and SSE-unsupported. When live, the stream carries us and we
  // don't poll at all.
  const live = conn === "live";
  useEffect(() => {
    if (live) return;
    const t = setInterval(refresh, 4000);
    return () => clearInterval(t);
  }, [live, refresh]);

  async function runBrief(id: string) {
    setActing(id);
    try {
      await api.post(`/v1/spine/briefs/${id}/run`, {});
      await refresh();
    } catch (e) {
      setErr(e instanceof Error ? e.message : "run failed");
    } finally {
      setActing(null);
    }
  }

  const counts = status?.counts ?? {};
  const briefs = status?.briefs ?? [];
  return (
    <div className="panel-section">
      <div className="row" style={{ gap: 8, alignItems: "center" }}>
        <strong style={{ fontSize: 13 }}>Shift Room</strong>
        {status?.mandate_title && <span className="muted" style={{ fontSize: 11 }}>· {status.mandate_title}</span>}
        {/* Honest connection state — only "live" when the stream is connected. */}
        <span
          className={"badge " + (live ? "done" : "backlog")}
          style={{ fontSize: 9 }}
          title={
            live
              ? "Live — pushed by the session status stream"
              : "Polling — the status stream is unavailable; refreshing every few seconds"
          }
        >
          {live ? "live" : "polling"}
        </span>
        <div className="spacer" style={{ flex: 1 }} />
        <button className="btn ghost" style={{ fontSize: 11, padding: "2px 8px" }} onClick={refresh}>
          Refresh
        </button>
      </div>

      {/* Prime guided driver — the ONE next governed step. When the driver may
          safely take it (create the Team Plan / orchestrate-assign a ready team),
          a restrained "Advance one step" runs it through the existing gated
          route. Otherwise it shows the route to take by hand — approvals, hires,
          and Start stay explicit operator decisions. */}
      {nextStep && (
        <div className="shift-room-row" style={{ marginTop: 8, alignItems: "flex-start" }}>
          <span className="badge todo" style={{ fontSize: 9, minWidth: 64, textAlign: "center" }}>
            next step
          </span>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontSize: 12 }}>{nextStep.label}</div>
            <div className="muted" style={{ fontSize: 10 }}>{nextStep.reason}</div>
            {!nextStep.can_advance && nextStep.route && (
              <div className="muted" style={{ fontSize: 10, marginTop: 2 }}>
                Route: <span className="mono">{nextStep.route}</span>
              </div>
            )}
          </div>
          {nextStep.can_advance && nextStep.advance_action ? (
            <button
              className="btn"
              style={{ fontSize: 11, padding: "2px 8px" }}
              disabled={advancing}
              onClick={advanceStep}
              title="Run this one governed step now through the existing gated route. The driver advances at most one step and never approves a strategy, hire, or budget gate."
            >
              {advancing ? "Advancing…" : "Advance one step"}
            </button>
          ) : (
            <span className="muted" style={{ fontSize: 10 }}>operator step</span>
          )}
        </div>
      )}

      {/* Roll-up counts — only the non-zero buckets, so it reads at a glance. */}
      <div className="row wrap" style={{ gap: 6, marginTop: 8 }}>
        {([
          ["ready", "ready"],
          ["running", "running"],
          ["needs_review", "review"],
          ["done", "done"],
          ["blocked", "blocked"],
          ["unassigned", "unassigned"],
          ["failed", "failed"],
          ["refused", "refused"],
        ] as [keyof StatusCounts, string][])
          .filter(([k]) => (counts[k] ?? 0) > 0)
          .map(([k, label]) => (
            <span key={k} className={"badge " + (READINESS_TONE[k] ?? "todo")} style={{ fontSize: 9 }}>
              {counts[k]} {label}
            </span>
          ))}
        <span className="muted" style={{ fontSize: 11 }}>{counts.total_briefs ?? briefs.length} Brief(s)</span>
      </div>

      {err && <div className="banner err" style={{ fontSize: 11, marginTop: 6 }}>⚠ {err}</div>}

      {/* Per-Brief rows: state badge, assignee/rig, latest Shift, next action. */}
      <div style={{ marginTop: 8 }}>
        {briefs.map((b, i) => {
          const run = b.latest_run ?? null;
          const tone = READINESS_TONE[b.start_readiness ?? ""] ?? "todo";
          return (
            <div key={b.brief_id ?? i} className="shift-room-row">
              <span className={"badge " + tone} style={{ fontSize: 9, minWidth: 64, textAlign: "center" }}>
                {b.needs_review ? "review" : b.start_readiness}
              </span>
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ fontSize: 12, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{b.title}</div>
                <div className="muted" style={{ fontSize: 10 }}>
                  {b.assignee ? <>→ {b.rig ?? "no rig"}</> : "unassigned"}
                  {run && <> · Shift <span className="mono">{(run.run_id ?? "").slice(0, 10)}</span> {run.status}</>}
                  {(b.blockers?.length ?? 0) > 0 && <> · blocked on {b.blockers!.length}</>}
                </div>
              </div>
              {/* The single most useful next action for this Brief. */}
              {b.start_readiness === "ready" ? (
                <button className="btn" style={{ fontSize: 11, padding: "2px 8px" }} disabled={acting === b.brief_id} onClick={() => b.brief_id && runBrief(b.brief_id)}>
                  Run
                </button>
              ) : run?.run_id && (b.needs_review || run.status === "running" || run.status === "failed" || run.status === "refused") ? (
                <Link to={`/runs?run=${run.run_id}`} className="link" style={{ fontSize: 11 }}>
                  {b.needs_review ? "Review →" : "Inspect →"}
                </Link>
              ) : run?.run_id && run.review === "accepted" && run.apply_status !== "applied" ? (
                <Link to={`/runs?run=${run.run_id}`} className="link" style={{ fontSize: 11 }}>Apply →</Link>
              ) : b.start_readiness === "unassigned" ? (
                <Link to="/mandates" className="link" style={{ fontSize: 11 }}>Hire →</Link>
              ) : (
                <Link to="/briefs" className="link" style={{ fontSize: 11 }}>Open →</Link>
              )}
            </div>
          );
        })}
        {briefs.length === 0 && <div className="empty" style={{ fontSize: 12 }}>No Briefs in this session yet.</div>}
      </div>

      {/* Session-level recommended next actions. */}
      {(status?.recommended_next_actions?.length ?? 0) > 0 && (
        <div style={{ marginTop: 8 }}>
          {status!.recommended_next_actions!.map((a, i) => (
            <div key={i} className="muted" style={{ fontSize: 11 }}>• {a}</div>
          ))}
        </div>
      )}
    </div>
  );
}

function StartedCard({ data }: { data: StartResponse }) {
  const started = data.started ?? [];
  const skipped = data.skipped ?? [];
  return (
    <div className="panel-msg">
      <div className="panel">
        <div className="panel-head">
          <span className={"badge " + (started.length > 0 ? "in_progress" : "todo")}>
            {started.length > 0 ? "running" : "nothing started"}
          </span>
          <span className="panel-title">{started.length} Shift(s) started</span>
        </div>

        {started.length > 0 && (
          <div style={{ marginTop: 6 }}>
            {started.map((s, i) => (
              <div key={i} style={{ fontSize: 12, padding: "1px 0" }}>
                • Brief <span className="mono">{(s.brief_id ?? "").slice(0, 10)}</span> → Shift{" "}
                <span className="mono">{(s.run_id ?? "").slice(0, 14)}</span>
                <span className="muted"> on {s.rig} · {s.status}</span>
              </div>
            ))}
            <div className="muted" style={{ fontSize: 11, marginTop: 4 }}>
              Watch them finish on the <Link to="/briefs" className="link">board →</Link>
            </div>
          </div>
        )}

        {skipped.length > 0 && (
          <div style={{ marginTop: 8 }}>
            <div className="muted" style={{ fontSize: 11, marginBottom: 2 }}>Not started ({skipped.length})</div>
            {skipped.map((s, i) => (
              <div key={i} className="muted" style={{ fontSize: 11 }}>
                • <span className="mono">{(s.brief_id ?? "").slice(0, 10)}</span> — {s.reason}
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
