import { useState, type FormEvent } from "react";
import { useAuth } from "../auth";

export function Login() {
  const { status, login, setup, bridgeDown, bridgeError, sessionExpired, refresh } = useAuth();
  const isSetup = status?.needs_setup ?? false;
  const [username, setUsername] = useState("admin");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setErr(null);
    if (isSetup && password !== confirm) {
      setErr("Passwords do not match");
      return;
    }
    setBusy(true);
    try {
      if (isSetup) await setup(username.trim(), password);
      else await login(username.trim(), password);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "Authentication failed");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="auth-wrap">
      <form className="auth-card" onSubmit={submit}>
        <div className="logo">R</div>
        <h2>{isSetup ? "Set up Relix" : "Sign in"}</h2>
        <p className="sub">
          {isSetup
            ? "Create the operator admin account for this bridge."
            : "Operator console for your Relix mesh."}
        </p>
        {sessionExpired && !isSetup && !bridgeDown && (
          <div className="banner err" style={{ marginBottom: 8 }}>
            Your session expired — sign in again to continue.
          </div>
        )}
        {bridgeDown && (
          <div className="banner err banner-action" style={{ marginBottom: 8 }}>
            <span>
              Can't reach the Relix bridge{bridgeError ? ` (${bridgeError})` : ""}. Start the mesh on
              the host (<span className="mono">scripts/relix-mesh-up</span>), then retry.
            </span>
            <span className="banner-cta" style={{ cursor: "pointer" }} onClick={() => void refresh()}>Retry →</span>
          </div>
        )}
        {err && <div className="banner err">{err}</div>}
        <label className="field">
          <span>Username</span>
          <input
            className="input"
            value={username}
            autoComplete="username"
            onChange={(e) => setUsername(e.target.value)}
          />
        </label>
        <label className="field">
          <span>Password</span>
          <input
            className="input"
            type="password"
            value={password}
            autoComplete={isSetup ? "new-password" : "current-password"}
            onChange={(e) => setPassword(e.target.value)}
          />
        </label>
        {isSetup && (
          <label className="field">
            <span>Confirm password (min 8 chars)</span>
            <input
              className="input"
              type="password"
              value={confirm}
              autoComplete="new-password"
              onChange={(e) => setConfirm(e.target.value)}
            />
          </label>
        )}
        <button className="btn" style={{ width: "100%", marginTop: 6 }} disabled={busy}>
          {busy ? "…" : isSetup ? "Create admin & continue" : "Sign in"}
        </button>
        {!isSetup && (
          <details className="forgot">
            <summary>Forgot the local admin password?</summary>
            <p className="muted" style={{ fontSize: 12, margin: "8px 0 4px" }}>
              Run this on the machine hosting the bridge, then restart it and sign in with the new password:
            </p>
            <pre className="forgot-cmd">scripts\relix-dashboard-admin-reset.ps1   (Windows){"\n"}./scripts/relix-dashboard-admin-reset.sh  (macOS / Linux)</pre>
            <p className="muted" style={{ fontSize: 11, margin: "4px 0 0" }}>
              Local operator recovery only — it rewrites just the admin credential, not your data. There is no online reset.
            </p>
          </details>
        )}
      </form>
    </div>
  );
}
