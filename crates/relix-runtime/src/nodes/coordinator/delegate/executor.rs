//! Delegation executor — picks up pending delegated tasks and
//! dispatches `ai.chat` against the configured AI peer.
//!
//! The handlers in `handlers.rs` create the child task and
//! the `delegated_to` edge; this loop is what actually runs
//! the child. Same pattern as the cron scheduler:
//!
//! 1. Periodic tick (default 5 s).
//! 2. Query `tasks WHERE origin_surface = 'delegation' AND
//!    status = 'pending'`.
//! 3. Acquire a semaphore permit (default `max_concurrent = 5`).
//! 4. Spawn a tokio task per due child: flip to `running`,
//!    dispatch ai.chat with the goal + context, write the
//!    reply to `latest_result`, flip to `completed` /
//!    `failed`, then flip the parent back to `running` with
//!    a `delegate.child_completed` chronicle event so the
//!    polling agent loop sees the new state.
//!
//! Hard timeout per child via `tokio::time::timeout(max_job_secs)`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::{OnceCell, Semaphore};

use crate::nodes::coordinator::TaskStore;
use crate::nodes::coordinator::delegate::handlers::{
    extract_context_from_params, extract_goal_from_params,
};

// ── Config ────────────────────────────────────────────────

/// `[coordinator.delegation]` config section. Optional —
/// absence means the executor loop is not spawned. The
/// capabilities are still registered (a SOL flow can still
/// create child tasks; they just stay `pending` until the
/// executor is enabled).
#[derive(Clone, Debug, Deserialize)]
pub struct DelegationConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Maximum delegation chain depth. Default 3.
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    /// Maximum jobs the executor will fire in flight.
    /// Default 5.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Seconds between executor poll ticks. Default 5 s.
    #[serde(default = "default_poll_secs")]
    pub executor_poll_secs: u64,
    /// Hard per-job timeout for the ai.chat dispatch.
    /// Default 300 s.
    #[serde(default = "default_max_job_secs")]
    pub max_job_secs: u64,
    /// Optional outbound AI peer. Same shape as the cron
    /// scheduler's `[coordinator.cron.ai_peer]`. When set
    /// the post-startup wiring builds a
    /// [`DelegationAiMeshDispatcher`] into the shared cell.
    #[serde(default, rename = "ai_peer")]
    pub ai_peer: Option<DelegationAiPeerConfig>,
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            max_depth: default_max_depth(),
            max_concurrent: default_max_concurrent(),
            executor_poll_secs: default_poll_secs(),
            max_job_secs: default_max_job_secs(),
            ai_peer: None,
        }
    }
}

fn default_enabled() -> bool {
    true
}
fn default_max_depth() -> usize {
    3
}
fn default_max_concurrent() -> usize {
    5
}
fn default_poll_secs() -> u64 {
    5
}
fn default_max_job_secs() -> u64 {
    300
}

#[derive(Clone, Debug, Deserialize)]
pub struct DelegationAiPeerConfig {
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

#[async_trait]
pub trait DelegationAiDispatcher: Send + Sync {
    /// Returns the model's reply text on success, `None` on
    /// any failure (network, decode, responder error).
    async fn chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String>;
}

/// Live impl wrapping a `MeshClient` against the configured
/// AI peer. Mirror of the cron scheduler's
/// `CronAiMeshDispatcher`.
#[derive(Clone)]
pub struct DelegationAiMeshDispatcher {
    mesh: crate::manifest::MeshClient,
    alias: String,
    identity: relix_core::bundle::Bundle,
    deadline_secs: i64,
}

impl DelegationAiMeshDispatcher {
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
impl DelegationAiDispatcher for DelegationAiMeshDispatcher {
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
                tracing::warn!(error = %e, "delegate: ai.chat fetch failed");
                return None;
            }
        };
        let env = match decode_response(&resp_bytes) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "delegate: ai.chat decode failed");
                return None;
            }
        };
        match env.res {
            ResponseResult::Ok(b) => Some(String::from_utf8_lossy(&b).to_string()),
            ResponseResult::Err(env) => {
                tracing::warn!(kind = env.kind, cause = %env.cause, "delegate: ai.chat responder err");
                None
            }
            ResponseResult::StreamHandle(_) => None,
        }
    }
}

