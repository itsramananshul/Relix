// Centralized dashboard invalidation bus — the first real, dependency-free
// slice of the realtime "surgical updates" pattern (relix-dashboard-design
// §11). After a mutation is CONFIRMED server-side, the mutating handler emits a
// small set of named keys; every mounted surface subscribes to the keys whose
// data it renders and refetches. This is the centralized invalidation-key
// discipline §11 calls to "adopt early", in its smallest honest form (a custom
// EventTarget — no query library, no socket of its own).
//
// HONESTY CONTRACT (§11 / CLAUDE.md): the bus never carries data and never
// applies optimistic state. It only says "this key changed — refetch from the
// server", so a surface can never diverge from the backend. Handlers emit only
// AFTER the server confirms the mutation.
//
// RELATIONSHIP TO SSE: this COMPLEMENTS the backend run-event stream
// (`subscribeRunEvents`), which already pushes run-lifecycle transitions
// (start/finish/refuse/recover/move/review/apply) to the surfaces that
// subscribe to it. The bus carries the mutation outcomes the SSE stream does
// NOT — comments, interaction/suggestion answers, assignment, hiring,
// orchestration, governance — and keeps co-mounted surfaces (the Issue board
// and the open Brief detail panel beside it) in lockstep.

import { useEffect, useRef } from "react";

// The named surfaces a mutation can invalidate. Kept deliberately small and
// coarse — one key per live data-surface, not per endpoint — so emitters and
// subscribers stay obvious. (The full hierarchical query-key factory §11
// envisions can grow from here once a query library lands.)
export type InvalidationKey =
  | "briefs" // the Issue board / any Brief list / inbox membership
  | "brief" // ONE Brief's detail: conversation, requests, chronicle, latest Shift
  | "runs" // the run (Shift) ledger
  | "actions" // the Overview Action Center + company readiness
  | "mandates"; // Mandate governance / readiness / orchestration

export interface InvalidationDetail {
  // When a single Brief changed, its id — so `brief` subscribers refetch only
  // when their Brief is the one affected (and ignore every other Brief's
  // churn). Absent means "unscoped" — a `brief` subscriber then refetches to
  // be safe (only one detail panel is ever open at a time).
  briefId?: string;
}

interface Frame {
  keys: InvalidationKey[];
  detail: InvalidationDetail;
}

const bus = new EventTarget();
const EVENT = "relix:invalidate";

// Emit an invalidation for one or more keys AFTER a mutation is confirmed
// server-side. Pass `briefId` when a specific Brief changed so `brief`
// subscribers can scope their refetch.
export function invalidate(
  keys: InvalidationKey | InvalidationKey[],
  detail: InvalidationDetail = {},
): void {
  const list = Array.isArray(keys) ? keys : [keys];
  bus.dispatchEvent(new CustomEvent<Frame>(EVENT, { detail: { keys: list, detail } }));
}

// Subscribe to one or more keys; `cb` fires once per matching emit with that
// emit's detail. Returns an unsubscribe fn. Lower-level than `useInvalidate` —
// prefer the hook inside components.
export function onInvalidate(
  keys: InvalidationKey | InvalidationKey[],
  cb: (detail: InvalidationDetail) => void,
): () => void {
  const want = new Set(Array.isArray(keys) ? keys : [keys]);
  const handler = (e: Event) => {
    const f = (e as CustomEvent<Frame>).detail;
    if (f && f.keys.some((k) => want.has(k))) cb(f.detail);
  };
  bus.addEventListener(EVENT, handler);
  return () => bus.removeEventListener(EVENT, handler);
}

// React helper: refetch (`cb`) when any of `keys` is invalidated. Coalesces a
// burst of emits into a single call (default 150ms) so a multi-key mutation
// triggers exactly one refetch, and keeps the subscription stable across
// renders (the callback rides a ref, so no listener churn / reconnect storms).
//
// Pass `match` to filter — e.g. a Brief detail passes
// `d => !d.briefId || d.briefId === id` so it ignores other Briefs' events.
export function useInvalidate(
  keys: InvalidationKey | InvalidationKey[],
  cb: () => void,
  opts: { debounceMs?: number; match?: (d: InvalidationDetail) => boolean } = {},
): void {
  const cbRef = useRef(cb);
  cbRef.current = cb;
  const matchRef = useRef(opts.match);
  matchRef.current = opts.match;
  const debounceMs = opts.debounceMs ?? 150;
  // A stable string key so the effect re-subscribes only if the key SET
  // actually changes, not on every render (array identity would churn).
  const keyId = (Array.isArray(keys) ? keys : [keys]).join(",");
  useEffect(() => {
    let pending: ReturnType<typeof setTimeout> | null = null;
    const unsub = onInvalidate(keys, (d) => {
      if (matchRef.current && !matchRef.current(d)) return;
      if (pending) clearTimeout(pending);
      pending = setTimeout(() => cbRef.current(), debounceMs);
    });
    return () => {
      if (pending) clearTimeout(pending);
      unsub();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keyId, debounceMs]);
}
