import { useEffect, useState, type ReactNode } from "react";
import { Link, NavLink, useLocation } from "react-router-dom";
import { useAuth } from "../auth";
import { tryGet } from "../api";

interface NavEntry {
  to: string;
  label: string;
  icon: string;
}

const PRIMARY: NavEntry[] = [
  { to: "/", label: "Command Center", icon: "◈" },
  { to: "/mandates", label: "Mandates", icon: "◎" },
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
  "/mandates": { title: "Mandates", sub: "Turn a big goal into a Brief tree" },
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
          title={it.label}
          className={({ isActive }) => "nav-item" + (isActive ? " active" : "")}
        >
          <span className="ico">{it.icon}</span>
          <span>{it.label}</span>
          {counts[it.to] != null && counts[it.to] > 0 && <span className="count">{counts[it.to]}</span>}
        </NavLink>
      ))}
    </div>
  );
}

// Compact identity for one Operative slot in the topbar band.
interface Ident { name?: string; rig?: string | null }
interface CompanyIdent {
  initialized?: boolean;
  founder?: Ident | null;
  prime?: Ident | null;
  operative_count?: number;
  crew?: { active?: number; total?: number };
}

export function Layout({ children }: { children: ReactNode }) {
  const { status, logout } = useAuth();
  const loc = useLocation();
  const [counts, setCounts] = useState<Record<string, number>>({});
  const [company, setCompany] = useState<CompanyIdent | null>(null);

  useEffect(() => {
    let on = true;
    (async () => {
      const inbox = await tryGet<Record<string, unknown[]>>("/v1/spine/inbox?limit=100", {});
      // The board summary is an object keyed by status, e.g. {todo:2,total:5}.
      const board = await tryGet<Record<string, number>>("/v1/spine/board", {});
      const co = await tryGet<CompanyIdent>("/v1/spine/company", {});
      if (!on) return;
      const needsAttention =
        (inbox.blocked?.length ?? 0) +
        (inbox.overdue?.length ?? 0) +
        (inbox.unassigned?.length ?? 0);
      const active = (board.todo ?? 0) + (board.in_progress ?? 0) + (board.in_review ?? 0);
      setCounts({ "/briefs": needsAttention, "/runs": active });
      setCompany(co ?? null);
    })();
    return () => {
      on = false;
    };
  }, [loc.pathname]);

  const meta = TITLES[loc.pathname] ?? { title: "Relix", sub: "" };
  const initial = (status?.username ?? "?").slice(0, 1).toUpperCase();
  const founder = company?.founder ?? null;
  const prime = company?.prime ?? null;
  const crew = company?.crew?.active ?? company?.crew?.total ?? company?.operative_count ?? 0;
  const showIdent = !!company?.initialized || !!founder;

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
            <div className="logout" title="Sign out" onClick={() => void logout()}>
              <span className="lbl">Sign out</span>
              <span className="ico-only" aria-hidden>⎋</span>
            </div>
          </div>
        </div>
      </aside>
      <div className="main">
        <header className="topbar">
          <div className="titlewrap">
            <h1>{meta.title}</h1>
            <span className="sub">{meta.sub}</span>
          </div>
          <div className="spacer" />
          {showIdent && (
            <div className="ident">
              <span className="ident-chip" title="Founder — the org root Operative">
                <span className={"dot " + (founder ? "on" : "")} />
                <span className="k">Founder</span> {founder?.name ?? "—"}
              </span>
              <span className="ident-chip" title="Prime — the planning lead">
                <span className={"dot " + (prime ? "on" : "warn")} />
                <span className="k">Prime</span> {prime?.name ?? "not hired"}
              </span>
              <span className="ident-chip" title="Active Operatives in your Guild">
                <span className="k">Crew</span> {crew}
              </span>
            </div>
          )}
          <Link to="/chat" title="Describe a goal — Prime proposes a governed plan">
            <button className="btn sm">Ask Prime →</button>
          </Link>
        </header>
        <div className="workspace">{children}</div>
      </div>
    </div>
  );
}
