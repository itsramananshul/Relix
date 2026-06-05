import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { probe, type Probe } from "../api";

// One diagnostic row: a layer of the stack, the probe result, and the
// exact next action when it is red. Nothing here is faked — each row is a
// live probe of a real endpoint, and a failure shows the bridge's own
// error string plus the concrete fix.
interface Check {
  key: string;
  label: string;
  hint: string; // what this layer is
  result: Probe | null;
  fix?: { text: string; to?: string };
}

// Probe a layer; map the failure to the right operator next-action.
async function runChecks(): Promise<{ checks: Check[]; tenant: string | null }> {
  // /health is public (no auth) — proves the bridge process is up.
  const bridge = await probe("/health");
  // /v1/auth/status is public — proves the session state.
  const session = await probe("/v1/auth/status");
  // Spine + coordinator are coordinator-backed: 200 = reachable; 502/503 =
  // the bridge can't reach the coordinator peer; 401 = session expired.
  const spine = await probe("/v1/spine/company");
  const coord = await probe("/v1/runs");
  const adapters = await probe("/v1/adapters");

  const tenant =
    spine.tenant ?? coord.tenant ?? adapters.tenant ?? session.tenant ?? null;

  const checks: Check[] = [
    {
      key: "bridge",
      label: "Bridge",
      hint: "the web bridge process (HTTP API)",
      result: bridge,
      fix: bridge.ok
        ? undefined
        : { text: "The bridge isn't answering. Start the mesh (scripts/relix-mesh-up) and reload." },
    },
    {
      key: "session",
      label: "Dashboard session",
      hint: "your authenticated operator login",
      result: session,
      fix: session.ok
        ? undefined
        : { text: "Sign in again. If you forgot the password, run scripts/relix-dashboard-admin-reset then restart the bridge." },
    },
    {
      key: "spine",
      label: "Spine",
      hint: "Guild / Briefs / Mandates (coordinator-backed)",
      result: spine,
      fix: spine.ok ? undefined : spineFix(spine),
    },
    {
      key: "coordinator",
      label: "Coordinator",
      hint: "the durable run/Brief ledger",
      result: coord,
      fix: coord.ok ? undefined : spineFix(coord),
    },
    {
      key: "adapters",
      label: "Adapters / Rigs",
      hint: "installed coding-agent backends",
      result: adapters,
      fix: adapters.ok
        ? undefined
        : { text: "Adapter readiness couldn't be read — usually the coordinator is unreachable.", to: "/settings" },
    },
  ];
  return { checks, tenant };
}

function spineFix(p: Probe): Check["fix"] {
  if (p.status === 401 || p.status === 403)
    return { text: "Session expired — sign in again." };
  if (p.status === 502 || p.status === 503 || p.status === null)
    return { text: "The bridge can't reach the coordinator. Confirm the coordinator controller is running (scripts/relix-mesh-up starts it)." };
  return { text: "Unexpected error — see the detail above." };
}

function Dot({ ok }: { ok: boolean | null }) {
  const color = ok == null ? "#999" : ok ? "var(--ok, #1e7e34)" : "var(--err, #c0392b)";
  return (
    <span
      style={{ display: "inline-block", width: 10, height: 10, borderRadius: 5, background: color, flex: "0 0 auto" }}
    />
  );
}

export function HealthPanel({ compact = false }: { compact?: boolean }) {
  const [checks, setChecks] = useState<Check[] | null>(null);
  const [tenant, setTenant] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const reload = useCallback(() => {
    setLoading(true);
    void runChecks()
      .then(({ checks, tenant }) => {
        setChecks(checks);
        setTenant(tenant);
      })
      .finally(() => setLoading(false));
  }, []);

  useEffect(reload, [reload]);

  const rows = checks ?? [];
  const allOk = rows.length > 0 && rows.every((c) => c.result?.ok);
  const anyDown = rows.some((c) => c.result && !c.result.ok);

  // Compact: a single status strip for the Overview (only loud when red).
  if (compact) {
    if (loading && !checks) return null;
    if (allOk) return null; // healthy → stay quiet on the Overview
    return (
      <div className="banner err banner-action">
        <span>
          System health:{" "}
          {rows.filter((c) => c.result && !c.result.ok).map((c) => c.label).join(", ")}{" "}
          {anyDown ? "unavailable" : ""} — some data may be missing.
        </span>
        <Link to="/settings" className="banner-cta">Run diagnostics →</Link>
      </div>
    );
  }

  return (
    <div className="card">
      <div className="row" style={{ marginBottom: 10 }}>
        <h3 style={{ margin: 0 }}>System health</h3>
        <span className={"badge " + (allOk ? "done" : anyDown ? "blocked" : "todo")} style={{ marginLeft: 8 }}>
          {loading && !checks ? "checking…" : allOk ? "all green" : anyDown ? "attention" : "—"}
        </span>
        <div className="spacer" style={{ flex: 1 }} />
        <span className="muted" style={{ fontSize: 12, marginRight: 8 }}>
          Guild/tenant: <span className="mono">{tenant ?? "default"}</span>
        </span>
        <button className="btn ghost sm" onClick={reload} disabled={loading}>
          {loading ? "…" : "Recheck"}
        </button>
      </div>
      <table className="table compact">
        <tbody>
          {rows.map((c) => {
            const ok = c.result?.ok ?? null;
            return (
              <tr key={c.key}>
                <td style={{ width: 18 }}><Dot ok={ok} /></td>
                <td>
                  <strong style={{ fontSize: 13 }}>{c.label}</strong>
                  <div className="muted" style={{ fontSize: 11 }}>{c.hint}</div>
                </td>
                <td>
                  <span className={"badge " + (ok ? "done" : ok === false ? "blocked" : "todo")} style={{ fontSize: 10 }}>
                    {ok == null ? "—" : ok ? "ok" : c.result?.status != null ? `HTTP ${c.result.status}` : "unreachable"}
                  </span>
                </td>
                <td className="muted" style={{ fontSize: 11, maxWidth: 360 }}>
                  {ok ? "" : (
                    <>
                      <div style={{ color: "var(--err, #c0392b)", wordBreak: "break-word" }}>{c.result?.detail}</div>
                      {c.fix && (
                        <div style={{ marginTop: 2 }}>
                          → {c.fix.text}{" "}
                          {c.fix.to && <Link to={c.fix.to} className="link">open</Link>}
                        </div>
                      )}
                    </>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
      {allOk && (
        <div className="muted" style={{ fontSize: 12, marginTop: 8 }}>
          Everything reachable. If a page still looks empty, hit Recheck — a transient coordinator restart can clear on retry.
        </div>
      )}
    </div>
  );
}
