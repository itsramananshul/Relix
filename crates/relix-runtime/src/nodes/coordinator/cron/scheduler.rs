//! Periodic scheduler that fires due cron jobs.
//!
//! Runs as a tokio task. On every tick (default 30 s):
//!
//! 1. Query `cron_jobs` for enabled rows with `next_run_at <= now`.
//! 2. For each due job, check the previous task's status — if
//!    it's still `running`, skip with a WARN (prevents pile-ups
//!    when a long-running flow stretches past the next fire).
//! 3. Acquire the per-tick semaphore (max_concurrent default 3).
//! 4. Call [`fire_job`] — creates a coordinator task, writes
//!    `cron.job_fired`, advances the job's bookkeeping, and
//!    spawns the AI dispatch with a hard timeout.
//!
//! `cron.trigger` reuses [`fire_job`] so the manual and
//! periodic paths produce identical chronicle / task records.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::{OnceCell, Semaphore};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use crate::nodes::coordinator::TaskStore;
use crate::nodes::coordinator::cron::schedule::Schedule;
use crate::nodes::coordinator::cron::store::{CronJob, CronStore};

// ── Config ────────────────────────────────────────────────

/// `[coordinator.cron]` config section. Optional — absence
/// means the scheduler loop is not spawned (cron.* capabilities
/// are still registered so operators can create jobs ahead of
/// enabling the scheduler).
#[derive(Clone, Debug, Deserialize)]
pub struct CronSchedulerConfig {
    /// Master switch. When `false`, the periodic loop is not
    /// spawned. Defaults to `true` so the presence of the
    /// `[coordinator.cron]` section is enough to opt in.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Seconds between ticks. Default 30 s.
    #[serde(default = "default_tick_secs")]
    pub tick_secs: u64,
    /// Maximum concurrent jobs the scheduler will fire in
    /// flight. Default 3.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Hard per-job timeout. The AI dispatch is wrapped in
    /// `tokio::time::timeout(max_job_secs)`. Default 300 s.
    #[serde(default = "default_max_job_secs")]
    pub max_job_secs: u64,
    /// Optional outbound AI peer config. When set, post-startup
    /// wiring builds a [`CronAiMeshDispatcher`] and the
    /// scheduler dispatches `ai.chat` against the named peer.
    /// When absent, jobs fire but the AI step is skipped —
    /// the task chronicle still records `cron.job_fired` and
    /// the task moves to `failed` with cause
    /// "ai dispatcher unset".
    #[serde(default, rename = "ai_peer")]
    pub ai_peer: Option<CronAiPeerConfig>,
}

impl Default for CronSchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            tick_secs: default_tick_secs(),
            max_concurrent: default_max_concurrent(),
            max_job_secs: default_max_job_secs(),
            ai_peer: None,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_tick_secs() -> u64 {
    30
}

fn default_max_concurrent() -> usize {
    3
}

fn default_max_job_secs() -> u64 {
    300
}

/// `[coordinator.cron.ai_peer]` — names the AI peer the
/// scheduler should dial. Mirrors the memory curator's pattern.
#[derive(Clone, Debug, Deserialize)]
pub struct CronAiPeerConfig {
    pub addr: String,
    #[serde(default = "default_ai_alias")]
    pub alias: String,
    #[serde(default = "default_ai_deadline")]
    pub deadline_secs: i64,
}

fn default_ai_alias() -> String {
    "ai".to_string()
}

fn default_ai_deadline() -> i64 {
    60
}

// ── AI dispatcher ────────────────────────────────────────

/// Async hook the scheduler reaches through to call `ai.chat`.
/// Production wraps a `MeshClient`; tests stub it.
#[async_trait]
pub trait CronAiDispatcher: Send + Sync {
    /// Return the model's reply text on success, `None` on any
    /// failure (network, decode, responder err).
    async fn chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String>;
}

/// Live `CronAiDispatcher` — wraps a `MeshClient` pointing at
/// the AI peer. Built by the coordinator at startup via
/// `discover_and_pin`, identical pattern to the memory
/// curator's `AiMeshDispatcher`.
#[derive(Clone)]
pub struct CronAiMeshDispatcher {
    mesh: crate::manifest::MeshClient,
    alias: String,
    identity: relix_core::bundle::Bundle,
    deadline_secs: i64,
}

impl CronAiMeshDispatcher {
    pub fn new(
        mesh: crate::manifest::MeshClient,
        alias: String,
        identity: relix_core::bundle::Bundle,
        deadline_secs: i64,
    ) -> Self {
        Self {
            mesh,
            alias,
            identity,
            deadline_secs,
        }
    }
}

