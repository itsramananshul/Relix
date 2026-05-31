//! RELIX-7.28 Part 1 — cost-control budget enforcement.
//!
//! Two surfaces:
//!
//! - [`BudgetConfig`] is the operator-facing TOML projection. It carries the
//!   per-agent limit rows plus an optional deployment-wide cap.
//! - [`BudgetEnforcer`] is the runtime enforcement object the dispatch
//!   bridge consults before invoking a handler. The enforcer keeps an
//!   in-memory cache of the current daily + hourly accumulated cost per
//!   agent so the hot path never reaches SQLite — the cache is refreshed
//!   from the metrics store at a configurable interval AND invalidated
//!   immediately when a cost-bearing metric lands.
//!
//! The enforcer is mesh-side observability: it does NOT replace policy or
//! identity admission. It runs after the agent-employee gate + access
//! broker, before the handler.
//!
//! ## Actions
//!
//! - `throttle` — sleeps for the configured backoff (default 2s) before
//!   admitting the call. Continues to throttle every subsequent call in
//!   the same window until the window resets.
//! - `reject` — short-circuits with a `RESOURCE_EXHAUSTED` error
//!   envelope. The caller sees a human-readable limit + reset time.
//! - `alert_only` — emits a `BudgetExceeded` alert through the wired
//!   `AlertSink` and lets the call through.
//!
//! Deployment-level caps are checked AFTER agent caps: the agent cap may
//! reject/throttle before the deployment cap is even examined.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::alert::{ActiveAlert, AlertDeliver, AlertEvent, AlertKind, AlertSeverity};
use super::collector::now_ms;
use super::query::MetricsQuery;

/// What a `BudgetEnforcer::check` returns to the dispatch bridge.
#[derive(Clone, Debug, PartialEq)]
pub enum BudgetDecision {
    /// Call is within every applicable limit. Dispatch proceeds.
    Allow,
    /// The matched limit is configured `throttle`. Bridge sleeps for the
    /// embedded duration before dispatching. `info` carries human-readable
    /// detail for the operator-facing chronicle event.
    Throttle { delay: Duration, info: BudgetBreach },
    /// The matched limit is configured `reject`. Bridge synthesises a
    /// `RESOURCE_EXHAUSTED` error envelope using `info.cause`.
    Reject { info: BudgetBreach },
}

/// One breach record. Same shape regardless of action so the audit chronicle
/// + alert pipeline see uniform fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetBreach {
    /// Agent whose budget tripped — empty string when the deployment-level
    /// cap was hit (no single agent owns it).
    pub agent: String,
    /// `"daily"` | `"hourly"`.
    pub window: String,
    /// `"agent"` | `"deployment"`.
    pub scope: String,
    /// Configured limit in micro-USD.
    pub limit_micros: u64,
    /// Current accumulated spend in micro-USD.
    pub actual_micros: u64,
    /// Unix-ms timestamp when the current window resets.
    pub resets_at_ms: i64,
    /// Operator-readable cause string.
    pub cause: String,
}

impl BudgetBreach {
    /// Build the alert message body the multi-channel sink renders.
    pub fn alert_message(&self) -> String {
        let scope = if self.scope == "deployment" {
            "deployment".to_string()
        } else if self.agent.is_empty() {
            "agent".to_string()
        } else {
            format!("agent {}", self.agent)
        };
        format!(
            "Budget exceeded — {scope}: {limit:.4} USD {window} limit (actual ${actual:.4}, resets at unix-ms {reset})",
            limit = self.limit_micros as f64 / 1_000_000.0,
            window = self.window,
            actual = self.actual_micros as f64 / 1_000_000.0,
            reset = self.resets_at_ms,
        )
    }
}

/// Operator-facing action on a budget cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetAction {
    /// Sleep `throttle_backoff_ms`, then allow the call. Continues every
    /// time the call passes through the same window.
    #[default]
    Throttle,
    /// Reject with `RESOURCE_EXHAUSTED` immediately.
    Reject,
    /// Allow the call but fire a `BudgetExceeded` alert.
    AlertOnly,
}

impl BudgetAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "throttle" => Some(Self::Throttle),
            "reject" => Some(Self::Reject),
            "alert_only" | "alert-only" | "alertonly" => Some(Self::AlertOnly),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Throttle => "throttle",
            Self::Reject => "reject",
            Self::AlertOnly => "alert_only",
        }
    }
}

/// One `[[budget.agents]]` row.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentBudget {
    pub agent: String,
    /// Optional daily limit in USD. `None` skips the daily check for this
    /// agent.
    #[serde(default)]
    pub daily_limit_usd: Option<f64>,
    /// Optional hourly limit in USD. `None` skips the hourly check.
    #[serde(default)]
    pub hourly_limit_usd: Option<f64>,
    #[serde(default)]
    pub action_on_exceed: BudgetAction,
}

/// `[budget.deployment]` block.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeploymentBudget {
    #[serde(default)]
    pub daily_limit_usd: Option<f64>,
    #[serde(default)]
    pub hourly_limit_usd: Option<f64>,
    #[serde(default)]
    pub action_on_exceed: BudgetAction,
}

