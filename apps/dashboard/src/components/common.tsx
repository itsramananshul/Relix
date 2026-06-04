import { useCallback, useEffect, useState, type ReactNode } from "react";

// Tiny async-data hook: runs `fn` on mount + when `deps` change, exposing
// loading/error/data and a manual `reload`.
export function useAsync<T>(fn: () => Promise<T>, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const run = useCallback(() => {
    let on = true;
    setLoading(true);
    setError(null);
    fn()
      .then((d) => {
        if (on) setData(d);
      })
      .catch((e) => {
        if (on) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (on) setLoading(false);
      });
    return () => {
      on = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  useEffect(run, [run]);
  return { data, loading, error, reload: run };
}

export function Badge({ status }: { status?: string | null }) {
  const s = (status ?? "").toLowerCase();
  return <span className={"badge " + s}>{status ?? "—"}</span>;
}

export function Section({ title, action, children }: { title: string; action?: ReactNode; children: ReactNode }) {
  return (
    <>
      <div className="section-head">
        <h2>{title}</h2>
        <div className="spacer" />
        {action}
      </div>
      {children}
    </>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="empty">{children}</div>;
}

export function asArray<T = unknown>(v: unknown): T[] {
  return Array.isArray(v) ? (v as T[]) : [];
}

// Many bridge endpoints wrap their list under a key (`{items:[…]}`,
// `{agents:[…]}`, `{jobs:[…]}`, …) or return a bare array. Pull the list
// out regardless of which shape arrived.
export function extractList<T = unknown>(v: unknown, keys: string[] = []): T[] {
  if (Array.isArray(v)) return v as T[];
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of [...keys, "items", "results", "rows", "list"]) {
      if (Array.isArray(o[k])) return o[k] as T[];
    }
  }
  return [];
}