#[async_trait]
impl CronAiDispatcher for CronAiMeshDispatcher {
    async fn chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
        use crate::dispatch::{build_request, decode_response};
        use crate::transport::envelope::ResponseResult;
        let arg = format!("{session_id}|{prompt}|{history}");
        let envelope = build_request(
            "ai.chat",
            arg.into_bytes(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = match self.mesh.call(&self.alias, envelope).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    alias = %self.alias,
                    error = %e,
                    "cron: ai.chat fetch failed (silent skip)"
                );
                return None;
            }
        };
        let env = match decode_response(&resp_bytes) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "cron: ai.chat decode failed");
                return None;
            }
        };
        match env.res {
            ResponseResult::Ok(b) => Some(String::from_utf8_lossy(&b).to_string()),
            ResponseResult::Err(env) => {
                tracing::warn!(kind = env.kind, cause = %env.cause, "cron: ai.chat responder err");
                None
            }
            ResponseResult::StreamHandle(_) => None,
        }
    }
}

/// Lazily-populated dispatcher cell. The scheduler reads it on
/// every tick; while empty, jobs fire (task is created, chronicle
/// recorded) but the AI step is skipped.
pub type CronAiDispatcherCell = Arc<OnceCell<Arc<dyn CronAiDispatcher>>>;

// ── Outcome of one fire ────────────────────────────────────

/// What `fire_job` returns to its caller. Both the scheduler
/// tick and the `cron.trigger` handler consume this.
#[derive(Debug, PartialEq, Eq)]
pub enum FireOutcome {
    /// Job was fired. Carries the new `task_id`.
    Fired(String),
    /// Previous task was still `running` — fire skipped.
    SkippedPreviousRunning,
    /// Storage / database failure.
    Failed(String),
}

/// Fire one cron job: create a task, write the chronicle
/// event, advance the cron row, and spawn the AI dispatch in
/// the background. Returns the new task_id (Fired), or a
/// reason (SkippedPreviousRunning / Failed).
///
/// The AI dispatch is detached so the scheduler tick stays
/// short. `max_job_secs` is enforced via `tokio::time::timeout`
/// inside the spawned task.
pub async fn fire_job(
    job: &CronJob,
    task_store: Arc<TaskStore>,
    cron_store: Arc<CronStore>,
    ai_cell: CronAiDispatcherCell,
    max_job_secs: u64,
) -> FireOutcome {
    // 1. Skip if the previous task is still running.
    if let Some(last) = job.last_task_id.as_deref()
        && let Ok(Some(view)) = task_store.get(last)
        && view.status == "running"
    {
        tracing::warn!(
            job = %job.name,
            last_task_id = %last,
            "cron: previous task still running; skipping fire"
        );
        return FireOutcome::SkippedPreviousRunning;
    }

    // 2. Create the coordinator task.
    let title = format!("cron:{}", job.name);
    let params_json = format!(
        "{{\"cron_job_id\":\"{}\",\"prompt\":\"{}\"}}",
        job.job_id,
        json_escape(&job.prompt)
    );
    let task_id = match task_store.create(
        &title,
        &job.flow_template,
        &params_json,
        &job.subject_id,
        crate::nodes::coordinator::RetryPolicy::None,
        0,
        Some(max_job_secs as i64),
        Some("scheduler"),
    ) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(job = %job.name, error = %e, "cron: task.create failed");
            return FireOutcome::Failed(format!("task.create: {e}"));
        }
    };

    // 3. Chronicle event.
    let payload = format!(
        "job_id={}|job_name={}|run_count={}",
        job.job_id,
        job.name,
        job.run_count + 1
    );
    if let Err(e) = task_store.append_event(&task_id, "cron.job_fired", &payload) {
        tracing::warn!(error = %e, "cron: append_event(cron.job_fired) failed");
    }

    // 4. Stamp the cron row + one-shot disable.
    let now = unix_now();
    let schedule = Schedule::parse(&job.schedule).ok();
    let next_run_at = match &schedule {
        Some(s) => s.next_after(now),
        None => now + 60, // unparseable: re-try in 60s
    };
    let disable_after = matches!(schedule, Some(Schedule::OneShot { .. }));
    if let Err(e) = cron_store.record_fire(&job.job_id, now, next_run_at, &task_id, disable_after) {
        tracing::warn!(error = %e, "cron: record_fire failed");
    }
    tracing::info!(job = %job.name, task_id = %task_id, "cron: fired job -> task");

    // 5. Spawn the AI dispatch + completion bookkeeping.
    let job_id = job.job_id.clone();
    let subject = job.subject_id.clone();
    let prompt = job.prompt.clone();
    let task_id_for_spawn = task_id.clone();
    let task_store_for_spawn = task_store.clone();
    let cron_store_for_spawn = cron_store.clone();
    tokio::spawn(async move {
        run_ai_then_complete(
            ai_cell,
            task_store_for_spawn,
            cron_store_for_spawn,
            job_id,
            task_id_for_spawn,
            subject,
            prompt,
            max_job_secs,
        )
        .await;
    });

    FireOutcome::Fired(task_id)
}

