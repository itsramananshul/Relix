import { useRef, useState } from "react";
import { Link } from "react-router-dom";
import { api } from "../api";

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
  ai_status?: string;
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
interface CompanionResponse { action?: string; reply?: string; result?: unknown }
// Start-to-Shift (POST /v1/spine/prime/start).
interface StartedShift { brief_id?: string; run_id?: string; rig?: string; status?: string }
interface SkippedBrief { brief_id?: string; reason?: string }
interface StartResponse {
  proposal_id?: string;
  mandate_id?: string;
  started?: StartedShift[];
  skipped?: SkippedBrief[];
}

// A chat-log entry is plain text, a Prime proposal card, an approval result,
// or a Start-to-Shift result.
type Entry =
  | { role: "user" | "assistant"; kind: "text"; text: string }
  | { role: "assistant"; kind: "proposal"; data: ProposalResponse; done?: boolean }
  | { role: "assistant"; kind: "approved"; data: ApproveResponse; done?: boolean }
  | { role: "assistant"; kind: "started"; data: StartResponse };

const INTENT_TONE: Record<string, string> = {
  build: "done",
  fix: "blocked",
  research: "in_progress",
  generic: "todo",
};

export function Chat() {
  const [log, setLog] = useState<Entry[]>([
    {
      role: "assistant",
      kind: "text",
      text:
        "I'm Prime, your company planner. Describe what you want to build and I'll propose a plan — a Mandate, the crew roles, suggested hires, and a Brief breakdown — for you to approve. Nothing is created or run until you approve. (Or use the quick commands: \"create a brief …\", \"move … to …\", \"board\".)",
    },
  ]);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const logRef = useRef<HTMLDivElement>(null);

  function scroll() {
    requestAnimationFrame(() => logRef.current?.scrollTo(0, logRef.current.scrollHeight));
  }

  // "Plan with Prime" — propose a governed plan (creates nothing).
  async function plan() {
    const message = text.trim();
    if (!message || busy) return;
    setText("");
    setLog((l) => [...l, { role: "user", kind: "text", text: message }]);
    setBusy(true);
    try {
      const res = await api.post<ProposalResponse>("/v1/spine/prime/propose", { message });
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

  // Quick companion command (the rule-based single-action path).
  async function send() {
    const message = text.trim();
    if (!message || busy) return;
    setText("");
    setLog((l) => [...l, { role: "user", kind: "text", text: message }]);
    setBusy(true);
    try {
      const res = await api.post<CompanionResponse>("/v1/spine/companion", { message });
      const reply =
        res.reply ||
        (res.action ? `Done: ${res.action}` : "OK.") +
          (res.result ? "\n\n" + JSON.stringify(res.result, null, 2) : "");
      setLog((l) => [...l, { role: "assistant", kind: "text", text: reply }]);
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
      <div className="chat-log" ref={logRef}>
        {log.map((m, i) => {
          if (m.kind === "text") {
            return (
              <div key={i} className={"msg " + m.role} style={{ whiteSpace: "pre-wrap" }}>
                {m.text}
              </div>
            );
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
      <div className="chat-input">
        <input
          className="input"
          placeholder="Describe what you want to build, or type a quick command…"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && plan()}
        />
        <button className="btn" onClick={plan} disabled={busy || !text.trim()} title="Propose a governed plan (creates nothing until you approve)">
          Plan with Prime
        </button>
        <button className="btn ghost" onClick={send} disabled={busy || !text.trim()} title="Run a single quick command">
          Command
        </button>
      </div>
    </div>
  );
}

function ProposalCard({ entry, onApprove, busy }: { entry: { data: ProposalResponse; done?: boolean }; onApprove: () => void; busy: boolean }) {
  const p = entry.data.proposal ?? {};
  const missing = (p.hires ?? []).length;
  return (
    <div className="msg assistant" style={{ maxWidth: "100%" }}>
      <div className="card" style={{ margin: 0 }}>
        <div className="row" style={{ gap: 8, alignItems: "center" }}>
          <span className={"badge " + (INTENT_TONE[p.intent ?? ""] ?? "todo")} style={{ fontSize: 9 }}>{p.intent ?? "plan"}</span>
          <strong>{p.summary ?? "Proposed plan"}</strong>
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

        {/* AI honesty */}
        {p.ai_used === false && (
          <div className="muted" style={{ fontSize: 10, marginTop: 6, fontStyle: "italic" }}>{p.ai_status}</div>
        )}

        {/* Actions */}
        <div className="row" style={{ gap: 8, marginTop: 10 }}>
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

function ApprovedCard({ entry, onStart, busy }: { entry: { data: ApproveResponse; done?: boolean }; onStart: () => void; busy: boolean }) {
  const data = entry.data;
  const assigned = (data.assigned_briefs ?? []).length;
  return (
    <div className="msg assistant" style={{ maxWidth: "100%" }}>
      <div className="card" style={{ margin: 0 }}>
        <div className="row" style={{ gap: 8 }}>
          <span className="badge done">created</span>
          <strong>{data.already_approved ? "Already approved" : "Plan approved"}</strong>
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
      </div>
    </div>
  );
}

function StartedCard({ data }: { data: StartResponse }) {
  const started = data.started ?? [];
  const skipped = data.skipped ?? [];
  return (
    <div className="msg assistant" style={{ maxWidth: "100%" }}>
      <div className="card" style={{ margin: 0 }}>
        <div className="row" style={{ gap: 8 }}>
          <span className={"badge " + (started.length > 0 ? "in_progress" : "todo")}>
            {started.length > 0 ? "running" : "nothing started"}
          </span>
          <strong>{started.length} Shift(s) started</strong>
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
