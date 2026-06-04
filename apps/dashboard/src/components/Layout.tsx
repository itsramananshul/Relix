import { useEffect, useState, type ReactNode } from "react";
import { NavLink, useLocation } from "react-router-dom";
import { useAuth } from "../auth";
import { tryGet } from "../api";

interface NavEntry {
  to: string;
  label: string;
  icon: string;
}

const PRIMARY: NavEntry[] = [
  { to: "/", label: "Command Center", icon: "◈" },
  { to: "/briefs", label: "Briefs", icon: "▤" },
  { to: "/runs", label: "Active Runs", icon: "◐" },
  { to: "/chat", label: "Chat", icon: "✦" },
];
const ORG: NavEntry[] = [
  { to: "/agents", label: "Crew", icon: "◍" },
  { to: "/company", label: "Company", icon: "▦" },
  { to: "/assign", label: "Assign Work", icon: "➜" },
];
const SYSTEM: NavEntry[] = [
  { to: "/scheduled", label: "Scheduled", icon: "◷" },
  { to: "/settings", label: "Settings", icon: "⚙" },
];

const TITLES: Record<string, { title: string; sub: string }> = {
  "/": { title: "Command Center", sub: "Mesh overview & what needs attention" },
  "/briefs": { title: "Briefs", sub: "The issue board — your unit of work" },
  "/runs": { title: "Active Runs", sub: "Execution & activity status" },
  "/chat": { title: "Chat", sub: "Talk to the company companion" },
  "/agents": { title: "Crew", sub: "Operatives in your Guild" },
  "/company": { title: "Company", sub: "Org hierarchy & mandates" },
  "/assign": { title: "Assign Work", sub: "Hand a Brief to an Operative" },
  "/scheduled": { title: "Scheduled Jobs", sub: "Cron-driven work" },
  "/settings": { title: "Settings", sub: "Providers, account & bridge info" },
};

function Group({ label, items, counts }: { label: string; items: NavEntry[]; counts: Record<string, number> }) {
  return (
    <div className="nav-group">
      <div className="nav-label">{label}</div>
      {items.map((it) => (
        <NavLink
          key={it.to}
          to={it.to}
          end={it.to === "/"}
          className={({ isActive }) => "nav-item" + (isActive ? " active" : "")}
        >
          <span className="ico">{it.icon}</span>
          <span>{it.label}</span>
          {counts[it.to] != null && <span className="count">{counts[it.to]}</span>}
        </NavLink>
      ))}
    </div>
  );
}

export function Layout({ children }: { children: ReactNode }) {
  const { status, logout } = useAuth();
  const loc = useLocation();
  const [counts, setCounts] = useState<Record<string, number>>({});

  useEffect(() => {
    let on = true;
    (async () => {
      const inbox = await tryGet<Record<string, unknown[]>>("/v1/spine/inbox?limit=100", {});
      // The board summary is an object keyed by status, e.g. {todo:2,total:5}.
      const board = await tryGet<Record<string, number>>("/v1/spine/board", {});
      if (!on) return;
      const needsAttention =
        (inbox.blocked?.length ?? 0) +
        (inbox.overdue?.length ?? 0) +
        (inbox.unassigned?.length ?? 0);
      const active = (board.todo ?? 0) + (board.in_progress ?? 0) + (board.in_review ?? 0);
      setCounts({ "/briefs": needsAttention, "/runs": active });
    })();
    return () => {
      on = false;
    };
  }, [loc.pathname]);

  const meta = TITLES[loc.pathname] ?? { title: "Relix", sub: "" };
  const initial = (status?.username ?? "?").slice(0, 1).toUpperCase();

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <div className="logo">R</div>
          <div className="name">Relix</div>
          <div className="env">bridge</div>
        </div>
        <Group label="Workspace" items={PRIMARY} counts={counts} />
        <Group label="Organization" items={ORG} counts={counts} />
        <Group label="System" items={SYSTEM} counts={counts} />
        <div className="sidebar-foot">
          <div className="who">
            <div className="avatar">{initial}</div>
            <div>{status?.username ?? "operator"}</div>
            <div className="logout" onClick={() => void logout()}>
              Sign out
            </div>
          </div>
        </div>
      </aside>
      <div className="main">
        <header className="topbar">
          <h1>{meta.title}</h1>
          <span className="sub">{meta.sub}</span>
          <div className="spacer" />
        </header>
        <div className="workspace">{children}</div>
      </div>
    </div>
  );
}
