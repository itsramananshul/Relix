import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";
import { api } from "./api";

export interface AuthStatus {
  needs_setup: boolean;
  authenticated: boolean;
  username: string | null;
}

interface AuthContextValue {
  loading: boolean;
  status: AuthStatus | null;
  refresh: () => Promise<void>;
  login: (username: string, password: string) => Promise<void>;
  setup: (username: string, password: string) => Promise<void>;
  logout: () => Promise<void>;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [loading, setLoading] = useState(true);
  const [status, setStatus] = useState<AuthStatus | null>(null);

  const refresh = useCallback(async () => {
    try {
      const s = await api.get<AuthStatus>("/v1/auth/status");
      setStatus(s);
    } catch {
      setStatus({ needs_setup: false, authenticated: false, username: null });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const login = useCallback(
    async (username: string, password: string) => {
      await api.post("/v1/auth/login", { username, password });
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
    <AuthContext.Provider value={{ loading, status, refresh, login, setup, logout }}>
      {children}
    </AuthContext.Provider>
  );
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used within AuthProvider");
  return ctx;
}