/// Top-level `[budget]` controller config.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BudgetConfig {
    #[serde(default)]
    pub agents: Vec<AgentBudget>,
    #[serde(default)]
    pub deployment: Option<DeploymentBudget>,
    /// How long the bridge sleeps when an action == `throttle`. Default 2000ms.
    #[serde(default = "default_throttle_backoff_ms")]
    pub throttle_backoff_ms: u64,
    /// How often the in-memory accumulated-cost cache is refreshed from
    /// the metrics store. Default 60s.
    #[serde(default = "default_cache_refresh_secs")]
    pub cache_refresh_secs: u64,
    /// Methods exempt from budget enforcement — useful for internal
    /// bookkeeping calls (`metrics.*`, `budget.*`) that operators don't
    /// want to count against the agent's quota when measuring the cap.
    /// Exact-match; case-sensitive.
    #[serde(default)]
    pub exempt_methods: Vec<String>,
}

fn default_throttle_backoff_ms() -> u64 {
    2000
}

fn default_cache_refresh_secs() -> u64 {
    60
}

impl BudgetConfig {
    /// True iff at least one agent OR a deployment cap is configured.
    /// When false the bridge skips the enforcer entirely.
    pub fn is_active(&self) -> bool {
        !self.agents.is_empty()
            || self
                .deployment
                .as_ref()
                .is_some_and(|d| d.daily_limit_usd.is_some() || d.hourly_limit_usd.is_some())
    }
}

/// Cached accumulated cost for one (scope, window) pair, plus the wall-clock
/// time it was refreshed at. `scope = "agent:<name>"` or `"deployment"`.
#[derive(Clone, Copy, Debug, Default)]
struct CacheEntry {
    /// Cost in micro-USD accumulated since `window_start_ms`.
    cost_micros: u64,
    /// Inclusive lower bound of the window, in unix-ms.
    window_start_ms: i64,
    /// When this entry was last refreshed from the store, in unix-ms.
    refreshed_at_ms: i64,
    /// Inclusive upper bound of the window (when the window resets).
    window_end_ms: i64,
}

/// Window granularity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Window {
    Daily,
    Hourly,
}

impl Window {
    pub fn as_str(self) -> &'static str {
        match self {
            Window::Daily => "daily",
            Window::Hourly => "hourly",
        }
    }

    fn duration_ms(self) -> i64 {
        match self {
            Window::Daily => 86_400_000,
            Window::Hourly => 3_600_000,
        }
    }

    fn window_bounds_ms(self) -> (i64, i64) {
        let now = now_ms();
        let dur = self.duration_ms();
        let start = (now / dur) * dur;
        (start, start + dur)
    }
}

type AgentMap = HashMap<String, AgentBudget>;

/// Budget enforcement runtime object. Cheap to clone — the cache lives
/// inside `Arc<RwLock<...>>` so the hot path stays read-side most of the
/// time.
#[derive(Clone)]
pub struct BudgetEnforcer {
    inner: Arc<BudgetInner>,
}

struct BudgetInner {
    query: Option<MetricsQuery>,
    agents: RwLock<AgentMap>,
    deployment: RwLock<Option<DeploymentBudget>>,
    throttle_backoff: Duration,
    cache_refresh: Duration,
    exempt_methods: Vec<String>,
    cache: Mutex<HashMap<CacheKey, CacheEntry>>,
    /// Optional alert delivery sink (composite of chronicle + multi-channel).
    /// `None` keeps `alert_only` behaviour silent at the alert pipeline —
    /// the call still passes through.
    sink: RwLock<Option<Arc<dyn AlertDeliver>>>,
    /// Per-(agent, window) firing dedup state. The enforcer fires a fresh
    /// `BudgetExceeded` alert when a key crosses from healthy → over, and a
    /// recovery event when it drops back. Without this, every throttled
    /// call would refire the alert.
    active: Mutex<HashMap<CacheKey, ActiveAlert>>,
    /// CORR-D2: async refresh serialisation. The pre-fix
    /// `std::sync::Mutex<()>` here was load-bearing equivalent
    /// for fully-sync callers, but `evaluate_agent` /
    /// `evaluate_deployment` / `check` / `status` are now
    /// `async fn` and would have to hold a sync MutexGuard
    /// across the SQLite read — a deadlock hazard on a
    /// single-threaded executor. The tokio mutex's guard
    /// can be held across `.await` points safely; it also
    /// prevents the lock-order inversion the std version
    /// risked when two refresh paths blocked each other on
    /// the runtime worker pool.
    refresh_mutex: tokio::sync::Mutex<()>,
}

/// `(scope, window)` cache + active-alert key. `scope = "agent:<name>"`
/// or `"deployment"`.
type CacheKey = (String, Window);

