import { useEffect, useMemo, useRef, useState } from "react";
import { runControls, subscribeRunEvents, type RunEvent, type RunEventConn } from "../api";

// ── Run transcript renderer (relix-dashboard-design §8) ────────────────────
// A standalone, reusable renderer over the durable `run_events` transcript.
// Used on the Runs page (expanded run) and embedded in the Brief workroom
// Conversation for the active/latest run. It folds the flat adapter stream
// into typed, readable blocks (lifecycle, assistant/result message, grouped
// tool actions, permission-denied, usage/cost, error/stderr) in a "nice"
// view, with a "raw" compact dump for debugging. It fetches its own events,
// live-tails the run via the existing run-event SSE while it is `running`,
// and falls back to polling with an honest status chip when the stream is
// unavailable. All real data — never a fabricated card.

// Run-event kind → a small color cue (semantic only; the rest stays B&W).
const KIND_TONE: Record<string, string> = {
  error: "var(--err)",
  stderr: "var(--err)",
  permission_denied: "var(--err)",
  failed: "var(--err)",
  cancelled: "var(--err)",
  "brief.dispatch_failed": "var(--err)",
  "artifacts.scan_failed": "var(--err)",
  cancel_requested: "var(--warn)",
  "apply.conflicted": "var(--warn)",
  "apply.failed": "var(--err)",
  result: "var(--ok)",
  "apply.applied": "var(--ok)",
  assistant_message: "var(--info)",
  tool_use: "#c297ff",
  command: "#c297ff",
  file_change: "#c297ff",
};
function kindDot(kind?: string): string {
  return KIND_TONE[kind ?? ""] ?? "var(--text-faint)";
}

// Humanize a relix/codex lifecycle kind for the "nice" view.
const KIND_LABEL: Record<string, string> = {
  accepted: "accepted",
  workspace_prepared: "workspace ready",
  process_started: "process started",
  process_exited: "process exited",
  "artifacts.scan_started": "scanning changes",
  "artifacts.detected": "changes detected",
  "artifacts.scan_failed": "change-scan failed",
  result: "result",
  failed: "failed",
  continued: "continued",
  cancelled: "cancelled",
  cancel_requested: "cancel requested",
  thread_started: "thread started",
  turn_started: "turn started",
  turn_completed: "turn completed",
  "apply.plan": "apply plan",
  "apply.started": "apply started",
  "apply.applied": "applied",
  "apply.conflicted": "apply conflicted",
  "apply.failed": "apply failed",
  review: "review",
};
function kindLabel(kind?: string): string {
  return KIND_LABEL[kind ?? ""] ?? (kind ?? "event");
}

function ts(ev: RunEvent): string {
  return ev.ts ? new Date(ev.ts * 1000).toLocaleTimeString() : "";
}

// ── Block grouping ─────────────────────────────────────────────────────────
type Family = "lifecycle" | "assistant" | "tool" | "denied" | "usage" | "error";

function familyOf(ev: RunEvent): Family {
  const k = ev.kind ?? "";
  const src = ev.source ?? "";
  if (k === "permission_denied") return "denied";
  if (k === "usage" || src === "cost") return "usage";
  if (k === "error" || k === "stderr" || k.endsWith("scan_failed") || k.endsWith("dispatch_failed"))
    return "error";
  if (k === "tool_use" || k === "command" || k === "file_change") return "tool";
  // `result`/`assistant_message` from an ADAPTER is the model talking; the same
  // `result` kind from `relix` is the terminal lifecycle line — keep it in the
  // lifecycle rail so the model's answer isn't duplicated as a system note.
  if ((k === "assistant_message" || k === "result") && src !== "relix") return "assistant";
  return "lifecycle";
}

interface Block {
  family: Family;
  events: RunEvent[];
  key: string;
}

// Fold the flat stream into blocks: consecutive lifecycle / tool / error /
// usage events merge into one block (collapsible accordions for tool runs);
// each assistant message and each permission-denial stands on its own so the
// model's words and a denial read clearly.
function groupEvents(events: RunEvent[]): Block[] {
  const blocks: Block[] = [];
  // Families that fold a consecutive run into a single block (the rest —
  // assistant messages, denials — each stand alone so they read clearly).
  const foldFamilies = new Set<Family>(["lifecycle", "tool", "error", "usage"]);
  for (let i = 0; i < events.length; i++) {
    const ev = events[i];
    const fam = familyOf(ev);
    const last = blocks[blocks.length - 1];
    if (last && last.family === fam && foldFamilies.has(fam)) {
      last.events.push(ev);
    } else {
      blocks.push({ family: fam, events: [ev], key: `${ev.event_id ?? i}-${fam}` });
    }
  }
  return blocks;
}