#[allow(clippy::too_many_arguments)]
async fn run_ai_then_complete(
    ai_cell: CronAiDispatcherCell,
    task_store: Arc<TaskStore>,
    cron_store: Arc<CronStore>,
    job_id: String,
    task_id: String,
    subject: String,
    prompt: String,
    max_job_secs: u64,
) {
    let outcome = match ai_cell.get().cloned() {
        Some(d) => match tokio::time::timeout(
            Duration::from_secs(max_job_secs),
            d.chat(&subject, &prompt, ""),
        )
        .await
        {
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err("ai dispatcher returned None".to_string()),
            Err(_) => Err(format!("ai dispatch exceeded max_job_secs={max_job_secs}")),
        },
        None => Err("ai dispatcher unset".to_string()),
    };
    let (status, result, payload_summary) = match outcome {
        Ok(reply) => {
            let payload = format!(
                "ok=1|chars={}|preview={}",
                reply.chars().count(),
                preview(&reply, 200)
            );
            let _ = task_store.append_event(&task_id, "cron.job_result", &payload);
            ("completed", reply, "ok".to_string())
        }
        Err(cause) => {
            let payload = format!("ok=0|cause={}", cause.replace('|', " "));
            let _ = task_store.append_event(&task_id, "cron.job_result", &payload);
            ("failed", cause, "failed".to_string())
        }
    };
    // Trim result so we don't dump a 100k AI reply into the
    // tasks table's `latest_result` column.
    let trimmed_result = preview(&result, 800);
    let _ = task_store.update(
        &task_id,
        Some(status),
        Some(&trimmed_result),
        None,
        None,
        None,
        None,
        None,
    );
    let _ = cron_store.record_status(&job_id, &payload_summary);
}

fn preview(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    cleaned.chars().take(max_chars).collect()
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Scheduler loop ────────────────────────────────────────

/// Spawn the periodic cron scheduler. Runs forever; if the
/// caller drops the JoinHandle the loop keeps running until
/// the process exits.
pub fn spawn_cron_scheduler(
    task_store: Arc<TaskStore>,
    cron_store: Arc<CronStore>,
    ai_cell: CronAiDispatcherCell,
    cfg: CronSchedulerConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(cfg.tick_secs.max(1)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let sem = Arc::new(Semaphore::new(cfg.max_concurrent.max(1)));
        loop {
            interval.tick().await;
            run_one_tick(
                task_store.clone(),
                cron_store.clone(),
                ai_cell.clone(),
                sem.clone(),
                cfg.max_job_secs,
            )
            .await;
        }
    })
}

/// Run a single scheduler tick. Public so unit tests can drive
/// the loop manually without waiting for wall-clock seconds.
pub async fn run_one_tick(
    task_store: Arc<TaskStore>,
    cron_store: Arc<CronStore>,
    ai_cell: CronAiDispatcherCell,
    sem: Arc<Semaphore>,
    max_job_secs: u64,
) {
    let now = unix_now();
    let due = match cron_store.due_jobs(now) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "cron: due_jobs failed");
            return;
        }
    };
    if due.is_empty() {
        return;
    }
    for job in due {
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                // No slot available — leave the job behind;
                // the next tick will pick it up.
                tracing::debug!(
                    job = %job.name,
                    "cron: max_concurrent reached; deferring fire"
                );
                continue;
            }
        };
        let task_store2 = task_store.clone();
        let cron_store2 = cron_store.clone();
        let ai_cell2 = ai_cell.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = fire_job(&job, task_store2, cron_store2, ai_cell2, max_job_secs).await;
        });
    }
}

// ── cron.trigger handler ──────────────────────────────────