pub type DelegationAiDispatcherCell = Arc<OnceCell<Arc<dyn DelegationAiDispatcher>>>;

// ── Executor loop ────────────────────────────────────────

/// Spawn the periodic executor task. Runs forever; drop the
/// returned JoinHandle to let it run until the process exits.
pub fn spawn_delegation_executor(
    task_store: Arc<TaskStore>,
    ai_cell: DelegationAiDispatcherCell,
    cfg: DelegationConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(cfg.executor_poll_secs.max(1)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let sem = Arc::new(Semaphore::new(cfg.max_concurrent.max(1)));
        loop {
            interval.tick().await;
            run_one_tick(
                task_store.clone(),
                ai_cell.clone(),
                sem.clone(),
                cfg.max_job_secs,
            )
            .await;
        }
    })
}

/// Single executor tick. Public so unit tests can drive the
/// loop manually without waiting for wall-clock seconds.
pub async fn run_one_tick(
    task_store: Arc<TaskStore>,
    ai_cell: DelegationAiDispatcherCell,
    sem: Arc<Semaphore>,
    max_job_secs: u64,
) {
    let pending = match task_store.list_pending_delegated(50) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "delegate: list_pending_delegated failed");
            return;
        }
    };
    for (child_id, params_json, owner) in pending {
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::debug!(
                    child = %child_id,
                    "delegate: max_concurrent reached; deferring"
                );
                continue;
            }
        };
        let task_store2 = task_store.clone();
        let ai_cell2 = ai_cell.clone();
        tokio::spawn(async move {
            let _permit = permit;
            run_one_child(
                task_store2,
                ai_cell2,
                child_id,
                params_json,
                owner,
                max_job_secs,
            )
            .await;
        });
    }
}

async fn run_one_child(
    task_store: Arc<TaskStore>,
    ai_cell: DelegationAiDispatcherCell,
    child_id: String,
    params_json: String,
    owner: String,
    max_job_secs: u64,
) {
    // Flip pending → running. If we lose a race against another
    // executor instance (or against delegate.cancel), bail
    // silently — the row's new owner runs it.
    if let Err(e) = task_store.update(
        &child_id,
        Some("running"),
        None,
        None,
        None,
        None,
        None,
        None,
    ) {
        tracing::warn!(child = %child_id, error = %e, "delegate: pending→running failed");
        return;
    }
    let goal = extract_goal_from_params(&params_json);
    let context = extract_context_from_params(&params_json);
    let history = render_history(&context);

    let outcome = match ai_cell.get().cloned() {
        Some(d) => match tokio::time::timeout(
            Duration::from_secs(max_job_secs),
            d.chat(&owner, &goal, &history),
        )
        .await
        {
            Ok(Some(reply)) if !reply.trim().is_empty() => Ok(reply),
            Ok(Some(_)) => Err("ai chat returned empty reply".to_string()),
            Ok(None) => Err("ai dispatcher returned None".to_string()),
            Err(_) => Err(format!("ai dispatch exceeded max_job_secs={max_job_secs}")),
        },
        None => Err("ai dispatcher unset".to_string()),
    };

    let (status, result_trimmed, event_type, event_payload) = match outcome {
        Ok(reply) => {
            let trimmed = preview(&reply, 800);
            (
                "completed",
                trimmed,
                "delegate.completed",
                format!(
                    "chars={}|preview={}",
                    reply.chars().count(),
                    preview(&reply, 200)
                ),
            )
        }
        Err(cause) => (
            "failed",
            preview(&cause, 800),
            "delegate.failed",
            format!("cause={}", cause.replace('|', " ")),
        ),
    };
    // Append on the child first so the chronicle has the
    // outcome BEFORE the status flip; reading clients that
    // re-query on completion always see the matching event.
    if let Err(e) = task_store.append_event(&child_id, event_type, &event_payload) {
        tracing::warn!(child = %child_id, error = %e, "delegate: child chronicle event failed");
    }
    if let Err(e) = task_store.update(
        &child_id,
        Some(status),
        Some(&result_trimmed),
        None,
        None,
        None,
        None,
        None,
    ) {
        tracing::warn!(child = %child_id, error = %e, "delegate: child status flip failed");
    }

    // Walk up to the parent via the delegated_to edge so we
    // can resume them. `list_edges_for_task` returns both
    // sides of the edge; we want the row where the child is
    // the `related_task_id`.
    let parent_task_id = match task_store.list_edges_for_task(&child_id) {
        Ok(edges) => edges.into_iter().find_map(|e| {
            if e.edge_type == "delegated_to"
                && e.related_task_id.as_deref() == Some(child_id.as_str())
            {
                Some(e.task_id)
            } else {
                None
            }
        }),
        Err(e) => {
            tracing::warn!(child = %child_id, error = %e, "delegate: list_edges_for_task failed");
            None
        }
    };

    if let Some(parent) = parent_task_id {
        let payload = format!(
            "child_task_id={child_id}|status={status}|preview={}",
            preview(&result_trimmed, 200).replace('|', " ")
        );
        if let Err(e) = task_store.append_event(&parent, "delegate.child_completed", &payload) {
            tracing::warn!(parent = %parent, error = %e, "delegate: parent chronicle event failed");
        }
        // Flip the parent back to running, but only when it's
        // still awaiting_input — don't trample paused / frozen
        // / terminal states.
        if let Ok(Some(pv)) = task_store.get(&parent)
            && pv.status == "awaiting_input"
            && let Err(e) =
                task_store.update(&parent, Some("running"), None, None, None, None, None, None)
        {
            tracing::warn!(parent = %parent, error = %e, "delegate: parent resume failed");
        }
    }
}

