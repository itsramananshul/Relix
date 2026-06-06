import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useAuth } from "../auth";
import { ALL_NAV } from "./nav";

// A small, dependency-free operator command palette (design §2 — a shell
// singleton; §12 — "keyboard-first, ⌘K palette"). It only NAVIGATES to existing
// routes or signs out; it performs NO backend mutation. Restraint by design: no
// fuzzy-search lib, no new dependency — a plain case-insensitive substring match
// over a fixed command set.

interface Command {
  id: string;
  label: string;
  hint?: string;
  // The action. Navigation or sign-out only — nothing that mutates work objects.
  run: () => void;
}

export function CommandPalette({ open, onClose }: { open: boolean; onClose: () => void }) {
  const navigate = useNavigate();
  const { logout } = useAuth();
  const [q, setQ] = useState("");
  const [sel, setSel] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);

  // The full command set: a primary "Ask Prime" alias, every rail destination,
  // an "Action Center" alias onto the Command Center (where that feed lives),
  // and Sign out. Each `run` closes the palette first.
  const commands = useMemo<Command[]>(() => {
    const go = (to: string) => () => {
      onClose();
      navigate(to);
    };
    const list: Command[] = [
      { id: "ask-prime", label: "Ask Prime", hint: "Describe a goal → governed plan", run: go("/chat") },
      ...ALL_NAV.map((n) => ({ id: "nav:" + n.to, label: n.label, hint: n.to, run: go(n.to) })),
      { id: "action-center", label: "Action Center", hint: "What needs you now", run: go("/") },
      {
        id: "sign-out",
        label: "Sign out",
        hint: "End this operator session",
        run: () => {
          onClose();
          void logout();
        },
      },
    ];
    return list;
  }, [navigate, logout, onClose]);

  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return commands;
    return commands.filter(
      (c) => c.label.toLowerCase().includes(needle) || (c.hint ?? "").toLowerCase().includes(needle),
    );
  }, [q, commands]);

  // Reset query/selection and focus the input each time the palette opens.
  useEffect(() => {
    if (open) {
      setQ("");
      setSel(0);
      // Focus after paint so the field is reliably ready.
      requestAnimationFrame(() => inputRef.current?.focus());
    }
  }, [open]);

  // Keep the selection in range as the filter narrows.
  useEffect(() => {
    setSel((s) => (filtered.length === 0 ? 0 : Math.min(s, filtered.length - 1)));
  }, [filtered.length]);

  // Keep the active option scrolled into view as the operator arrows through.
  useEffect(() => {
    const el = listRef.current?.querySelector<HTMLElement>('[aria-selected="true"]');
    el?.scrollIntoView({ block: "nearest" });
  }, [sel, filtered.length]);

  if (!open) return null;

  function onKeyDown(e: React.KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      onClose();
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      setSel((s) => (filtered.length ? (s + 1) % filtered.length : 0));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setSel((s) => (filtered.length ? (s - 1 + filtered.length) % filtered.length : 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      filtered[sel]?.run();
    }
  }

  const listId = "cmdk-list";
  const activeId = filtered[sel] ? "cmdk-opt-" + filtered[sel].id : undefined;
  return (
    <div className="cmdk-overlay" role="presentation" onMouseDown={onClose}>
      <div
        className="cmdk"
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <input
          ref={inputRef}
          className="cmdk-input"
          type="text"
          placeholder="Jump to… (type to filter)"
          aria-label="Search commands"
          role="combobox"
          aria-expanded="true"
          aria-controls={listId}
          aria-activedescendant={activeId}
          autoComplete="off"
          spellCheck={false}
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
        <ul className="cmdk-list" id={listId} role="listbox" aria-label="Commands" ref={listRef}>
          {filtered.length === 0 && <li className="cmdk-empty">No matching command</li>}
          {filtered.map((c, i) => (
            <li
              key={c.id}
              id={"cmdk-opt-" + c.id}
              role="option"
              aria-selected={i === sel}
              className={"cmdk-item" + (i === sel ? " active" : "")}
              onMouseEnter={() => setSel(i)}
              onMouseDown={(e) => {
                e.preventDefault();
                c.run();
              }}
            >
              <span className="cmdk-label">{c.label}</span>
              {c.hint && <span className="cmdk-hint">{c.hint}</span>}
            </li>
          ))}
        </ul>
        <div className="cmdk-foot">
          <span><kbd>↑</kbd><kbd>↓</kbd> navigate</span>
          <span><kbd>↵</kbd> open</span>
          <span><kbd>esc</kbd> close</span>
        </div>
      </div>
    </div>
  );
}