/// Register `cron.trigger`. Lives here so the manual + periodic
/// fire paths share [`fire_job`].
pub fn register_trigger(
    bridge: &mut DispatchBridge,
    task_store: Arc<TaskStore>,
    cron_store: Arc<CronStore>,
    ai_cell: CronAiDispatcherCell,
    max_job_secs: u64,
) {
    let task_store = task_store;
    let cron_store = cron_store;
    bridge.register(
        "cron.trigger",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let task_store = task_store.clone();
            let cron_store = cron_store.clone();
            let ai_cell = ai_cell.clone();
            async move {
                let job_id = match std::str::from_utf8(&ctx.args) {
                    Ok(s) => s.trim().to_string(),
                    Err(e) => return invalid(format!("cron.trigger utf8: {e}")),
                };
                if job_id.is_empty() {
                    return invalid("cron.trigger: job_id required".into());
                }
                let job = match cron_store.get(&job_id) {
                    Ok(Some(j)) => j,
                    Ok(None) => return invalid(format!("cron.trigger: not found: {job_id}")),
                    Err(e) => return internal(format!("cron.trigger: {e}")),
                };
                match fire_job(&job, task_store, cron_store, ai_cell, max_job_secs).await {
                    FireOutcome::Fired(task_id) => {
                        HandlerOutcome::Ok(format!("{task_id}\n").into_bytes())
                    }
                    FireOutcome::SkippedPreviousRunning => {
                        invalid("cron.trigger: previous task still running; skipped".into())
                    }
                    FireOutcome::Failed(m) => internal(format!("cron.trigger: {m}")),
                }
            }
        })),
    );
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
        kind: relix_core::types::error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stores() -> (Arc<TaskStore>, Arc<CronStore>) {
        let task_store = Arc::new(TaskStore::in_memory().unwrap());
        let cron_store = Arc::new(CronStore::in_memory().unwrap());
        (task_store, cron_store)
    }

    #[derive(Default)]
    struct StubAi {
        reply: std::sync::Mutex<Option<String>>,
        calls: std::sync::Mutex<u32>,
    }

    #[async_trait]
    impl CronAiDispatcher for StubAi {
        async fn chat(&self, _session: &str, _prompt: &str, _hist: &str) -> Option<String> {
            *self.calls.lock().unwrap() += 1;
            self.reply.lock().unwrap().clone()
        }
    }

    fn ai_cell_with(reply: Option<&str>) -> (CronAiDispatcherCell, Arc<StubAi>) {
        let cell: CronAiDispatcherCell = Arc::new(OnceCell::new());
        let stub = Arc::new(StubAi::default());
        *stub.reply.lock().unwrap() = reply.map(|s| s.to_string());
        assert!(cell.set(stub.clone() as Arc<dyn CronAiDispatcher>).is_ok());
        (cell, stub)
    }

    fn force_due(cron: &CronStore, job_id: &str) {
        // Reach into the connection just for tests: push the
        // job's next_run_at into the past so the scheduler
        // picks it up.
        let conn = cron.conn_for_tests();
        conn.execute(
            "UPDATE cron_jobs SET next_run_at = 0 WHERE job_id = ?1",
            rusqlite::params![job_id],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn run_one_tick_fires_a_due_job_and_creates_a_task() {
        let (tasks, cron) = stores();
        let id = cron
            .create("daily", "1d", "f.sol", "summarise", "subj-1", "default")
            .unwrap();
        force_due(&cron, &id);
        let (cell, ai) = ai_cell_with(Some("ai reply"));
        let sem = Arc::new(Semaphore::new(3));
        run_one_tick(tasks.clone(), cron.clone(), cell, sem, 30).await;
        // Wait briefly for the spawned fire_job to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Job advanced and recorded a task_id.
        let j = cron.get(&id).unwrap().unwrap();
        assert!(j.last_task_id.is_some(), "expected a task_id on the job");
        assert_eq!(j.run_count, 1);
        // AI was invoked once.
        assert_eq!(*ai.calls.lock().unwrap(), 1);
        // Task lives in the task store with title cron:daily.
        let tid = j.last_task_id.unwrap();
        let view = tasks.get(&tid).unwrap().unwrap();
        assert_eq!(view.title, "cron:daily");
        assert_eq!(view.origin_surface.as_deref(), Some("scheduler"));
    }

    #[tokio::test]
    async fn run_one_tick_skips_non_due_jobs() {
        let (tasks, cron) = stores();
        let id = cron
            .create("future", "1d", "f.sol", "p", "subj-1", "default")
            .unwrap();
        let (cell, ai) = ai_cell_with(Some("x"));
        let sem = Arc::new(Semaphore::new(3));
        run_one_tick(tasks.clone(), cron.clone(), cell, sem, 30).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*ai.calls.lock().unwrap(), 0);
        assert_eq!(cron.get(&id).unwrap().unwrap().run_count, 0);
    }

    #[tokio::test]
    async fn one_shot_is_disabled_after_firing() {
        let (tasks, cron) = stores();
        // ISO instant in the past so it's immediately due.
        let id = cron
            .create(
                "once",
                "2020-01-01T00:00:00Z",
                "f.sol",
                "p",
                "subj",
                "default",
            )
            .unwrap();
        let (cell, _ai) = ai_cell_with(Some("x"));
        let sem = Arc::new(Semaphore::new(3));
        run_one_tick(tasks, cron.clone(), cell, sem, 30).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let j = cron.get(&id).unwrap().unwrap();
        assert_eq!(j.run_count, 1);
        assert!(!j.enabled, "one-shot must be disabled after fire");
    }

    #[tokio::test]
    async fn already_running_previous_task_is_skipped() {
        let (tasks, cron) = stores();
        let id = cron
            .create("daily", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        // Pretend a previous task is still running.
        let prev_task = tasks
            .create(
                "cron:daily",
                "f.sol",
                "",
                "subj",
                crate::nodes::coordinator::RetryPolicy::None,
                0,
                None,
                Some("scheduler"),
            )
            .unwrap();
        tasks
            .update(
                &prev_task,
                Some("running"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        cron.record_fire(&id, 1, 0, &prev_task, false).unwrap();
        // Force due again.
        force_due(&cron, &id);

        let job = cron.get(&id).unwrap().unwrap();
        let (cell, _ai) = ai_cell_with(Some("x"));
        let result = fire_job(&job, tasks.clone(), cron.clone(), cell, 30).await;
        assert_eq!(result, FireOutcome::SkippedPreviousRunning);
    }

    #[tokio::test]
    async fn semaphore_caps_concurrent_fires() {
        // max_concurrent = 1; two due jobs. Only one fires
        // immediately; the other is deferred (run_count stays 0).
        let (tasks, cron) = stores();
        let a = cron
            .create("a", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        let b = cron
            .create("b", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        force_due(&cron, &a);
        force_due(&cron, &b);
        let (cell, _ai) = ai_cell_with(Some("x"));
        let sem = Arc::new(Semaphore::new(1));
        // Pre-acquire the single permit so the tick can't fire.
        let _hold = sem.clone().try_acquire_owned().unwrap();
        run_one_tick(tasks, cron.clone(), cell, sem, 30).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(cron.get(&a).unwrap().unwrap().run_count, 0);
        assert_eq!(cron.get(&b).unwrap().unwrap().run_count, 0);
    }

    #[tokio::test]
    async fn ai_timeout_results_in_failed_task() {
        let (tasks, cron) = stores();
        let id = cron
            .create("a", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        force_due(&cron, &id);

        // Dispatcher that never responds.
        struct StallingAi;
        #[async_trait]
        impl CronAiDispatcher for StallingAi {
            async fn chat(&self, _s: &str, _p: &str, _h: &str) -> Option<String> {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Some("never".into())
            }
        }
        let cell: CronAiDispatcherCell = Arc::new(OnceCell::new());
        assert!(
            cell.set(Arc::new(StallingAi) as Arc<dyn CronAiDispatcher>)
                .is_ok()
        );
        let sem = Arc::new(Semaphore::new(1));
        // max_job_secs = 0 forces immediate timeout.
        run_one_tick(tasks.clone(), cron.clone(), cell, sem, 0).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let tid = cron.get(&id).unwrap().unwrap().last_task_id.unwrap();
        let view = tasks.get(&tid).unwrap().unwrap();
        assert_eq!(view.status, "failed");
    }

    #[tokio::test]
    async fn fire_job_writes_cron_job_fired_chronicle_event() {
        let (tasks, cron) = stores();
        let id = cron
            .create("daily", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        let job = cron.get(&id).unwrap().unwrap();
        let (cell, _ai) = ai_cell_with(Some("x"));
        let outcome = fire_job(&job, tasks.clone(), cron.clone(), cell, 30).await;
        let tid = match outcome {
            FireOutcome::Fired(t) => t,
            other => panic!("expected Fired, got {other:?}"),
        };
        // Read the task's chronicle and look for cron.job_fired.
        let events = tasks
            .query_events(
                &tid,
                0,
                100,
                None,
                crate::nodes::coordinator::EventOrder::Asc,
            )
            .unwrap();
        assert!(events.iter().any(|e| e.event_type == "cron.job_fired"));
    }
}
