import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";
import { ApiError, api, onSessionExpired } from "./api";

export interface AuthStatus {
  needs_setup: boolean;
  authenticated: boolean;
  username: string | null;
}

interface AuthContextValue {
  loading: boolean;
  status: AuthStatus | null;
  // The auth-status probe couldn't reach the bridge at all (network/DNS/TLS
  // failure, bridge process down). Distinct from "reached the bridge and it
  // says you're not logged in" so the UI can explain the right fix.
  bridgeDown: boolean;
  bridgeError: string | null;
  // A protected API returned 401/403 mid-session: the cookie lapsed. The app
  // shows the login screen with a "session expired" note instead of broken
  // cards. Cleared on a successful re-login.
  sessionExpired: boolean;
  refresh: () => Promise<void>;
  login: (username: string, password: string) => Promise<void>;
  setup: (username: string, password: string) => Promise<void>;
  logout: () => Promise<void>;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [loading, setLoading] = useState(true);
  const [status, setStatus] = useState<AuthStatus | null>(null);
  const [bridgeDown, setBridgeDown] = useState(false);
  const [bridgeError, setBridgeError] = useState<string | null>(null);
  const [sessionExpired, setSessionExpired] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const s = await api.get<AuthStatus>("/v1/auth/status");
      setStatus(s);
      setBridgeDown(false);
      setBridgeError(null);
      // A fresh status read is the authority: if it says we're in, the prior
      // expiry is resolved; if it says we're out, the flag already matches.
      if (s.authenticated) setSessionExpired(false);
    } catch (e) {
      // An ApiError means the bridge answered (e.g. 5xx) — still "reachable
      // but unhealthy". A non-ApiError (TypeError from fetch) means the
      // request never reached the bridge: treat that as bridge-down.
      const reached = e instanceof ApiError;
      setBridgeDown(!reached);
      setBridgeError(e instanceof Error ? e.message : String(e));
      setStatus({ needs_setup: false, authenticated: false, username: null });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Listen for a protected-API 401/403 (see api.ts): mark the session expired
  // and re-probe /v1/auth/status. The re-probe flips `authenticated` to false,
  // so <App> renders <Login> — now with the "session expired" banner.
  useEffect(() => {
    return onSessionExpired(() => {
      setSessionExpired(true);
      void refresh();
    });
  }, [refresh]);

  const login = useCallback(
    async (username: string, password: string) => {
      await api.post("/v1/auth/login", { username, password });
      setSessionExpired(false);
      await refresh();
    },
    [refresh],
  );

  const setup = useCallback(
    async (username: string, password: string) => {
      await api.post("/v1/auth/setup", { username, password });
      await refresh();
    },
    [refresh],
  );

  const logout = useCallback(async () => {
    try {
      await api.post("/v1/auth/logout");
    } finally {
      await refresh();
    }
  }, [refresh]);

  return (
    <AuthContext.Provider
      value={{ loading, status, bridgeDown, bridgeError, sessionExpired, refresh, login, setup, logout }}
    >
      {children}
    </AuthContext.Provider>
  );
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used within AuthProvider");
  return ctx;
}