impl BudgetEnforcer {
    /// Construct from config. `query` is the read engine the enforcer uses
    /// to refresh accumulated cost from SQLite — pass `None` only in tests
    /// that pre-seed the cache directly via [`Self::set_cached_for_test`].
    pub fn new(cfg: BudgetConfig, query: Option<MetricsQuery>) -> Self {
        let mut agents = HashMap::new();
        for row in cfg.agents {
            agents.insert(row.agent.clone(), row);
        }
        Self {
            inner: Arc::new(BudgetInner {
                query,
                agents: RwLock::new(agents),
                deployment: RwLock::new(cfg.deployment),
                throttle_backoff: Duration::from_millis(cfg.throttle_backoff_ms),
                cache_refresh: Duration::from_secs(cfg.cache_refresh_secs.max(1)),
                exempt_methods: cfg.exempt_methods,
                cache: Mutex::new(HashMap::new()),
                sink: RwLock::new(None),
                active: Mutex::new(HashMap::new()),
                refresh_mutex: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Wire (or rewire) the alert sink. Idempotent.
    pub fn set_alert_sink(&self, sink: Arc<dyn AlertDeliver>) {
        let mut g = self.inner.sink.write().expect("budget sink write");
        *g = Some(sink);
    }

    /// True iff the enforcer has at least one limit configured. The
    /// dispatch bridge short-circuits without consulting `check` when this
    /// returns false.
    pub fn is_active(&self) -> bool {
        let agents_empty = self
            .inner
            .agents
            .read()
            .map(|g| g.is_empty())
            .unwrap_or(true);
        let deployment_active = self
            .inner
            .deployment
            .read()
            .ok()
            .and_then(|g| {
                g.as_ref()
                    .map(|d| d.daily_limit_usd.is_some() || d.hourly_limit_usd.is_some())
            })
            .unwrap_or(false);
        !agents_empty || deployment_active
    }

    /// Configured throttle backoff.
    pub fn throttle_backoff(&self) -> Duration {
        self.inner.throttle_backoff
    }

    /// Run the pre-dispatch budget check. The caller passes the verified
    /// agent name and the capability method.
    ///
    /// The first matching `Reject` action wins. A `Throttle` from the agent
    /// cap suppresses the deployment cap's evaluation (the call is already
    /// going to wait before dispatch). An `AlertOnly` action never blocks —
    /// the enforcer fires the alert as a side effect and returns
    /// [`BudgetDecision::Allow`].
    pub async fn check(&self, agent: &str, method: &str) -> BudgetDecision {
        if self.inner.exempt_methods.iter().any(|m| m == method) {
            return BudgetDecision::Allow;
        }
        if let Some(d) = self.evaluate_agent(agent).await {
            // Reject + Throttle short-circuit. AlertOnly falls through
            // so the deployment cap can still fire if applicable.
            match &d {
                BudgetDecision::Reject { .. } | BudgetDecision::Throttle { .. } => return d,
                BudgetDecision::Allow => {}
            }
        }
        if let Some(d) = self.evaluate_deployment().await {
            return d;
        }
        BudgetDecision::Allow
    }

    /// Snapshot of every configured agent's current spend + limits.
    /// Used by `budget.status`.
    pub async fn status(&self) -> BudgetStatus {
        let mut rows = Vec::new();
        // CORR-D2: scope the std RwLock guard so it is dropped
        // before the first `.await`. The compiler's
        // Send-future analysis would otherwise reject the
        // async fn even with an explicit `drop(...)` call.
        let agent_names: Vec<String> = {
            let agents_map = self.inner.agents.read().expect("budget agents read");
            agents_map.keys().cloned().collect()
        };
        for name in agent_names {
            let daily = self
                .refresh_window(Some(name.as_str()), Window::Daily)
                .await;
            let hourly = self
                .refresh_window(Some(name.as_str()), Window::Hourly)
                .await;
            let row = self.build_agent_row(&name, daily, hourly);
            rows.push(row);
        }
        let deployment_daily = self.refresh_window(None, Window::Daily).await;
        let deployment_hourly = self.refresh_window(None, Window::Hourly).await;
        let deployment = self.build_deployment_row(deployment_daily, deployment_hourly);
        BudgetStatus {
            agents: rows,
            deployment,
        }
    }

    /// Reset the cache for one (agent, window) — exposed via `budget.reset`
    /// for incident recovery + tests. Resetting the cache forces the next
    /// `check` to re-read from the metrics store. Also clears any active
    /// `BudgetExceeded` alert dedup state for that key so a subsequent
    /// breach fires a fresh event.
    pub fn reset(&self, agent: Option<&str>, window: Window) {
        let key = match agent {
            Some(a) => (format!("agent:{a}"), window),
            None => ("deployment".to_string(), window),
        };
        if let Ok(mut g) = self.inner.cache.lock() {
            g.remove(&key);
        }
        if let Ok(mut g) = self.inner.active.lock() {
            g.remove(&key);
        }
    }

    /// Force-invalidate every cached window for `agent`. Called by the
    /// metrics collector after a `cost > 0` row lands so the next check
    /// reflects the new spend within microseconds rather than waiting for
    /// the next refresh tick.
    pub fn invalidate_agent(&self, agent: &str) {
        if let Ok(mut g) = self.inner.cache.lock() {
            g.remove(&(format!("agent:{agent}"), Window::Daily));
            g.remove(&(format!("agent:{agent}"), Window::Hourly));
            g.remove(&("deployment".to_string(), Window::Daily));
            g.remove(&("deployment".to_string(), Window::Hourly));
        }
    }

    /// Test seam — pre-populate the cache. Used by unit tests that don't
    /// want to stand up a full MetricsStore.
    #[doc(hidden)]
    pub fn set_cached_for_test(&self, scope: &str, window: Window, cost_micros: u64) {
        let (start, end) = window.window_bounds_ms();
        let entry = CacheEntry {
            cost_micros,
            window_start_ms: start,
            refreshed_at_ms: now_ms(),
            window_end_ms: end,
        };
        let mut g = self.inner.cache.lock().expect("cache lock");
        g.insert((scope.to_string(), window), entry);
    }

    async fn evaluate_agent(&self, agent: &str) -> Option<BudgetDecision> {
        let cfg = {
            let g = self.inner.agents.read().expect("budget agents read");
            g.get(agent).cloned()
        };
        let cfg = cfg?;
        let daily = self.refresh_window(Some(agent), Window::Daily).await;
        let hourly = self.refresh_window(Some(agent), Window::Hourly).await;
        // Daily first — it's the harder ceiling.
        if let Some(limit_usd) = cfg.daily_limit_usd
            && let Some(decision) = self.compare(
                Some(agent),
                Window::Daily,
                "agent",
                limit_usd,
                cfg.action_on_exceed,
                daily,
            )
        {
            return Some(decision);
        }
        if let Some(limit_usd) = cfg.hourly_limit_usd
            && let Some(decision) = self.compare(
                Some(agent),
                Window::Hourly,
                "agent",
                limit_usd,
                cfg.action_on_exceed,
                hourly,
            )
        {
            return Some(decision);
        }
        Some(BudgetDecision::Allow)
    }

    async fn evaluate_deployment(&self) -> Option<BudgetDecision> {
        let cfg = {
            let g = self
                .inner
                .deployment
                .read()
                .expect("budget deployment read");
            g.clone()
        };
        let cfg = cfg?;
        let daily = self.refresh_window(None, Window::Daily).await;
        let hourly = self.refresh_window(None, Window::Hourly).await;
        if let Some(limit_usd) = cfg.daily_limit_usd
            && let Some(decision) = self.compare(
                None,
                Window::Daily,
                "deployment",
                limit_usd,
                cfg.action_on_exceed,
                daily,
            )
        {
            return Some(decision);
        }
        if let Some(limit_usd) = cfg.hourly_limit_usd
            && let Some(decision) = self.compare(
                None,
                Window::Hourly,
                "deployment",
                limit_usd,
                cfg.action_on_exceed,
                hourly,
            )
        {
            return Some(decision);
        }
        Some(BudgetDecision::Allow)
    }

    fn compare(
        &self,
        agent: Option<&str>,
        window: Window,
        scope: &'static str,
        limit_usd: f64,
        action: BudgetAction,
        entry: CacheEntry,
    ) -> Option<BudgetDecision> {
        // SEC PART 6: an `f64::NAN` or `f64::INFINITY` cast
        // straight to u64 is undefined behaviour in older
        // compiler versions and silently produces 0 / u64::MAX
        // in current ones — either way the operator loses
        // their cap. FAIL CLOSED: synthesise a Reject decision
        // with an InvalidLimit cause so the call doesn't slip
        // through unlimited and the bad-config error is
        // visible to the caller.
        if limit_usd.is_nan() || limit_usd.is_infinite() {
            tracing::warn!(
                ?agent,
                limit_usd,
                "budget enforcer: NaN/inf limit — rejecting all calls in this window",
            );
            let breach = BudgetBreach {
                agent: agent.unwrap_or("").to_string(),
                window: window.as_str().to_string(),
                scope: scope.to_string(),
                limit_micros: 0,
                actual_micros: entry.cost_micros,
                resets_at_ms: entry.window_end_ms,
                cause: format!(
                    "budget: invalid limit for {scope}{label} {window_lbl} window: \
                     {limit_usd:?} (NaN or infinite — operator must set a finite USD value)",
                    label = agent.map(|a| format!(" {a}")).unwrap_or_default(),
                    window_lbl = window.as_str(),
                ),
            };
            return Some(BudgetDecision::Reject { info: breach });
        }
        let limit_micros_f = (limit_usd * 1_000_000.0).max(0.0);
        // Saturate the post-multiply value via try_from to
        // avoid wrap-around when limit_usd is implausibly
        // large (e.g. f64::MAX → multiply overflows the i64
        // mantissa).
        let limit_micros = if limit_micros_f >= u64::MAX as f64 {
            u64::MAX
        } else {
            limit_micros_f as u64
        };
        let crossed = entry.cost_micros >= limit_micros && limit_micros > 0;
        let key = match agent {
            Some(a) => (format!("agent:{a}"), window),
            None => ("deployment".to_string(), window),
        };
        if !crossed {
            // Healthy: emit a recovery if previously active.
            self.maybe_emit_recovery(&key, &entry);
            return None;
        }
        let breach = BudgetBreach {
            agent: agent.unwrap_or("").to_string(),
            window: window.as_str().to_string(),
            scope: scope.to_string(),
            limit_micros,
            actual_micros: entry.cost_micros,
            resets_at_ms: entry.window_end_ms,
            cause: format!(
                "budget exceeded: {scope}{label} {window_lbl} limit ${limit:.4} reached \
                 (actual ${actual:.4}; window resets at unix-ms {reset})",
                label = agent.map(|a| format!(" {a}")).unwrap_or_default(),
                window_lbl = window.as_str(),
                limit = limit_usd,
                actual = entry.cost_micros as f64 / 1_000_000.0,
                reset = entry.window_end_ms,
            ),
        };
        // Fire-edge alert emission — only on the transition from healthy
        // → over for this (key) dedup pair.
        self.maybe_emit_fire(&key, &breach);
        let decision = match action {
            BudgetAction::Reject => BudgetDecision::Reject { info: breach },
            BudgetAction::Throttle => BudgetDecision::Throttle {
                delay: self.inner.throttle_backoff,
                info: breach,
            },
            BudgetAction::AlertOnly => BudgetDecision::Allow,
        };
        Some(decision)
    }

    fn maybe_emit_fire(&self, key: &CacheKey, breach: &BudgetBreach) {
        let mut active = match self.inner.active.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if active.contains_key(key) {
            return;
        }
        let alert = ActiveAlert {
            agent: breach.agent.clone(),
            kind: AlertKind::BudgetExceeded,
            severity: AlertSeverity::Critical,
            triggered_at_ms: now_ms(),
            threshold: breach.limit_micros as f64,
            actual: breach.actual_micros as f64,
            message: breach.alert_message(),
            method: Some(format!("budget:{}:{}", breach.scope, breach.window)),
        };
        active.insert(key.clone(), alert.clone());
        drop(active);
        let sink = self.inner.sink.read().ok().and_then(|g| g.clone());
        if let Some(s) = sink {
            s.deliver(&AlertEvent::Fired(alert));
        }
    }

    fn maybe_emit_recovery(&self, key: &CacheKey, entry: &CacheEntry) {
        let mut active = match self.inner.active.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let Some(prior) = active.remove(key) else {
            return;
        };
        drop(active);
        let recovered = ActiveAlert {
            actual: entry.cost_micros as f64,
            ..prior
        };
        let sink = self.inner.sink.read().ok().and_then(|g| g.clone());
        if let Some(s) = sink {
            s.deliver(&AlertEvent::Recovered(recovered));
        }
    }

    async fn refresh_window(&self, agent: Option<&str>, window: Window) -> CacheEntry {
        let scope_key = match agent {
            Some(a) => format!("agent:{a}"),
            None => "deployment".to_string(),
        };
        let key = (scope_key.clone(), window);
        let now = now_ms();
        let (window_start, window_end) = window.window_bounds_ms();
        // SEC PART 6: `as_millis()` returns u128. Saturate to
        // i64::MAX via try_from so an operator-configured
        // `Duration::MAX` cache_refresh doesn't wrap to a
        // negative value and short-circuit the freshness check.
        let cache_refresh_ms =
            i64::try_from(self.inner.cache_refresh.as_millis()).unwrap_or(i64::MAX);
        // Fast path — fresh cache, same window. The cache
        // mutex is std::sync::Mutex<HashMap>; we lock it,
        // copy out the entry, and DROP the guard BEFORE any
        // `.await` so a sync MutexGuard never crosses an
        // await point.
        {
            if let Ok(g) = self.inner.cache.lock()
                && let Some(entry) = g.get(&key)
                && entry.window_start_ms == window_start
                && (now - entry.refreshed_at_ms) < cache_refresh_ms
            {
                return *entry;
            }
        }
        // CORR-D2: serialise the refresh path through the
        // tokio mutex. The guard is held across the SQLite
        // read below; the std cache mutex is acquired only
        // for the milliseconds needed to read or insert and
        // never held across an `.await`.
        let _refresh_guard = self.inner.refresh_mutex.lock().await;
        // Double-checked: another caller may have raced ahead
        // of us and landed a fresh entry while we were waiting.
        {
            if let Ok(g) = self.inner.cache.lock()
                && let Some(entry) = g.get(&key)
                && entry.window_start_ms == window_start
                && (now - entry.refreshed_at_ms) < cache_refresh_ms
            {
                return *entry;
            }
        }
        let cost_micros = self.read_cost(agent, window_start);
        let entry = CacheEntry {
            cost_micros,
            window_start_ms: window_start,
            refreshed_at_ms: now,
            window_end_ms: window_end,
        };
        if let Ok(mut g) = self.inner.cache.lock() {
            g.insert(key, entry);
        }
        entry
    }

    fn read_cost(&self, agent: Option<&str>, since_ms: i64) -> u64 {
        let Some(query) = self.inner.query.as_ref() else {
            return 0;
        };
        match query.cost_since(agent, since_ms) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "budget enforcer: cost_since query failed; treating window as zero"
                );
                0
            }
        }
    }

    fn build_agent_row(&self, name: &str, daily: CacheEntry, hourly: CacheEntry) -> AgentStatusRow {
        let cfg = {
            let g = self.inner.agents.read().expect("budget agents read");
            g.get(name).cloned()
        };
        AgentStatusRow {
            agent: name.to_string(),
            daily_limit_micros: cfg
                .as_ref()
                .and_then(|c| c.daily_limit_usd)
                .map(|v| (v * 1_000_000.0) as u64),
            daily_actual_micros: daily.cost_micros,
            daily_resets_at_ms: daily.window_end_ms,
            hourly_limit_micros: cfg
                .as_ref()
                .and_then(|c| c.hourly_limit_usd)
                .map(|v| (v * 1_000_000.0) as u64),
            hourly_actual_micros: hourly.cost_micros,
            hourly_resets_at_ms: hourly.window_end_ms,
            action: cfg
                .map(|c| c.action_on_exceed.as_str().to_string())
                .unwrap_or_else(|| BudgetAction::default().as_str().to_string()),
        }
    }

    fn build_deployment_row(
        &self,
        daily: CacheEntry,
        hourly: CacheEntry,
    ) -> Option<DeploymentStatusRow> {
        let cfg = {
            let g = self
                .inner
                .deployment
                .read()
                .expect("budget deployment read");
            g.clone()
        }?;
        Some(DeploymentStatusRow {
            daily_limit_micros: cfg.daily_limit_usd.map(|v| (v * 1_000_000.0) as u64),
            daily_actual_micros: daily.cost_micros,
            daily_resets_at_ms: daily.window_end_ms,
            hourly_limit_micros: cfg.hourly_limit_usd.map(|v| (v * 1_000_000.0) as u64),
            hourly_actual_micros: hourly.cost_micros,
            hourly_resets_at_ms: hourly.window_end_ms,
            action: cfg.action_on_exceed.as_str().to_string(),
        })
    }
}

/// Wire-format `budget.status` response.
#[derive(Clone, Debug, Serialize)]
pub struct BudgetStatus {
    pub agents: Vec<AgentStatusRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<DeploymentStatusRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentStatusRow {
    pub agent: String,
    pub daily_limit_micros: Option<u64>,
    pub daily_actual_micros: u64,
    pub daily_resets_at_ms: i64,
    pub hourly_limit_micros: Option<u64>,
    pub hourly_actual_micros: u64,
    pub hourly_resets_at_ms: i64,
    pub action: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeploymentStatusRow {
    pub daily_limit_micros: Option<u64>,
    pub daily_actual_micros: u64,
    pub daily_resets_at_ms: i64,
    pub hourly_limit_micros: Option<u64>,
    pub hourly_actual_micros: u64,
    pub hourly_resets_at_ms: i64,
    pub action: String,
}

/// Window parsed from the `budget.reset` request.
pub fn parse_window(s: &str) -> Option<Window> {
    match s.trim().to_ascii_lowercase().as_str() {
        "daily" | "day" => Some(Window::Daily),
        "hourly" | "hour" => Some(Window::Hourly),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::store::MetricsStore;
    use crate::metrics::types::InvocationMetric;

    fn metric(agent: &str, ts_ms: i64, cost: u64) -> InvocationMetric {
        InvocationMetric {
            agent_name: agent.into(),
            tenant_id: "default".into(),
            peer_alias: "p".into(),
            method: "ai.chat".into(),
            timestamp_ms: ts_ms,
            latency_ms: 10,
            success: true,
            error_kind: None,
            token_count: Some(100),
            cost_micros: Some(cost),
            input_bytes: 10,
            output_bytes: 20,
            model: Some("gpt-4o-mini".into()),
            confidence_score: None,
            routing_tier: None,
            request_id: None,
        }
    }

    fn enforcer_with_agent(
        agent: &str,
        daily_usd: Option<f64>,
        hourly_usd: Option<f64>,
        action: BudgetAction,
    ) -> BudgetEnforcer {
        let store = MetricsStore::in_memory().unwrap();
        let q = MetricsQuery::new(store);
        BudgetEnforcer::new(
            BudgetConfig {
                agents: vec![AgentBudget {
                    agent: agent.into(),
                    daily_limit_usd: daily_usd,
                    hourly_limit_usd: hourly_usd,
                    action_on_exceed: action,
                }],
                deployment: None,
                throttle_backoff_ms: 2000,
                cache_refresh_secs: 60,
                exempt_methods: vec![],
            },
            Some(q),
        )
    }

    #[tokio::test]
    async fn allow_when_under_limit() {
        let enf = enforcer_with_agent("alice", Some(1.0), None, BudgetAction::Reject);
        enf.set_cached_for_test("agent:alice", Window::Daily, 100_000); // $0.10
        match enf.check("alice", "ai.chat").await {
            BudgetDecision::Allow => {}
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reject_when_daily_limit_exceeded() {
        let enf = enforcer_with_agent("alice", Some(1.0), None, BudgetAction::Reject);
        enf.set_cached_for_test("agent:alice", Window::Daily, 2_000_000); // $2.00
        match enf.check("alice", "ai.chat").await {
            BudgetDecision::Reject { info } => {
                assert_eq!(info.agent, "alice");
                assert_eq!(info.window, "daily");
                assert_eq!(info.limit_micros, 1_000_000);
                assert!(info.cause.contains("daily"));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn throttle_when_hourly_limit_exceeded() {
        let enf = enforcer_with_agent("alice", None, Some(0.5), BudgetAction::Throttle);
        enf.set_cached_for_test("agent:alice", Window::Hourly, 600_000); // $0.60
        match enf.check("alice", "ai.chat").await {
            BudgetDecision::Throttle { delay, info } => {
                assert_eq!(delay, Duration::from_millis(2000));
                assert_eq!(info.window, "hourly");
            }
            other => panic!("expected Throttle, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn alert_only_allows_call_through() {
        let enf = enforcer_with_agent("alice", Some(1.0), None, BudgetAction::AlertOnly);
        enf.set_cached_for_test("agent:alice", Window::Daily, 5_000_000); // $5.00
        match enf.check("alice", "ai.chat").await {
            BudgetDecision::Allow => {}
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exempt_method_skips_enforcement() {
        let enf = BudgetEnforcer::new(
            BudgetConfig {
                agents: vec![AgentBudget {
                    agent: "alice".into(),
                    daily_limit_usd: Some(0.001),
                    hourly_limit_usd: None,
                    action_on_exceed: BudgetAction::Reject,
                }],
                deployment: None,
                throttle_backoff_ms: 2000,
                cache_refresh_secs: 60,
                exempt_methods: vec!["budget.status".into()],
            },
            Some(MetricsQuery::new(MetricsStore::in_memory().unwrap())),
        );
        enf.set_cached_for_test("agent:alice", Window::Daily, 10_000_000);
        // Non-exempt method rejects.
        assert!(matches!(
            enf.check("alice", "ai.chat").await,
            BudgetDecision::Reject { .. }
        ));
        // Exempt method passes.
        assert!(matches!(
            enf.check("alice", "budget.status").await,
            BudgetDecision::Allow
        ));
    }

    #[tokio::test]
    async fn deployment_cap_triggers_when_total_spend_exceeds_limit() {
        let store = MetricsStore::in_memory().unwrap();
        let now = now_ms();
        store.insert(&metric("alice", now, 4_000_000)).unwrap();
        store.insert(&metric("bob", now, 7_000_000)).unwrap();
        let q = MetricsQuery::new(store);
        let enf = BudgetEnforcer::new(
            BudgetConfig {
                agents: vec![],
                deployment: Some(DeploymentBudget {
                    daily_limit_usd: Some(10.0),
                    hourly_limit_usd: None,
                    action_on_exceed: BudgetAction::Reject,
                }),
                throttle_backoff_ms: 2000,
                cache_refresh_secs: 60,
                exempt_methods: vec![],
            },
            Some(q),
        );
        match enf.check("alice", "ai.chat").await {
            BudgetDecision::Reject { info } => {
                assert_eq!(info.scope, "deployment");
            }
            other => panic!("expected deployment Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cache_is_invalidated_immediately() {
        let enf = enforcer_with_agent("alice", Some(1.0), None, BudgetAction::Reject);
        enf.set_cached_for_test("agent:alice", Window::Daily, 100_000); // healthy
        assert!(matches!(
            enf.check("alice", "ai.chat").await,
            BudgetDecision::Allow
        ));
        // Now invalidate and re-seed over the limit.
        enf.invalidate_agent("alice");
        enf.set_cached_for_test("agent:alice", Window::Daily, 2_000_000);
        assert!(matches!(
            enf.check("alice", "ai.chat").await,
            BudgetDecision::Reject { .. }
        ));
    }

    #[tokio::test]
    async fn budget_status_returns_configured_agents_and_actuals() {
        let enf = enforcer_with_agent("alice", Some(2.0), Some(0.5), BudgetAction::Throttle);
        enf.set_cached_for_test("agent:alice", Window::Daily, 250_000);
        enf.set_cached_for_test("agent:alice", Window::Hourly, 100_000);
        let status = enf.status().await;
        assert_eq!(status.agents.len(), 1);
        let row = &status.agents[0];
        assert_eq!(row.agent, "alice");
        assert_eq!(row.daily_actual_micros, 250_000);
        assert_eq!(row.hourly_actual_micros, 100_000);
        assert_eq!(row.daily_limit_micros, Some(2_000_000));
        assert_eq!(row.hourly_limit_micros, Some(500_000));
        assert_eq!(row.action, "throttle");
    }

    #[tokio::test]
    async fn reset_clears_cache_for_specified_window() {
        let enf = enforcer_with_agent("alice", Some(1.0), None, BudgetAction::Reject);
        enf.set_cached_for_test("agent:alice", Window::Daily, 2_000_000);
        // Confirm the over-limit cache made the check reject.
        assert!(matches!(
            enf.check("alice", "ai.chat").await,
            BudgetDecision::Reject { .. }
        ));
        enf.reset(Some("alice"), Window::Daily);
        // Reset clears the cache → next check re-reads from the (empty)
        // store, which returns 0 → call allowed.
        match enf.check("alice", "ai.chat").await {
            BudgetDecision::Allow => {}
            other => panic!("expected Allow after reset, got {other:?}"),
        }
    }

    /// Verify that BudgetExceeded fires through the configured AlertSink
    /// when the agent crosses the threshold, and exactly once (no re-fire
    /// on the second check within the same window).
    #[tokio::test]
    async fn alert_fires_once_per_breach_then_dedups() {
        use crate::metrics::alert::LoggingAlertSink;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingSink(Arc<AtomicUsize>, Arc<AtomicUsize>);
        impl AlertDeliver for CountingSink {
            fn deliver(&self, e: &AlertEvent) {
                match e {
                    AlertEvent::Fired(a) => {
                        if a.kind == AlertKind::BudgetExceeded {
                            self.0.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    AlertEvent::Recovered(a) => {
                        if a.kind == AlertKind::BudgetExceeded {
                            self.1.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            }
        }

        let fires = Arc::new(AtomicUsize::new(0));
        let recovers = Arc::new(AtomicUsize::new(0));
        let sink: Arc<dyn AlertDeliver> = Arc::new(CountingSink(fires.clone(), recovers.clone()));
        let _ = LoggingAlertSink; // dead silence on the warn channel
        let enf = enforcer_with_agent("alice", Some(1.0), None, BudgetAction::AlertOnly);
        enf.set_alert_sink(sink);
        enf.set_cached_for_test("agent:alice", Window::Daily, 2_000_000);
        let _ = enf.check("alice", "ai.chat").await;
        let _ = enf.check("alice", "ai.chat").await;
        assert_eq!(
            fires.load(Ordering::SeqCst),
            1,
            "expected exactly one Fired event"
        );
        // Reset cache to a healthy value and verify the recovery event
        // fires exactly once.
        enf.set_cached_for_test("agent:alice", Window::Daily, 0);
        let _ = enf.check("alice", "ai.chat").await;
        assert_eq!(
            recovers.load(Ordering::SeqCst),
            1,
            "expected exactly one Recovered event"
        );
    }

    // ── CORR-D2: concurrent refresh under tokio mutex ────

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn corr_d2_concurrent_check_calls_do_not_deadlock() {
        // 16 concurrent `check` calls against the same agent
        // exercise the tokio refresh mutex from multiple
        // worker threads. Pre-fix std::sync::Mutex would have
        // been held across the SQLite read on a single-
        // threaded executor and risked deadlock when nested
        // calls overlapped; the tokio mutex's guard is
        // await-safe so this completes without contention.
        let enf = std::sync::Arc::new(enforcer_with_agent(
            "alice",
            Some(1.0),
            None,
            BudgetAction::AlertOnly,
        ));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let e = enf.clone();
            handles.push(tokio::spawn(
                async move { e.check("alice", "ai.chat").await },
            ));
        }
        for h in handles {
            let _ = h.await.expect("task panicked");
        }
        // The metrics store was never populated, so every
        // refresh returns 0 micro-USD and every decision is
        // Allow. The point of the test is the absence of
        // deadlock + the consistent cache state across
        // concurrent refreshers.
        let status = enf.status().await;
        assert_eq!(status.agents.len(), 1);
        assert_eq!(status.agents[0].daily_actual_micros, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn corr_d2_two_concurrent_refresh_calls_produce_consistent_state() {
        // Seed the cache to a known healthy value, then fire
        // two concurrent `status` calls. The tokio refresh
        // mutex serialises the writers; both observers must
        // see the same accumulated cost back from `status`
        // and the cache must contain exactly one entry per
        // (scope, window) afterwards (i.e. duplicate inserts
        // do not happen).
        let enf = std::sync::Arc::new(enforcer_with_agent(
            "alice",
            Some(1.0),
            None,
            BudgetAction::Reject,
        ));
        enf.set_cached_for_test("agent:alice", Window::Daily, 250_000);
        let a = enf.clone();
        let b = enf.clone();
        let (sa, sb) = tokio::join!(
            tokio::spawn(async move { a.status().await }),
            tokio::spawn(async move { b.status().await }),
        );
        let sa = sa.expect("task a panicked");
        let sb = sb.expect("task b panicked");
        assert_eq!(sa.agents.len(), 1);
        assert_eq!(sb.agents.len(), 1);
        assert_eq!(
            sa.agents[0].daily_actual_micros,
            sb.agents[0].daily_actual_micros
        );
    }

    #[test]
    fn parses_budget_action_from_strings() {
        assert_eq!(
            BudgetAction::parse("throttle"),
            Some(BudgetAction::Throttle)
        );
        assert_eq!(BudgetAction::parse("REJECT"), Some(BudgetAction::Reject));
        assert_eq!(
            BudgetAction::parse("alert_only"),
            Some(BudgetAction::AlertOnly)
        );
        assert_eq!(
            BudgetAction::parse("alert-only"),
            Some(BudgetAction::AlertOnly)
        );
        assert_eq!(BudgetAction::parse("nope"), None);
    }

    #[tokio::test]
    async fn nan_limit_rejects_calls_instead_of_silently_unlimited() {
        // SEC PART 6: an operator who fat-fingers a TOML
        // limit to NaN (e.g. `daily_limit_usd = nan` from a
        // templater) should NOT get an unlimited budget. The
        // call must fail closed with a clear cause.
        let enf = enforcer_with_agent("alice", Some(f64::NAN), None, BudgetAction::Reject);
        enf.set_cached_for_test("agent:alice", Window::Daily, 100);
        match enf.check("alice", "ai.chat").await {
            BudgetDecision::Reject { info } => {
                assert!(
                    info.cause.contains("NaN") || info.cause.contains("infinite"),
                    "expected NaN/infinite mention, got: {}",
                    info.cause
                );
            }
            other => panic!("expected Reject for NaN limit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn infinite_limit_rejects_calls_instead_of_silently_unlimited() {
        let enf = enforcer_with_agent("alice", Some(f64::INFINITY), None, BudgetAction::Reject);
        enf.set_cached_for_test("agent:alice", Window::Daily, 100);
        assert!(matches!(
            enf.check("alice", "ai.chat").await,
            BudgetDecision::Reject { .. }
        ));
    }

    #[test]
    fn config_is_active_only_when_caps_exist() {
        let empty = BudgetConfig::default();
        assert!(!empty.is_active());
        let with_agent = BudgetConfig {
            agents: vec![AgentBudget {
                agent: "alice".into(),
                daily_limit_usd: Some(1.0),
                hourly_limit_usd: None,
                action_on_exceed: BudgetAction::Reject,
            }],
            ..Default::default()
        };
        assert!(with_agent.is_active());
        let with_deployment = BudgetConfig {
            deployment: Some(DeploymentBudget {
                daily_limit_usd: Some(50.0),
                hourly_limit_usd: None,
                action_on_exceed: BudgetAction::AlertOnly,
            }),
            ..Default::default()
        };
        assert!(with_deployment.is_active());
    }

    #[test]
    fn parses_budget_config_from_toml() {
        let text = r#"
            throttle_backoff_ms = 1500
            cache_refresh_secs = 30

            [[agents]]
            agent = "research-agent"
            daily_limit_usd = 5.0
            hourly_limit_usd = 1.0
            action_on_exceed = "throttle"

            [[agents]]
            agent = "code-agent"
            daily_limit_usd = 10.0
            action_on_exceed = "reject"

            [deployment]
            daily_limit_usd = 50.0
            action_on_exceed = "alert_only"
        "#;
        let cfg: BudgetConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.agents.len(), 2);
        assert_eq!(cfg.agents[0].agent, "research-agent");
        assert_eq!(cfg.agents[0].daily_limit_usd, Some(5.0));
        assert_eq!(cfg.agents[0].action_on_exceed, BudgetAction::Throttle);
        assert_eq!(cfg.agents[1].action_on_exceed, BudgetAction::Reject);
        let dep = cfg.deployment.unwrap();
        assert_eq!(dep.daily_limit_usd, Some(50.0));
        assert_eq!(dep.action_on_exceed, BudgetAction::AlertOnly);
        assert_eq!(cfg.throttle_backoff_ms, 1500);
        assert_eq!(cfg.cache_refresh_secs, 30);
    }
}