// One collapsible tool-action accordion. Defaults collapsed when it holds more
// than two actions (progressive disclosure — design §12: "no log-worship").
function ToolBlock({ events }: { events: RunEvent[] }) {
  const [open, setOpen] = useState(events.length <= 2);
  return (
    <div className="xtr-tool">
      <button className="xtr-acc-head" onClick={() => setOpen((o) => !o)}>
        <span className="xtr-caret">{open ? "▾" : "▸"}</span>
        <span className="mono" style={{ fontSize: 11 }}>
          {events.length} tool action{events.length === 1 ? "" : "s"}
        </span>
      </button>
      {open && (
        <div className="xtr-acc-body">
          {events.map((ev, j) => (
            <div key={ev.event_id ?? j} className="xtr-tool-row">
              <span className="badge" style={{ fontSize: 9, borderColor: "rgba(194,151,255,0.4)", color: "#c297ff" }}>
                {ev.kind}
              </span>{" "}
              <span style={{ wordBreak: "break-word" }}>{ev.message}</span>
              {ev.payload_json && (
                <pre className="xtr-payload">{ev.payload_json}</pre>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// The grouped "nice" view.
function NiceView({ blocks }: { blocks: Block[] }) {
  return (
    <div className="xtr-nice">
      {blocks.map((b) => {
        if (b.family === "assistant") {
          const ev = b.events[0];
          const isResult = ev.kind === "result";
          return (
            <div key={b.key} className={"xtr-msg" + (isResult ? " xtr-msg-result" : "")}>
              <div className="xtr-msg-head">
                <span className="mono" style={{ fontSize: 10, fontWeight: 600 }}>
                  {ev.source}
                </span>
                <span className="badge" style={{ fontSize: 9 }}>{isResult ? "result" : "message"}</span>
                <span className="muted" style={{ fontSize: 10 }}>{ts(ev)}</span>
              </div>
              <div className="xtr-msg-body">{ev.message}</div>
            </div>
          );
        }
        if (b.family === "tool") {
          return <ToolBlock key={b.key} events={b.events} />;
        }
        if (b.family === "denied") {
          const ev = b.events[0];
          return (
            <div key={b.key} className="banner err xtr-callout">
              <span className="badge blocked" style={{ fontSize: 9 }}>permission denied</span>{" "}
              {ev.message}
            </div>
          );
        }
        if (b.family === "error") {
          return (
            <div key={b.key} className="banner err xtr-callout">
              {b.events.map((ev, j) => (
                <div key={ev.event_id ?? j} style={{ wordBreak: "break-word" }}>
                  <span className="mono" style={{ fontSize: 10 }}>{ev.kind}</span> — {ev.message}
                </div>
              ))}
            </div>
          );
        }
        if (b.family === "usage") {
          return (
            <div key={b.key} className="xtr-usage">
              {b.events.map((ev, j) => (
                <span key={ev.event_id ?? j} className="badge" style={{ fontSize: 10 }} title="real adapter usage/cost">
                  ◷ {ev.message}
                </span>
              ))}
            </div>
          );
        }
        // lifecycle rail
        return (
          <div key={b.key} className="xtr-life">
            {b.events.map((ev, j) => (
              <div key={ev.event_id ?? j} className="xtr-life-row">
                <span className="xtr-dot" style={{ background: kindDot(ev.kind) }} />
                <span className="muted" style={{ fontSize: 10 }}>{ts(ev)}</span>{" "}
                <span className="mono" style={{ fontSize: 10 }}>{kindLabel(ev.kind)}</span>
                {ev.message && <span className="muted" style={{ fontSize: 11 }}> — {ev.message}</span>}
              </div>
            ))}
          </div>
        );
      })}
    </div>
  );
}

// The compact "raw" view — every event verbatim, for debugging.
function RawView({ events }: { events: RunEvent[] }) {
  return (
    <div className="xtr-raw">
      {events.map((ev, j) => (
        <div key={ev.event_id ?? j} className="xtr-raw-row">
          <span className="xtr-dot" style={{ background: kindDot(ev.kind) }} />
          <span className="muted" style={{ fontSize: 10 }}>{ts(ev)}</span>{" "}
          <span className="mono" style={{ fontSize: 11 }}>{ev.source}/{ev.kind}</span>
          {ev.message ? <> — <span style={{ whiteSpace: "pre-wrap", wordBreak: "break-word" }}>{ev.message}</span></> : null}
          {ev.payload_json && <pre className="xtr-payload">{ev.payload_json}</pre>}
        </div>
      ))}
    </div>
  );
}

// Live-tail connection → a small honest chip (only shown while `running`).
const LIVE_LABEL: Record<RunEventConn, string> = {
  connecting: "connecting…",
  live: "live",
  reconnecting: "reconnecting…",
  unavailable: "polling",
};
const LIVE_TONE: Record<RunEventConn, string> = {
  connecting: "todo",
  live: "done",
  reconnecting: "in_progress",
  unavailable: "in_progress",
};

export interface RunTranscriptProps {
  runId: string;
  // Run status; `running` shows the live indicator + enables the polling
  // fallback so an in-flight Shift updates without a manual refresh.
  status?: string;
  // Tighter layout for the Brief workroom embed (smaller transcript height).
  compact?: boolean;
  // Bump to force a refetch after a parent mutation (apply/review/cancel).
  refreshKey?: number;
  // Observe the loaded events (e.g. so a parent can surface a scan-failed
  // banner) without re-fetching them itself.
  onEvents?: (events: RunEvent[]) => void;
}

export function RunTranscript({ runId, status, compact, refreshKey, onEvents }: RunTranscriptProps) {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [loading, setLoading] = useState(true);
  const [mode, setMode] = useState<"nice" | "raw">("nice");
  const [liveConn, setLiveConn] = useState<RunEventConn>("connecting");
  const running = status === "running";

  const onEventsRef = useRef(onEvents);
  onEventsRef.current = onEvents;

  async function load() {
    setLoading(true);
    try {
      const ev = await runControls.events(runId);
      const list = Array.isArray(ev) ? ev : [];
      setEvents(list);
      onEventsRef.current?.(list);
    } finally {
      setLoading(false);
    }
  }

  // Fetch on mount / run change / explicit refresh.
  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [runId, refreshKey]);

  // Live tail: subscribe ONCE to the execution event stream. The stream is
  // keyed by Brief (not run), so on any transition we refetch THIS run's
  // transcript while it is in flight — cheap, and it keeps an in-flight Shift
  // current without a manual click. Refs keep the subscription stable.
  const loadRef = useRef(load);
  loadRef.current = load;
  const runningRef = useRef(running);
  runningRef.current = running;
  useEffect(() => {
    let pending: ReturnType<typeof setTimeout> | null = null;
    const unsub = subscribeRunEvents(
      () => {
        if (!runningRef.current) return;
        if (pending) clearTimeout(pending);
        pending = setTimeout(() => void loadRef.current(), 500);
      },
      (state) => setLiveConn(state),
    );
    return () => {
      if (pending) clearTimeout(pending);
      unsub();
    };
  }, []);

  // Polling fallback: when the stream can't stay connected AND the run is
  // still in flight, poll the transcript so it never silently freezes.
  useEffect(() => {
    if (!running || liveConn !== "unavailable") return;
    const t = setInterval(() => void loadRef.current(), 4000);
    return () => clearInterval(t);
  }, [running, liveConn]);

  const blocks = useMemo(() => groupEvents(events), [events]);

  return (
    <div className="xtr">
      <div className="xtr-bar">
        <strong style={{ fontSize: 12 }}>Transcript</strong>
        {running && (
          <span
            className={"badge " + LIVE_TONE[liveConn]}
            style={{ fontSize: 9 }}
            title="live run-event stream (auto-updates this transcript)"
          >
            ● {LIVE_LABEL[liveConn]}
          </span>
        )}
        <div className="spacer" style={{ flex: 1 }} />
        <div className="btn-group" role="group" aria-label="Transcript view mode">
          <button className={"btn sm " + (mode === "nice" ? "" : "ghost")} onClick={() => setMode("nice")}>
            nice
          </button>
          <button className={"btn sm " + (mode === "raw" ? "" : "ghost")} onClick={() => setMode("raw")}>
            raw
          </button>
        </div>
        <button className="btn ghost sm" onClick={() => void load()}>Refresh</button>
      </div>
      {loading && events.length === 0 ? (
        <div className="loading">Loading transcript…</div>
      ) : events.length === 0 ? (
        <div className="muted" style={{ fontSize: 12 }}>
          {running ? "No transcript events yet — the Shift just started." : "No transcript events recorded."}
        </div>
      ) : (
        <div className="xtr-scroll" style={{ maxHeight: compact ? 280 : 420 }}>
          {mode === "nice" ? <NiceView blocks={blocks} /> : <RawView events={events} />}
        </div>
      )}
    </div>
  );
}
