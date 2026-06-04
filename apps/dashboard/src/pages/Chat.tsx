import { useRef, useState } from "react";
import { api } from "../api";

interface Msg { role: "user" | "assistant"; text: string }
interface CompanionResponse { action?: string; reply?: string; result?: unknown }

export function Chat() {
  const [log, setLog] = useState<Msg[]>([
    {
      role: "assistant",
      text: "I'm the company companion. Ask me to create a Brief, move work, or check status — e.g. \"create a brief to ship the login page\".",
    },
  ]);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const logRef = useRef<HTMLDivElement>(null);

  async function send() {
    const message = text.trim();
    if (!message || busy) return;
    setText("");
    setLog((l) => [...l, { role: "user", text: message }]);
    setBusy(true);
    try {
      const res = await api.post<CompanionResponse>("/v1/spine/companion", { message });
      const reply =
        res.reply ||
        (res.action ? `Done: ${res.action}` : "OK.") +
          (res.result ? "\n\n" + JSON.stringify(res.result, null, 2) : "");
      setLog((l) => [...l, { role: "assistant", text: reply }]);
    } catch (e) {
      setLog((l) => [...l, { role: "assistant", text: "⚠ " + (e instanceof Error ? e.message : "failed") }]);
    } finally {
      setBusy(false);
      requestAnimationFrame(() => logRef.current?.scrollTo(0, logRef.current.scrollHeight));
    }
  }

  return (
    <div className="chat" style={{ height: "calc(100vh - 96px)" }}>
      <div className="chat-log" ref={logRef}>
        {log.map((m, i) => (
          <div key={i} className={"msg " + m.role}>
            {m.text}
          </div>
        ))}
        {busy && <div className="msg assistant muted">…thinking</div>}
      </div>
      <div className="chat-input">
        <input
          className="input"
          placeholder="Message the companion…"
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && send()}
        />
        <button className="btn" onClick={send} disabled={busy || !text.trim()}>
          Send
        </button>
      </div>
    </div>
  );
}
