// Shared navigation entries for the shell. Both the sidebar (Layout) and the
// command palette read from one source so they never drift apart (design §3 —
// the rail is the work-object nav; the palette is the same destinations,
// keyboard-first per §12).

export interface NavEntry {
  to: string;
  label: string;
  icon: string;
}

export const PRIMARY: NavEntry[] = [
  { to: "/", label: "Command Center", icon: "◈" },
  { to: "/mandates", label: "Mandates", icon: "◎" },
  { to: "/briefs", label: "Briefs", icon: "▤" },
  { to: "/runs", label: "Active Runs", icon: "◐" },
  { to: "/approvals", label: "Approvals", icon: "✔" },
  { to: "/chat", label: "Chat", icon: "✦" },
];

export const ORG: NavEntry[] = [
  { to: "/agents", label: "Crew", icon: "◍" },
  { to: "/lattice", label: "Lattice", icon: "⬡" },
  { to: "/company", label: "Company", icon: "▦" },
  { to: "/costs", label: "Costs", icon: "$" },
  { to: "/assign", label: "Assign Work", icon: "➜" },
];

export const SYSTEM: NavEntry[] = [
  { to: "/scheduled", label: "Scheduled", icon: "◷" },
  { to: "/settings", label: "Settings", icon: "⚙" },
];

// Flat list in rail order — the palette's navigable command set.
export const ALL_NAV: NavEntry[] = [...PRIMARY, ...ORG, ...SYSTEM];
