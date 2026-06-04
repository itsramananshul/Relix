import { useState } from "react";
import { api, tryGet } from "../api";
import { asArray, Empty, Section, useAsync } from "../components/common";

interface Job {
  job_id?: string;
  id?: string;
  name?: string;
  schedule?: string;
  prompt?: string;
  flow_template?: string;
  next_run?: number;
  enabled?: boolean;
}

function extractJobs(v: unknown): Job[] {
  if (Array.isArray(v)) return v as Job[];
  if (v && typeof v === "object") {
    const o = v as Record<string, unknown>;
    for (const k of ["jobs", "items", "results"]) if (Array.isArray(o[k])) return o[k] as Job[];
  }
  return [];
}

export function Scheduled() {
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [schedule, setSchedule] = useState("0 9 * * *");
  const [prompt, setPrompt] = useState("");
  const [banner, setBanner] = useState<{ kind: string; msg: string } | null>(null);

  const { data, loading, reload } = useAsync(async () => extractJobs(await tryGet<unknown>("/v1/cron/jobs", [])), []);
  const jobs = data ?? [];

  async function create() {
    if (!schedule.trim() || !prompt.trim()) {
      setBanner({ kind: "err", msg: "Schedule and prompt are required." });
      return;
    }
    setBanner(null);
    try {
      await api.post("/v1/cron/jobs", { name: name.trim() || undefined, schedule: schedule.trim(), prompt: prompt.trim() });
      setBanner({ kind: "ok", msg: "Scheduled job created." });
      setCreating(false);
      setName("");
      setPrompt("");
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Create failed" });
    }
  }

  async function trigger(id: string) {
    setBanner(null);
    try {
      await api.post(`/v1/cron/jobs/${encodeURIComponent(id)}/trigger`);
      setBanner({ kind: "ok", msg: "Job triggered." });
      reload();
    } catch (e) {
      setBanner({ kind: "err", msg: e instanceof Error ? e.message : "Trigger failed" });
    }
  }

  return (
    <div className="grid">
      <Section
        title="Scheduled jobs"
        action={<button className="btn" onClick={() => setCreating((v) => !v)}>{creating ? "Cancel" : "+ New job"}</button>}
      >
        {banner && <div className={"banner " + banner.kind}>{banner.msg}</div>}
        {creating && (
          <div className="card" style={{ marginBottom: 14, maxWidth: 620 }}>
            <label className="field">
              <span>Name (optional)</span>
              <input className="input" value={name} onChange={(e) => setName(e.target.value)} placeholder="Nightly digest" />
            </label>
            <label className="field">
              <span>Schedule (cron)</span>
              <input className="input mono" value={schedule} onChange={(e) => setSchedule(e.target.value)} placeholder="0 9 * * *" />
            </label>
            <label className="field">
              <span>Prompt / instruction</span>
              <textarea className="input" rows={3} value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder="Summarize yesterday's completed Briefs…" />
            </label>
            <button className="btn" onClick={create}>Create job</button>
          </div>
        )}
        <div className="card">
          {loading ? (
            <div className="loading">Loading jobs…</div>
          ) : jobs.length === 0 ? (
            <Empty>No scheduled jobs. Cron-driven work runs unattended on a schedule.</Empty>
          ) : (
            <table className="table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Schedule</th>
                  <th>Instruction</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {asArray<Job>(jobs).map((j, i) => {
                  const id = j.job_id ?? j.id ?? "";
                  return (
                    <tr key={id || i}>
                      <td><strong>{j.name ?? id.slice(0, 10) ?? "job"}</strong></td>
                      <td className="mono">{j.schedule ?? "—"}</td>
                      <td className="muted">{(j.prompt ?? j.flow_template ?? "").slice(0, 60)}</td>
                      <td>
                        <button className="btn ghost sm" onClick={() => trigger(id)}>Run now</button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </div>
      </Section>
    </div>
  );
}