fn render_history(context: &str) -> String {
    if context.trim().is_empty() {
        String::new()
    } else {
        // Inject the context as a system-style history block
        // the AI peer's frozen-snapshot memory injection
        // already understands.
        format!("[delegation_context] {context}\n")
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::coordinator::RetryPolicy;
    use std::sync::Mutex;

    fn stores() -> Arc<TaskStore> {
        Arc::new(TaskStore::in_memory().unwrap())
    }

    fn make_parent(s: &TaskStore) -> String {
        let p = s
            .create(
                "parent",
                "agent.sol",
                "{}",
                "subj-a",
                RetryPolicy::None,
                0,
                None,
                Some("dashboard"),
            )
            .unwrap();
        s.update(&p, Some("running"), None, None, None, None, None, None)
            .unwrap();
        p
    }

    fn spawn_child(s: &TaskStore, parent: &str, goal: &str, context: &str) -> String {
        let arg = format!("{parent}|{goal}|{context}||0");
        let out = super::super::handlers::handle_spawn(
            s,
            &super::super::handlers::fake_ctx(arg.as_bytes()),
            3,
        );
        match out {
            crate::dispatch::HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap().trim().into(),
            crate::dispatch::HandlerOutcome::Err(e) => panic!("spawn failed: {}", e.cause),
        }
    }

    #[derive(Default)]
    struct StubAi {
        reply: Mutex<Option<String>>,
        calls: Mutex<Vec<(String, String, String)>>,
    }
    #[async_trait]
    impl DelegationAiDispatcher for StubAi {
        async fn chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
            self.calls
                .lock()
                .unwrap()
                .push((session_id.into(), prompt.into(), history.into()));
            self.reply.lock().unwrap().clone()
        }
    }

    fn cell_with(reply: Option<&str>) -> (DelegationAiDispatcherCell, Arc<StubAi>) {
        let cell: DelegationAiDispatcherCell = Arc::new(OnceCell::new());
        let stub = Arc::new(StubAi::default());
        *stub.reply.lock().unwrap() = reply.map(|s| s.to_string());
        let _ = cell.set(stub.clone() as Arc<dyn DelegationAiDispatcher>);
        (cell, stub)
    }

    #[tokio::test]
    async fn pending_delegated_task_gets_picked_up_and_executed() {
        let s = stores();
        let parent = make_parent(&s);
        let child = spawn_child(&s, &parent, "do the thing", "ctx-info");
        let (cell, ai) = cell_with(Some("the answer"));
        let sem = Arc::new(Semaphore::new(2));

        run_one_tick(s.clone(), cell, sem, 10).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // AI was invoked once with the goal as prompt.
        let calls = ai.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "do the thing");
        assert!(calls[0].2.contains("ctx-info"));

        // Child moved to completed and got the reply.
        let view = s.get(&child).unwrap().unwrap();
        assert_eq!(view.status, "completed");
        assert_eq!(view.latest_result.as_deref(), Some("the answer"));
    }

    #[tokio::test]
    async fn child_moves_to_failed_when_ai_returns_none() {
        let s = stores();
        let parent = make_parent(&s);
        let child = spawn_child(&s, &parent, "g", "");
        let (cell, _ai) = cell_with(None);
        let sem = Arc::new(Semaphore::new(2));
        run_one_tick(s.clone(), cell, sem, 10).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let view = s.get(&child).unwrap().unwrap();
        assert_eq!(view.status, "failed");
    }

    #[tokio::test]
    async fn parent_resumes_to_running_after_child_completes() {
        let s = stores();
        let parent = make_parent(&s);
        let child = spawn_child(&s, &parent, "g", "");
        // Parent should be in awaiting_input after spawn.
        assert_eq!(s.get(&parent).unwrap().unwrap().status, "awaiting_input");
        let (cell, _ai) = cell_with(Some("ok"));
        let sem = Arc::new(Semaphore::new(2));
        run_one_tick(s.clone(), cell, sem, 10).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(s.get(&child).unwrap().unwrap().status, "completed");
        assert_eq!(s.get(&parent).unwrap().unwrap().status, "running");
        // delegate.child_completed event lives on the parent.
        let events = s
            .query_events(
                &parent,
                0,
                100,
                None,
                crate::nodes::coordinator::EventOrder::Asc,
            )
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "delegate.child_completed")
        );
    }

    #[tokio::test]
    async fn cancelled_task_is_not_executed() {
        let s = stores();
        let parent = make_parent(&s);
        let child = spawn_child(&s, &parent, "g", "");
        // Cancel before the executor runs.
        s.update(
            &child,
            Some("cancelled"),
            Some("cancelled by test"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let (cell, ai) = cell_with(Some("would-not-fire"));
        let sem = Arc::new(Semaphore::new(2));
        run_one_tick(s.clone(), cell, sem, 10).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(ai.calls.lock().unwrap().is_empty());
        assert_eq!(s.get(&child).unwrap().unwrap().status, "cancelled");
    }

    #[tokio::test]
    async fn semaphore_caps_concurrent_executions() {
        let s = stores();
        let parent = make_parent(&s);
        // Two children both pending.
        let c1 = spawn_child(&s, &parent, "g1", "");
        // Reset parent so the second spawn passes admission.
        s.update(&parent, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let c2 = spawn_child(&s, &parent, "g2", "");
        let (cell, _ai) = cell_with(Some("x"));
        let sem = Arc::new(Semaphore::new(1));
        // Pre-acquire the only permit so the tick can't fire.
        let _hold = sem.clone().try_acquire_owned().unwrap();
        run_one_tick(s.clone(), cell, sem, 10).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Neither child ran.
        assert_eq!(s.get(&c1).unwrap().unwrap().status, "pending");
        assert_eq!(s.get(&c2).unwrap().unwrap().status, "pending");
    }

    #[tokio::test]
    async fn executor_injects_context_into_ai_chat_history() {
        let s = stores();
        let parent = make_parent(&s);
        let _ = spawn_child(&s, &parent, "g", "important-context-string");
        let (cell, ai) = cell_with(Some("ok"));
        let sem = Arc::new(Semaphore::new(2));
        run_one_tick(s.clone(), cell, sem, 10).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let calls = ai.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].2.contains("important-context-string"));
        assert!(calls[0].2.contains("[delegation_context]"));
    }
}
