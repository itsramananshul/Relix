//! AI node — registers the `ai.chat` capability with a provider-agnostic
//! backend.
//!
//! Provider selection is in config; the SOL flow never changes. See
//! `provider/mod.rs` for the [`ChatProvider`] trait and the per-backend
//! implementations.
//!
//! ## Wire format (SIMP-016 alpha)
//!
//! Arg:    `session_id|prompt|history`   (UTF-8; pipe-delimited; history may be empty)
//! Return: provider's reply text         (UTF-8)
//!
//! ## Config
//!
//! ```toml
//! [controller]
//! node_type = "ai"
//!
//! [ai]
//! # Active provider: `mock` | `openai` | `openrouter` | `xai` | `local`
//! #                | `anthropic` | `gemini`
//! provider = "mock"
//! # Optional default model id. ChatInput.model overrides; empty means
//! # provider-side default.
//! model = ""
//!
//! [ai.providers.openai]
//! base_url     = "https://api.openai.com/v1"
//! api_key_env  = "OPENAI_API_KEY"
//! default_model = "gpt-4o-mini"
//!
//! [ai.providers.openrouter]
//! base_url     = "https://openrouter.ai/api/v1"
//! api_key_env  = "OPENROUTER_API_KEY"
//!
//! [ai.providers.xai]
//! base_url     = "https://api.x.ai/v1"
//! api_key_env  = "XAI_API_KEY"
//!
//! [ai.providers.local]
//! base_url     = "http://localhost:11434/v1"
//! # api_key_env unset or empty == no auth (Ollama-style local server).
//!
//! [ai.providers.anthropic]
//! api_key_env  = "ANTHROPIC_API_KEY"
//! default_model = "claude-3-5-sonnet-latest"
//!
//! [ai.providers.gemini]
//! api_key_env  = "GEMINI_API_KEY"
//! ```
//!
//! Provider keys live ONLY here on the AI node — never in `relix-web-bridge`
//! or any presentation peer.

pub mod belief_state;
pub mod complexity;
pub mod execution;
pub mod failover;
pub mod guardrails;
pub mod judge;
pub mod memory_dispatcher;
pub mod perception_security;
pub mod provenance_hooks;
pub mod provider;
pub mod reasoning_status;
pub mod router;
pub mod skill_caps;
pub mod skill_extractor;
pub mod skill_refinement;
pub mod skill_store;
pub mod skills;
pub mod soul;
pub mod tier_routing;

pub use memory_dispatcher::{MemoryDispatcher, MemoryFetcher};

pub use failover::{
    FailoverCategory, FailoverReason, classify_http_failure, classify_transport_failure,
};
pub use router::{
    HealthAwareRouter, NoopRouter, ProviderHealth, ProviderRouter, RouteCandidate, RouteDecision,
};

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
pub use provider::ChatInput;
use provider::{
    AnthropicProvider, ChatProvider, EmbedInput, GeminiProvider, MockProvider,
    OpenAICompatibleProvider, ProviderEntries, ProviderEntry, ProviderError, StreamingChunk,
};

/// Per-node AI configuration parsed from controller TOML `[ai]`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AiConfig {
    /// Active provider name. See module docs for the supported set.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Default model id used when ChatInput.model is empty. The provider
    /// also has its own `default_model`; this field is `[ai] model`.
    #[serde(default)]
    pub model: String,
    /// Per-provider settings, keyed by provider name (e.g. `openrouter`).
    #[serde(default)]
    pub providers: ProviderEntries,
    /// Optional memory-peer wiring for frozen-snapshot memory
    /// injection. When set, the AI controller dials this peer
    /// at startup and `ai.chat` reads per-subject memory from
    /// it before invoking the provider. When `None`, memory
    /// injection is silently skipped — the AI node runs with no
    /// outbound mesh capability.
    #[serde(default, rename = "memory_peer")]
    pub memory_peer: Option<AiMemoryPeerConfig>,
    /// Optional `[ai.agent]` block carrying the agent's name and
    /// soul (persona) file pointer. When set with `soul_path` OR
    /// with `name` matching a discoverable file under
    /// `~/.relix/souls/<name>.md` or `./souls/<name>.md`, the
    /// AI node prepends the soul content to the system prompt
    /// for every `ai.chat` call.
    ///
    /// Missing block means no soul is loaded — the existing
    /// memory + RAG composition path is unchanged. See
    /// [`crate::nodes::ai::soul`].
    #[serde(default)]
    pub agent: Option<AgentConfig>,
    /// `[ai.routing]` — RELIX-7.29 PART 1 smart model routing.
    /// When present and `enabled = true`, every chat request
    /// is classified via [`complexity::ComplexityClassifier`]
    /// and dispatched against the per-tier provider + model
    /// configured under `[ai.routing.tiers.<simple|medium|complex>]`.
    /// Absent / `enabled = false` keeps the AI handler byte-
    /// identical to its pre-routing behaviour.
    #[serde(default)]
    pub routing: Option<tier_routing::RoutingConfig>,
    /// `[ai.belief_state]` — RELIX-7.29 PART 3 LLM-driven
    /// belief tracker. When `enabled = true`, every `ai.chat`
    /// prepends the current beliefs for `(subject, session)`
    /// to the system prompt and asynchronously updates them
    /// via a structured belief-model prompt after the
    /// response is delivered. Absent / disabled keeps the AI
    /// handler byte-identical.
    #[serde(default)]
    pub belief_state: Option<belief_state::BeliefStateConfig>,
    /// `[ai.judge]` — RELIX-7.29 PART 4 judge model. When
    /// `enabled = true` AND the activation gate (low
    /// confidence + tool/structured response + ≥2 prior
    /// turns) fires, the AI handler dispatches a second
    /// provider call to a judge model with a 5-question
    /// prompt. The judge's verdict is recorded in a ring
    /// buffer surfaced by `judge.recent_verdicts` and
    /// `judge.stats`.
    #[serde(default)]
    pub judge: Option<judge::JudgeConfig>,
    /// `[ai.perception_security]` — RELIX-7.23 two-stage
    /// isolation. When `enabled = true`, the
    /// `ai.perception_extract` cap dispatches against the
    /// configured `extraction_model` with a hardened system
    /// prompt that treats every byte of input as untrusted
    /// data. Absent / `enabled = false` keeps the cap
    /// registered but in a documented-disabled mode that
    /// signals callers to fall through to plain `ai.chat`.
    #[serde(default)]
    pub perception_security: Option<perception_security::PerceptionSecurityConfig>,
}

/// `[ai.agent]` config — operator-supplied persona pointer for
/// this AI controller. Both fields are optional but at least one
/// must be set for the loader to find a soul file.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct AgentConfig {
    /// Agent slug. When `soul_path` is unset, the loader probes
    /// `~/.relix/souls/<name>.md` and `./souls/<name>.md`.
    #[serde(default)]
    pub name: String,
    /// Explicit path to the SOUL.md file. Wins over `name`-based
    /// auto-discovery when set.
    #[serde(default)]
    pub soul_path: Option<std::path::PathBuf>,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: String::new(),
            providers: ProviderEntries::new(),
            memory_peer: None,
            agent: None,
            routing: None,
            belief_state: None,
            judge: None,
            perception_security: None,
        }
    }
}

/// `[ai.memory_peer]` config — names the memory peer this AI
/// controller should dial for frozen-snapshot memory AND for
/// automatic conversation history.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AiMemoryPeerConfig {
    /// libp2p multiaddr of the memory peer (e.g.
    /// `/ip4/127.0.0.1/tcp/19711`).
    pub addr: String,
    /// Alias the outbound MeshClient uses to dial. Defaults
    /// to `"memory"` so chat code can just say `memory`.
    #[serde(default = "default_memory_alias")]
    pub alias: String,
    /// Per-call deadline in seconds. `memory.agent_read` and
    /// `memory.recent_for_session` are both cheap point reads;
    /// 5s is plenty.
    #[serde(default = "default_memory_deadline_secs")]
    pub deadline_secs: i64,
    /// How many recent turns the AI node asks
    /// `memory.recent_for_session` for when auto-injecting
    /// conversation history. Defaults to 10. Memory enforces its
    /// own ceiling on top of this.
    #[serde(default = "default_max_history_turns")]
    pub max_history_turns: usize,
    /// Whether `ai.chat` performs RAG retrieval against the
    /// vector memory before invoking the provider. Defaults to
    /// `false` so existing deployments don't pay the embed +
    /// search cost without opting in.
    #[serde(default)]
    pub rag_enabled: bool,
    /// Top-K limit for RAG. Defaults to 5.
    #[serde(default = "default_rag_top_k")]
    pub rag_top_k: usize,
    /// Cosine-similarity floor for RAG hits. Defaults to 0.70.
    /// Hits below this score are dropped before formatting.
    #[serde(default = "default_rag_min_score")]
    pub rag_min_score: f32,
}

fn default_memory_alias() -> String {
    "memory".to_string()
}

fn default_memory_deadline_secs() -> i64 {
    5
}

fn default_max_history_turns() -> usize {
    10
}

fn default_rag_top_k() -> usize {
    5
}

fn default_rag_min_score() -> f32 {
    0.70
}

fn default_provider() -> String {
    "mock".to_string()
}

/// Build the configured provider. Returns an Arc-wrapped trait object so
/// the handler closure can clone it cheaply across concurrent requests.
pub fn build_provider(cfg: &AiConfig) -> Result<Arc<dyn ChatProvider>, Box<dyn std::error::Error>> {
    // Helper: get the per-provider entry; default-construct if absent so
    // providers that legitimately need no config (mock) still boot.
    let entry =
        |name: &str| -> ProviderEntry { cfg.providers.get(name).cloned().unwrap_or_default() };

    match cfg.provider.as_str() {
        "mock" => Ok(Arc::new(MockProvider) as Arc<dyn ChatProvider>),
        "openai" => {
            let e = entry_or_err(&cfg.providers, "openai")?;
            let p = OpenAICompatibleProvider::from_entry("openai", &e)?;
            Ok(Arc::new(p))
        }
        "openrouter" => {
            let e = entry_or_err(&cfg.providers, "openrouter")?;
            let p = OpenAICompatibleProvider::from_entry("openrouter", &e)?;
            Ok(Arc::new(p))
        }
        "xai" => {
            let e = entry_or_err(&cfg.providers, "xai")?;
            let p = OpenAICompatibleProvider::from_entry("xai", &e)?;
            Ok(Arc::new(p))
        }
        "local" => {
            let e = entry_or_err(&cfg.providers, "local")?;
            let p = OpenAICompatibleProvider::from_entry("local", &e)?;
            Ok(Arc::new(p))
        }
        "anthropic" => {
            let e = entry_or_err(&cfg.providers, "anthropic")?;
            let p = AnthropicProvider::from_entry(&e)?;
            Ok(Arc::new(p))
        }
        "gemini" => {
            let e = entry(&cfg.provider);
            let p = GeminiProvider::from_entry(&e)?;
            Ok(Arc::new(p))
        }
        other => Err(format!("ai: unknown provider '{other}'").into()),
    }
}

fn entry_or_err(
    map: &ProviderEntries,
    name: &str,
) -> Result<ProviderEntry, Box<dyn std::error::Error>> {
    map.get(name).cloned().ok_or_else(|| {
        format!("provider='{name}' requires an [ai.providers.{name}] config section").into()
    })
}

/// Register the `ai.chat` capability with the supplied provider.
///
/// `memory_dispatcher` is the frozen-snapshot memory hook. The
/// AI controller populates the `OnceCell` after startup once it
/// has dialled the memory peer; when the cell is empty (memory
/// peer not configured or discovery hasn't finished yet),
/// `ai.chat` proceeds without memory injection. The cell stays
/// shared across all chat invocations, so the dispatcher is
/// constructed exactly once per controller process.
///
/// `metrics_sink` is the RELIX-7.11 AI-side enrichment hook.
/// When the operator has enabled `[metrics]` on this controller
/// the runtime builds a `MetricsCollector` whose
/// `attach_ai_usage` joins per-call token usage onto the metric
/// row the dispatch bridge records. `None` keeps the AI node
/// running in pre-7.11 mode — every metric column except
/// `token_count` + `cost_micros` populates regardless.
#[allow(clippy::too_many_arguments)]
pub fn register(
    bridge: &mut DispatchBridge,
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    memory_dispatcher: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>>,
    soul_cache: SoulCache,
    skills_cache: skills::SkillMatcher,
    input_guardrail: guardrails::InputGuardrail,
    tool_dispatcher: Option<Arc<crate::nodes::tool::dispatcher::ToolDispatcher>>,
    tool_mesh: Arc<tokio::sync::OnceCell<Arc<dyn execution::ToolMeshDispatcher>>>,
    metrics_sink: Option<Arc<dyn crate::metrics::MetricsSink>>,
    interaction_sink: Option<Arc<dyn crate::training::InteractionSink>>,
    skill_extractor: Option<Arc<skill_extractor::SkillExtractor>>,
    observability: Option<Arc<crate::observability::ObservabilityContext>>,
    routing_config: Option<tier_routing::RoutingConfig>,
    self_consistency_config: Option<crate::confidence::SelfConsistencyConfig>,
    self_consistency_stats: Option<crate::confidence::SelfConsistencyStats>,
    belief_state_config: Option<belief_state::BeliefStateConfig>,
    // RELIX-7.29 follow-up — optional layered store handle the
    // belief tracker writes through for cross-restart
    // persistence. `None` keeps the tracker process-local.
    belief_persistence_store: Option<Arc<crate::nodes::memory::schema::LayeredMemoryStore>>,
    judge_runtime_config: Option<judge::JudgeConfig>,
    perception_security_config: Option<perception_security::PerceptionSecurityConfig>,
) -> (belief_state::BeliefStateTracker, judge::JudgeRecorder) {
    // RELIX-7.29 PART 1: build the spec'd `[ai.routing]` tier
    // router. The registry maps provider names to the active
    // provider Arc — single-provider deployments register only
    // their own name. Multi-provider tier routing is honoured
    // when operators wire additional providers via
    // `[ai.providers.<name>]`; a tier whose `provider` field
    // does not match any registered name falls back to the
    // default provider per the resolver's policy.
    let mut routing_registry_map: std::collections::HashMap<String, Arc<dyn ChatProvider>> =
        std::collections::HashMap::new();
    routing_registry_map.insert(provider.provider_name().to_string(), provider.clone());
    let routing_registry = tier_routing::ProviderRegistry::new(routing_registry_map);
    let routing_cfg_shared = routing_config.unwrap_or_default();
    let routing_router_shared =
        tier_routing::TierRouter::new(routing_cfg_shared.clone(), routing_registry, Vec::new());
    let routing_router_for_chat = routing_router_shared.clone();
    // RELIX-7.29 PART 2: per-controller `[confidence.self_consistency]`
    // config. Absent / `enabled = false` keeps the AI handler
    // byte-identical to its pre-SC behaviour.
    let sc_cfg_shared = self_consistency_config.unwrap_or_default();
    let sc_cfg_for_chat = sc_cfg_shared.clone();
    let sc_stats_shared = self_consistency_stats.unwrap_or_default();
    let sc_stats_for_chat = sc_stats_shared.clone();
    // RELIX-7.29 PART 3: shared belief tracker. The AI handler
    // reads + writes through this Arc; the coordinator
    // `belief.*` caps are constructed against the same
    // instance so operator reads / resets see the same store.
    //
    // RELIX-7.29 follow-up: when an in-process LayeredMemoryStore
    // handle is wired, the tracker upserts every belief list to
    // a Layer-4 record so beliefs survive a controller restart.
    let belief_cfg_resolved = belief_state_config.unwrap_or_default();
    let belief_tracker_shared = match belief_persistence_store {
        Some(store) => belief_state::BeliefStateTracker::with_store(belief_cfg_resolved, store),
        None => belief_state::BeliefStateTracker::new(belief_cfg_resolved),
    };
    let belief_tracker_for_chat = belief_tracker_shared.clone();
    // RELIX-7.29 PART 4: judge config + recorder + turn
    // counter. The AI handler bumps the turn counter once per
    // call; the recorder buffers verdicts for the
    // `judge.recent_verdicts` cap.
    let judge_runtime_cfg_shared = judge_runtime_config.unwrap_or_default();
    let judge_recorder_shared =
        judge::JudgeRecorder::new(judge_runtime_cfg_shared.recent_buffer_size);
    let judge_turns_shared = judge::SessionTurnCounter::new();
    let judge_runtime_cfg_for_chat = judge_runtime_cfg_shared.clone();
    let judge_recorder_for_chat = judge_recorder_shared.clone();
    let judge_turns_for_chat = judge_turns_shared.clone();
    // RELIX-7.29 PART 1: register the `routing.explain`
    // coordinator cap. The cap is always registered (regardless
    // of whether `[ai.routing] enabled` is true) so operators
    // can dry-run the classifier even when routing is off.
    tier_routing::caps::register(bridge, routing_router_shared.clone());
    // RELIX-GAP-10 / §7.23: register `ai.perception_extract`.
    // Always registered; disabled config returns a documented
    // "isolated=false" envelope so callers know to fall
    // through to plain `ai.chat`.
    perception_security::register(
        bridge,
        provider.clone(),
        default_model.clone(),
        perception_security_config.unwrap_or_default(),
    );
    let provider_for_chat = provider.clone();
    let model_for_chat = default_model.clone();
    let memory_for_chat = memory_dispatcher.clone();
    let soul_for_chat = soul_cache.clone();
    let skills_for_chat = skills_cache.clone();
    let guardrail_for_chat = input_guardrail.clone();
    let dispatcher_for_chat = tool_dispatcher.clone();
    let mesh_for_chat = tool_mesh.clone();
    let metrics_for_chat = metrics_sink.clone();
    let provider_name_for_chat: String = provider.provider_name().to_string();
    let interaction_for_chat = interaction_sink.clone();
    let extractor_for_chat = skill_extractor.clone();
    let observability_for_chat = observability.clone();
    bridge.register(
        "ai.chat",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let p = provider_for_chat.clone();
            let model = model_for_chat.clone();
            let mem = memory_for_chat.clone();
            let soul = soul_for_chat.clone();
            let sk = skills_for_chat.clone();
            let gr = guardrail_for_chat.clone();
            let td = dispatcher_for_chat.clone();
            let mesh = mesh_for_chat.clone();
            let metrics = metrics_for_chat.clone();
            let provider_name = provider_name_for_chat.clone();
            let training = interaction_for_chat.clone();
            let extractor = extractor_for_chat.clone();
            let obs = observability_for_chat.clone();
            let routing_router = routing_router_for_chat.clone();
            let sc_runtime_cfg = sc_cfg_for_chat.clone();
            let sc_runtime_stats = sc_stats_for_chat.clone();
            let belief_tracker = belief_tracker_for_chat.clone();
            let judge_runtime_cfg = judge_runtime_cfg_for_chat.clone();
            let judge_recorder = judge_recorder_for_chat.clone();
            let judge_turns = judge_turns_for_chat.clone();
            async move {
                handle_chat(
                    p,
                    model,
                    mem,
                    soul,
                    sk,
                    gr,
                    td,
                    mesh,
                    metrics,
                    training,
                    extractor,
                    obs,
                    provider_name,
                    routing_router,
                    sc_runtime_cfg,
                    sc_runtime_stats,
                    belief_tracker,
                    judge_runtime_cfg,
                    judge_recorder,
                    judge_turns,
                    ctx,
                )
                .await
            }
        })),
    );
    // RELIX-2 step 3: register the streaming variant. Shares
    // the same provider + model + memory + soul + skills +
    // guardrail as the unary path; differs ONLY in:
    //   * uses `generate_reply_stream` instead of
    //     `generate_reply`;
    //   * skips the planner / tool dispatch / approval verdict
    //     pipeline (streaming is "stream tokens, period");
    //   * registered against the bridge's streaming dispatch
    //     so a `/relix/rpc/stream/1` substream caller hits
    //     this handler.
    let provider_for_chat_stream = provider.clone();
    let model_for_chat_stream = default_model.clone();
    let mem_for_chat_stream = memory_dispatcher.clone();
    let soul_for_chat_stream = soul_cache.clone();
    let skills_for_chat_stream = skills_cache.clone();
    let guardrail_for_chat_stream = input_guardrail.clone();
    let metrics_for_chat_stream = metrics_sink.clone();
    let interaction_for_chat_stream = interaction_sink.clone();
    let provider_name_for_chat_stream: String = provider.provider_name().to_string();
    let observability_for_stream = observability.clone();
    let routing_router_for_chat_stream = routing_router_shared.clone();
    let sc_cfg_for_chat_stream_runtime = sc_cfg_shared.clone();
    let sc_stats_for_chat_stream = sc_stats_shared.clone();
    let belief_tracker_for_chat_stream = belief_tracker_shared.clone();
    let judge_runtime_cfg_for_chat_stream = judge_runtime_cfg_shared.clone();
    let judge_recorder_for_chat_stream = judge_recorder_shared.clone();
    let judge_turns_for_chat_stream = judge_turns_shared.clone();
    bridge.register_streaming(
        "ai.chat.stream",
        Arc::new(crate::dispatch::FnStreamingHandler(
            move |ctx: InvocationCtx| {
                let p = provider_for_chat_stream.clone();
                let model = model_for_chat_stream.clone();
                let mem = mem_for_chat_stream.clone();
                let soul = soul_for_chat_stream.clone();
                let sk = skills_for_chat_stream.clone();
                let gr = guardrail_for_chat_stream.clone();
                let metrics = metrics_for_chat_stream.clone();
                let training = interaction_for_chat_stream.clone();
                let provider_name = provider_name_for_chat_stream.clone();
                let obs = observability_for_stream.clone();
                let routing_router = routing_router_for_chat_stream.clone();
                let sc_runtime_cfg = sc_cfg_for_chat_stream_runtime.clone();
                let sc_runtime_stats = sc_stats_for_chat_stream.clone();
                let belief_tracker = belief_tracker_for_chat_stream.clone();
                let judge_runtime_cfg = judge_runtime_cfg_for_chat_stream.clone();
                let judge_recorder = judge_recorder_for_chat_stream.clone();
                let judge_turns = judge_turns_for_chat_stream.clone();
                async move {
                    handle_chat_stream(
                        p,
                        model,
                        mem,
                        soul,
                        sk,
                        gr,
                        metrics,
                        training,
                        obs,
                        provider_name,
                        routing_router,
                        sc_runtime_cfg,
                        sc_runtime_stats,
                        belief_tracker,
                        judge_runtime_cfg,
                        judge_recorder,
                        judge_turns,
                        ctx,
                    )
                    .await
                }
            },
        )),
    );
    let provider_for_embed = provider.clone();
    bridge.register(
        "ai.embed",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let p = provider_for_embed.clone();
            async move { handle_embed(p, ctx).await }
        })),
    );
    // RELIX-7.29 PART 3: register `belief.get` + `belief.reset`
    // coordinator caps so operators can inspect / clear the
    // tracker through the dispatch bridge. Always registered;
    // caps return `enabled: false` when the tracker is off.
    belief_state::caps::register(bridge, belief_tracker_shared.clone());
    // RELIX-7.29 PART 4: register `judge.recent_verdicts` +
    // `judge.stats` against the shared recorder.
    judge::caps::register(bridge, judge_recorder_shared.clone());
    // RELIX-7.29 PART 5: register `reasoning.status` against
    // every shared component handle so the cap snapshots the
    // SAME instances the AI handler reads + writes to.
    let status = reasoning_status::ReasoningStatus::new(
        routing_router_shared.clone(),
        sc_cfg_shared.clone(),
        sc_stats_shared.clone(),
        belief_tracker_shared.clone(),
        judge_runtime_cfg_shared.clone(),
        judge_recorder_shared.clone(),
    );
    reasoning_status::register(bridge, status);
    (belief_tracker_shared, judge_recorder_shared)
}

/// Render an `f32` array as standard base64 of the little-endian
/// packed bytes. Used by `ai.embed` to keep the wire format ASCII.
fn encode_embedding_b64(v: &[f32]) -> String {
    use base64::Engine;
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn handle_embed(provider: Arc<dyn ChatProvider>, ctx: InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("ai.embed arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    // Wire: `model|text1§text2§text3...`. Model may be empty
    // (provider chooses default); texts are §-separated since `|`
    // is the field separator. Empty text segments are dropped.
    let Some((model, rest)) = s.split_once('|') else {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "ai.embed arg must be `model|text1§text2§...`".to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    };
    let texts: Vec<String> = rest
        .split('§')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if texts.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "ai.embed: at least one non-empty text required".to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let result = provider
        .generate_embeddings(EmbedInput {
            model: model.to_string(),
            texts,
        })
        .await;
    match result {
        Ok(out) => {
            let mut body = String::with_capacity(out.model.len() + out.vectors.len() * 64);
            body.push_str(&out.model);
            for v in &out.vectors {
                body.push('|');
                body.push_str(&encode_embedding_b64(v));
            }
            body.push('\n');
            HandlerOutcome::Ok(body.into_bytes())
        }
        Err(ProviderError::Transient(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_OVERLOADED,
            cause: format!("ai.embed: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
        Err(ProviderError::Permanent(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("ai.embed: {c}"),
            retry_hint: 2,
            retry_after: None,
        }),
    }
}

/// Embed the user prompt locally via the controller's own
/// provider (no libp2p hop — same process, same Arc), then ask
/// the memory dispatcher to do the actual vector search. Any
/// provider error (including "embeddings unsupported") returns
/// `None` so RAG silently skips and `ai.chat` continues. Empty
/// or missing query vector also returns `None`.
async fn embed_and_rag(
    provider: &dyn provider::ChatProvider,
    disp: &dyn MemoryFetcher,
    subject_id: &str,
    prompt: &str,
) -> Option<String> {
    let out = match provider
        .generate_embeddings(provider::EmbedInput {
            model: String::new(),
            texts: vec![prompt.to_string()],
        })
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "ai.chat rag: local embed failed (silent skip)"
            );
            return None;
        }
    };
    let query_vec = out.vectors.into_iter().next()?;
    if query_vec.is_empty() {
        return None;
    }
    disp.fetch_rag(
        subject_id,
        &query_vec,
        disp.rag_top_k(),
        disp.rag_min_score(),
    )
    .await
}

/// Cache for the resolved SOUL.md content. Tracks the source
/// file's last-modified timestamp so an operator who edits the
/// soul file mid-run sees the change on the next `ai.chat`
/// call without restarting the AI controller.
///
/// Cheap to clone (everything behind `Arc<Mutex<...>>`). One
/// instance lives on the AI node's closure capture; every
/// `handle_chat` call probes `current_content()` to get the
/// freshest soul without paying the discovery cost when the
/// file hasn't changed.
#[derive(Clone, Debug)]
pub struct SoulCache {
    inner: Arc<std::sync::Mutex<SoulCacheInner>>,
    explicit: Option<std::path::PathBuf>,
    agent_name: String,
}

#[derive(Debug)]
struct SoulCacheInner {
    /// Last seen on-disk mtime, in unix seconds. `None` means
    /// the cache has never seen the file (cold) OR the file is
    /// missing.
    last_mtime: Option<i64>,
    /// Cached resolved soul. `None` when no soul file was
    /// discovered on the last probe.
    cached: Option<soul::Soul>,
}

impl SoulCache {
    /// Permanent-no-op cache — `current()` always returns
    /// `None`. Tests and the legacy code path that doesn't pass
    /// an agent config use this; the AI handler skips the soul
    /// prepend when the cache returns `None`.
    pub fn no_op() -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(SoulCacheInner {
                last_mtime: None,
                cached: None,
            })),
            explicit: None,
            agent_name: String::new(),
        }
    }

    /// Construct from the `[ai.agent]` config block. When the
    /// block is absent (or both `name` and `soul_path` are
    /// empty), the cache is a permanent no-op — every probe
    /// returns `None` and the AI handler skips the prepend.
    pub fn from_config(agent: Option<&AgentConfig>) -> Self {
        let (explicit, agent_name) = match agent {
            Some(a) => (a.soul_path.clone(), a.name.clone()),
            None => (None, String::new()),
        };
        Self {
            inner: Arc::new(std::sync::Mutex::new(SoulCacheInner {
                last_mtime: None,
                cached: None,
            })),
            explicit,
            agent_name,
        }
    }

    /// Return the current soul content, reloading from disk when
    /// the source file's mtime changed since the previous probe.
    /// Returns `None` when no soul file is configured / discovered.
    pub fn current(&self) -> Option<soul::Soul> {
        // No agent + no explicit path → cache is a permanent
        // no-op. Spelled out so the disk probe doesn't fire
        // for controllers that don't use souls at all.
        if self.explicit.is_none() && self.agent_name.is_empty() {
            return None;
        }
        let candidates = soul::candidate_paths(&self.agent_name, self.explicit.as_deref());
        let mut current_mtime: Option<i64> = None;
        for c in &candidates {
            if let Ok(meta) = std::fs::metadata(c)
                && let Ok(modified) = meta.modified()
                && let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                current_mtime = Some(d.as_secs() as i64);
                break;
            }
        }
        let mut guard = self.inner.lock().expect("soul cache lock");
        // Fast path: file mtime unchanged AND we have a cached
        // value — return the cached soul.
        if guard.last_mtime == current_mtime && guard.cached.is_some() {
            return guard.cached.clone();
        }
        // Reload. discover() walks the same candidate paths.
        let fresh = soul::discover(&self.agent_name, self.explicit.as_deref());
        guard.last_mtime = current_mtime;
        guard.cached = fresh.clone();
        fresh
    }
}

/// Build the final `system_prompt` from the two optional blocks
/// the dispatcher might produce. Agent / user memory comes
/// first; RAG block sits after it. A single blank line separates
/// them so the model sees two distinct sections.
fn combine_system_blocks(agent_block: Option<String>, rag_block: Option<String>) -> Option<String> {
    match (agent_block, rag_block) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(r)) => Some(r),
        (Some(a), Some(r)) => {
            let mut out = String::with_capacity(a.len() + r.len() + 2);
            out.push_str(&a);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
            out.push_str(&r);
            Some(out)
        }
    }
}

/// Combine auto-fetched conversation history with the caller-
/// supplied `history` field on the wire. Auto-fetched lines come
/// first (they're the older context), caller-supplied lines are
/// appended after. A single trailing newline is normalised on
/// the auto-fetched block so the two segments meet cleanly.
/// Pull an `approval_token=<value>` field out of an `ai.chat`
/// arg buffer. Looks for the substring anywhere in the args
/// — operators can append it to the prompt or the history
/// without breaking the existing pipe-delimited shape.
/// Returns `None` when no token is present.
fn extract_approval_token(args: &str) -> Option<String> {
    let needle = "approval_token=";
    let idx = args.find(needle)?;
    let rest = &args[idx + needle.len()..];
    // The token runs until the next whitespace or pipe so
    // callers can suffix it without trailing garbage.
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|')
        .unwrap_or(rest.len());
    let token = rest[..end].trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn merge_history(auto: &str, caller: &str) -> String {
    if auto.is_empty() {
        return caller.to_string();
    }
    if caller.is_empty() {
        return auto.to_string();
    }
    let mut out = String::with_capacity(auto.len() + caller.len() + 1);
    out.push_str(auto);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(caller);
    out
}

#[allow(clippy::too_many_arguments)]
/// RELIX-2 step 3: pre-flight common to `ai.chat` and
/// `ai.chat.stream`. Parses the wire args, runs the input
/// guardrail, fetches memory + RAG, applies SOUL persona +
/// skill hints, and produces a [`ChatInput`] ready for the
/// provider. Both the unary `generate_reply` and the streaming
/// `generate_reply_stream` paths consume the same input.
///
/// Errors mirror the early-return shapes from the original
/// `handle_chat` — invalid utf-8, missing session_id, guardrail
/// rejection. Callers convert the `ErrorEnvelope` to whichever
/// response shape their transport expects (unary
/// `HandlerOutcome::Err`, streaming terminal `StreamFrame::Err`).
struct ChatPreflight {
    session_id: String,
    input: ChatInput,
    /// Approval token extracted from the request envelope.
    /// Used by `handle_chat`'s post-flight policy verdict path;
    /// the streaming variant ignores it (no planner runs).
    approval_token: Option<String>,
}

#[allow(clippy::too_many_arguments)]
/// SEC PART 5: parse `ai.chat` args from EITHER the new
/// JSON object form (`{"session_id":"…","prompt":"…","history":"…"}`)
/// OR the legacy pipe-delimited form (`session_id|prompt|history`).
/// The pipe-delimited form corrupts on `|` characters in
/// `session_id` or `prompt`; JSON is unambiguous so the
/// planning callers all send JSON now. Pre-existing callers
/// (SOL flows, the CLI, etc.) keep working through the
/// pipe-delimited fallback.
fn parse_ai_chat_args(args: &[u8]) -> Result<(String, String, String), ErrorEnvelope> {
    let s = match std::str::from_utf8(args) {
        Ok(s) => s,
        Err(e) => {
            return Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("ai.chat arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let trimmed = s.trim_start();
    if trimmed.starts_with('{') {
        #[derive(serde::Deserialize)]
        struct Wire {
            session_id: String,
            prompt: String,
            #[serde(default)]
            history: String,
        }
        match serde_json::from_str::<Wire>(trimmed) {
            Ok(w) => {
                if w.session_id.is_empty() {
                    return Err(ErrorEnvelope {
                        kind: error_kinds::INVALID_ARGS,
                        cause: "ai.chat: session_id required".to_string(),
                        retry_hint: 2,
                        retry_after: None,
                    });
                }
                return Ok((w.session_id, w.prompt, w.history));
            }
            Err(e) => {
                return Err(ErrorEnvelope {
                    kind: error_kinds::INVALID_ARGS,
                    cause: format!("ai.chat JSON args decode: {e}"),
                    retry_hint: 2,
                    retry_after: None,
                });
            }
        }
    }
    let mut parts = s.splitn(3, '|');
    let session_id = parts.next().unwrap_or("");
    let prompt = parts.next();
    let history = parts.next().unwrap_or("");
    let Some(prompt) = prompt else {
        return Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "ai.chat arg must be JSON object {session_id,prompt[,history]} \
                    or legacy pipe-delimited `session_id|prompt[|history]`"
                .to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    };
    if session_id.is_empty() {
        return Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "ai.chat: session_id required".to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    Ok((
        session_id.to_string(),
        prompt.to_string(),
        history.to_string(),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn build_chat_preflight(
    args: &[u8],
    provider: &Arc<dyn ChatProvider>,
    default_model: &str,
    memory_dispatcher: &Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>>,
    soul_cache: &SoulCache,
    skills_cache: &skills::SkillMatcher,
    input_guardrail: &guardrails::InputGuardrail,
    caller_subject_id: &str,
) -> Result<ChatPreflight, ErrorEnvelope> {
    let s = match std::str::from_utf8(args) {
        Ok(s) => s,
        Err(e) => {
            return Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("ai.chat arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    // SEC PART 5: JSON-first, pipe-fallback decoder.
    let (session_id_owned, prompt_owned, history_owned) = parse_ai_chat_args(args)?;
    let session_id = session_id_owned.as_str();
    let prompt = prompt_owned.as_str();
    let history = history_owned.as_str();

    // Input guardrail. Same posture as `handle_chat`.
    let guardrail_result = input_guardrail.check(prompt);
    if !guardrail_result.allowed {
        let reason = guardrail_result
            .reason
            .unwrap_or_else(|| "input guardrail rejected prompt".to_string());
        let preview: String = prompt.chars().take(80).collect();
        tracing::warn!(
            session_id,
            preview = %preview,
            reason = %reason,
            "ai.chat: input guardrail blocked prompt"
        );
        return Err(ErrorEnvelope {
            kind: error_kinds::SECURITY_DENIED,
            cause: format!("ai.chat: {reason}"),
            retry_hint: 0,
            retry_after: None,
        });
    }
    if guardrail_result.pii_detected {
        tracing::info!(
            session_id,
            categories = ?guardrail_result.categories,
            "ai.chat: input guardrail redacted PII before model call"
        );
    }
    let prompt: &str = &guardrail_result.text;

    // Memory + RAG.
    let (system_prompt, merged_history) = if let Some(disp) = memory_dispatcher.get() {
        let agent_block = match disp.fetch(caller_subject_id).await {
            Some((agent_mem, user_mem)) => {
                memory_dispatcher::format_memory_block(&agent_mem, &user_mem)
            }
            None => None,
        };
        let rag_block = if disp.rag_enabled() {
            embed_and_rag(provider.as_ref(), disp.as_ref(), caller_subject_id, prompt).await
        } else {
            None
        };
        let sys = combine_system_blocks(agent_block, rag_block);
        let auto_history = disp.fetch_history(session_id).await.unwrap_or_default();
        (sys, merge_history(&auto_history, history))
    } else {
        (None, history.to_string())
    };

    // SOUL persona.
    let system_prompt = match soul_cache.current() {
        Some(soul) => Some(soul.into_system_prompt(system_prompt.as_deref())),
        None => system_prompt,
    };

    // Skill hint.
    let system_prompt = match skills_cache.matched_hint(prompt).await {
        Some(hint) => Some(match system_prompt {
            Some(existing) => {
                let mut combined = existing;
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push('\n');
                combined.push_str(&hint);
                combined
            }
            None => hint,
        }),
        None => system_prompt,
    };

    let approval_token = extract_approval_token(s);
    let input = ChatInput {
        session_id: session_id.to_string(),
        prompt: prompt.to_string(),
        history: merged_history,
        model: default_model.to_string(),
        system_prompt,
        ..ChatInput::default()
    };
    Ok(ChatPreflight {
        session_id: session_id.to_string(),
        input,
        approval_token,
    })
}

/// RELIX-2 step 3: `ai.chat.stream` handler. Runs the same
/// pre-flight as `ai.chat` (guardrails, memory + RAG, soul,
/// skills) and then pipes tokens from
/// [`ChatProvider::generate_reply_stream`] through the
/// dispatcher's `HandlerStream`. Each Ok(chunk) becomes a
/// `StreamFrame::Chunk` on the wire; a provider error
/// terminates with `StreamFrame::Err`.
///
/// Semantic difference from `ai.chat`: the streaming variant
/// does NOT run the planner / tool dispatch / approval
/// verdict pipeline. Streaming is "stream tokens to the user,
/// period." Operators that need inline tool execution use the
/// unary `ai.chat`. The capability descriptor declares this
/// explicitly so the dashboard surfaces the distinction.
#[allow(clippy::too_many_arguments)]
async fn handle_chat_stream(
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    memory_dispatcher: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>>,
    soul_cache: SoulCache,
    skills_cache: skills::SkillMatcher,
    input_guardrail: guardrails::InputGuardrail,
    metrics_sink: Option<Arc<dyn crate::metrics::MetricsSink>>,
    interaction_sink: Option<Arc<dyn crate::training::InteractionSink>>,
    observability: Option<Arc<crate::observability::ObservabilityContext>>,
    provider_name: String,
    routing_router: tier_routing::TierRouter,
    sc_cfg: crate::confidence::SelfConsistencyConfig,
    sc_stats: crate::confidence::SelfConsistencyStats,
    belief_tracker: belief_state::BeliefStateTracker,
    judge_runtime_cfg: judge::JudgeConfig,
    judge_recorder: judge::JudgeRecorder,
    judge_turns: judge::SessionTurnCounter,
    ctx: InvocationCtx,
) -> Result<crate::dispatch::HandlerStream, ErrorEnvelope> {
    // RELIX-7.29 (post-rebuild): the streaming variant uses
    // the new spec'd modules (tier_routing, confidence::SC,
    // belief_state, judge). Belief + judge are consumed
    // implicitly through the unary path called by the SC
    // sampler; on the pure-stream fallback they don't apply
    // because the streaming flow lacks the synchronous
    // post-response hooks.
    let _ = (
        &routing_router,
        &belief_tracker,
        &judge_runtime_cfg,
        &judge_recorder,
        &judge_turns,
    );
    let stream_started_at = std::time::Instant::now();
    let preflight = build_chat_preflight(
        &ctx.args,
        &provider,
        &default_model,
        &memory_dispatcher,
        &soul_cache,
        &skills_cache,
        &input_guardrail,
        &ctx.caller.subject_id.to_string(),
    )
    .await?;
    let session_id = preflight.session_id.clone();
    let _ = preflight.approval_token;
    let mut input = preflight.input;
    // RELIX-7.29 PART 1: streaming tier override via the
    // spec'd `[ai.routing]` tier router. Mirrors the unary
    // `handle_chat` logic so a smart-router operator sees the
    // same per-tier model on streaming + unary. Disabled
    // routers and unmapped tiers fall through to `input.model`
    // unchanged.
    if routing_router.enabled() {
        let score = complexity::ComplexityClassifier::new().classify(&input.prompt, 0);
        let decision = routing_router.resolve(&score);
        if let Some(m) = decision.model.as_deref() {
            tracing::info!(
                session_id,
                tier = decision.tier.as_str(),
                model = m,
                fell_back = decision.fell_back,
                "ai.chat.stream: ai.routing tier resolved"
            );
            input.model = m.to_string();
        }
    }
    // Snapshot fields the training-record build path needs
    // BEFORE we move `input` into `generate_reply_stream`. The
    // streaming path doesn't run the planner so there are no
    // tool calls to record.
    let training_system_prompt = input.system_prompt.clone().unwrap_or_default();
    let training_user_message = input.prompt.clone();
    let training_model = if input.model.is_empty() {
        default_model.clone()
    } else {
        input.model.clone()
    };
    let training_agent = ctx.caller.name.clone();

    // RELIX-7.29 (post-rebuild) — self-consistency on
    // streaming. Activation: enabled + capability matches +
    // sample_count >= 2. When the gate fires we run N unary
    // samples in parallel, score them, and chunk-stream the
    // winner instead of calling `generate_reply_stream`.
    // Failures (embed error, < 2 samples returned) fall back
    // to the normal streaming path with no observable change.
    if sc_cfg.matches_capability("ai.chat.stream") && sc_cfg.sample_count >= 2 {
        let sc_template = ChatInput {
            session_id: input.session_id.clone(),
            prompt: input.prompt.clone(),
            history: input.history.clone(),
            model: input.model.clone(),
            system_prompt: input.system_prompt.clone(),
            ..ChatInput::default()
        };
        if let Some(result) = run_self_consistency_stream(&provider, sc_template, &sc_cfg).await {
            // Attach the SC hint so the dispatch bridge's
            // ConfidenceScorer substitutes the score for
            // `provider_signal`.
            let request_id = ctx.request_id;
            if let Some(sink) = metrics_sink.as_ref() {
                sink.attach_self_consistency(crate::metrics::AiSelfConsistencyHint {
                    request_id,
                    score: result.outcome.score,
                    sample_count: result.outcome.samples.len() as u32,
                    best_sample_index: result.best_index as u32,
                });
                // Attach aggregated usage from every sample so
                // the metrics row reflects the true cost of the
                // SC fan-out, not just the winning sample.
                sink.attach_ai_usage(crate::metrics::AiUsageHint {
                    request_id,
                    prompt_tokens: result.total_prompt_tokens.min(u32::MAX as u64) as u32,
                    completion_tokens: result.total_completion_tokens.min(u32::MAX as u64) as u32,
                    model: result.model.clone(),
                    routing_tier: None,
                });
            }
            sc_stats.record(result.outcome.score, result.outcome.samples.len());
            tracing::info!(
                session_id,
                sc_score = result.outcome.score,
                sample_count = result.outcome.samples.len(),
                best_sample = result.best_index,
                "ai.chat.stream: self-consistency outcome recorded"
            );
            // Chunk the winning text into wire-sized frames
            // and stream them through. Mirror the normal-path
            // training + observability writes so the record
            // shape stays identical.
            let chunks = chunk_for_stream(&result.winner_text);
            let winner_text = result.winner_text.clone();
            let model_used = if result.model.is_empty() {
                training_model.clone()
            } else {
                result.model.clone()
            };
            let total_prompt = result.total_prompt_tokens.min(u32::MAX as u64) as u32;
            let total_completion = result.total_completion_tokens.min(u32::MAX as u64) as u32;
            let training_session = session_id.clone();
            let provider_name_owned = provider_name.clone();
            let training_system_prompt = training_system_prompt.clone();
            let training_user_message = training_user_message.clone();
            let training_agent = training_agent.clone();
            let interaction_sink = interaction_sink.clone();
            let observability = observability.clone();
            let mapped = async_stream::stream! {
                for c in chunks {
                    yield Ok(c);
                }
                if let Some(sink) = interaction_sink.as_ref() {
                    let rec = crate::training::InteractionRecord::new(
                        crate::training::InteractionId::from_request(&request_id),
                        training_session.clone(),
                        training_agent.clone(),
                        model_used.clone(),
                        provider_name_owned.clone(),
                        training_system_prompt.clone(),
                        training_user_message.clone(),
                        winner_text.clone(),
                        vec![],
                        Some(total_prompt),
                        Some(total_completion),
                        stream_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        true,
                        None,
                        0,
                    );
                    sink.record_interaction(rec);
                }
                if let Some(obs) = observability.as_ref() {
                    let trace_id = format!("{:032x}", u128::from_le_bytes(request_id.0));
                    let system_hash = if training_system_prompt.is_empty() {
                        String::new()
                    } else {
                        crate::nodes::ai::provenance_hooks::hash_blake3(&training_system_prompt)
                    };
                    crate::nodes::ai::provenance_hooks::record_chat_provenance(
                        obs.as_ref(),
                        &training_session,
                        &trace_id,
                        &model_used,
                        &system_hash,
                        Some(&training_agent),
                    );
                    let duration_ms =
                        stream_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    crate::nodes::ai::provenance_hooks::record_chat_metadata(
                        obs.as_ref(),
                        &training_session,
                        &trace_id,
                        &training_agent,
                        "ai.chat.stream.complete",
                        &model_used,
                        duration_ms,
                        Some(total_prompt as u64 + total_completion as u64),
                        true,
                    );
                }
            };
            return Ok(Box::pin(mapped));
        }
        // SC pipeline failed (provider error / embed error /
        // single sample returned). Drop through to the normal
        // streaming path so the caller still gets a response.
    }

    let provider_stream = provider.generate_reply_stream(input).await.map_err(|e| {
        let (kind, retry_hint) = match &e {
            ProviderError::Transient(_) => (error_kinds::RESPONDER_OVERLOADED, 1),
            ProviderError::Permanent(_) => (error_kinds::RESPONDER_INTERNAL, 2),
        };
        let cause = match e {
            ProviderError::Transient(c) | ProviderError::Permanent(c) => c,
        };
        ErrorEnvelope {
            kind,
            cause: format!("ai.chat.stream: {cause}"),
            retry_hint,
            retry_after: None,
        }
    })?;

    // Adapt the provider's `Result<StreamingChunk, ProviderError>`
    // stream into the dispatcher's `Result<Vec<u8>, ErrorEnvelope>`
    // shape. Text frames forward to the wire verbatim. Usage
    // frames are intercepted (NOT forwarded to the client — they
    // carry token counts the wire consumer doesn't care about)
    // and routed to `metrics_sink.attach_ai_usage` so the
    // RELIX-7.11 collector's join cache can merge them onto the
    // dispatch row.
    let request_id = ctx.request_id;
    let training_session = session_id.clone();
    let mapped = async_stream::stream! {
        use futures::StreamExt;
        let mut s = std::pin::pin!(provider_stream);
        let mut accumulated_text = String::new();
        let mut accumulated_usage: Option<(u32, u32, String)> = None;
        let mut stream_error: Option<String> = None;
        let mut stream_error_kind: Option<&'static str> = None;
        while let Some(item) = s.next().await {
            match item {
                Ok(StreamingChunk::Text(t)) => {
                    accumulated_text.push_str(&t);
                    yield Ok(t.into_bytes());
                }
                Ok(StreamingChunk::Usage(u)) => {
                    if let Some(sink) = metrics_sink.as_ref() {
                        sink.attach_ai_usage(crate::metrics::AiUsageHint {
                            request_id,
                            prompt_tokens: u.prompt_tokens,
                            completion_tokens: u.completion_tokens,
                            model: u.model.clone(),
                            routing_tier: None,
                        });
                    }
                    accumulated_usage = Some((u.prompt_tokens, u.completion_tokens, u.model));
                    // Don't forward Usage to the wire — it's
                    // observation metadata, not assistant text.
                }
                Ok(StreamingChunk::FinishReason(fr)) => {
                    // RELIX-7.19 GAP 3: side-channel the finish
                    // reason to the metrics sink so the dispatch
                    // bridge's ConfidenceScorer can pick it up
                    // when scoring the streaming response.
                    if let Some(sink) = metrics_sink.as_ref() {
                        sink.attach_provider_signals(crate::metrics::AiProviderSignalsHint {
                            request_id,
                            finish_reason: Some(fr),
                            logprob: None,
                        });
                    }
                    // Don't forward to the wire — operator-
                    // observation metadata only.
                }
                Err(ProviderError::Transient(c)) => {
                    stream_error = Some(c.clone());
                    stream_error_kind = Some("RESPONDER_OVERLOADED");
                    yield Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_OVERLOADED,
                        cause: format!("ai.chat.stream: {c}"),
                        retry_hint: 1,
                        retry_after: None,
                    });
                }
                Err(ProviderError::Permanent(c)) => {
                    stream_error = Some(c.clone());
                    stream_error_kind = Some("RESPONDER_INTERNAL");
                    yield Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("ai.chat.stream: {c}"),
                        retry_hint: 2,
                        retry_after: None,
                    });
                }
            }
        }
        // RELIX-7.15: record one training interaction at stream
        // close. Success iff we never observed a stream error;
        // tool calls are always empty (streaming path bypasses
        // the planner). The model name comes from the Usage
        // frame when present, else falls back to the requested
        // model.
        let (prompt_tokens_for_obs, completion_tokens_for_obs, model_used_for_obs) =
            match accumulated_usage.as_ref() {
                Some((p, c, m)) => (Some(*p), Some(*c), if m.is_empty() { training_model.clone() } else { m.clone() }),
                None => (None, None, training_model.clone()),
            };
        let stream_success = stream_error.is_none();
        if let Some(sink) = interaction_sink.as_ref() {
            let rec = crate::training::InteractionRecord::new(
                crate::training::InteractionId::from_request(&request_id),
                training_session.clone(),
                training_agent.clone(),
                model_used_for_obs.clone(),
                provider_name.clone(),
                training_system_prompt.clone(),
                training_user_message.clone(),
                accumulated_text.clone(),
                vec![],
                prompt_tokens_for_obs,
                completion_tokens_for_obs,
                stream_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                stream_success,
                stream_error_kind.map(|k| k.to_string()).or_else(|| {
                    stream_error.as_ref().map(|_| "RESPONDER_INTERNAL".to_string())
                }),
                0,
            );
            sink.record_interaction(rec);
        }
        // GAP 13 + 14: post-stream provenance + observability
        // metadata writes. Same shape as the unary path so
        // downstream tooling sees identical fields per session.
        if let Some(obs) = observability.as_ref() {
            let trace_id = format!("{:032x}", u128::from_le_bytes(request_id.0));
            let system_hash = if training_system_prompt.is_empty() {
                String::new()
            } else {
                crate::nodes::ai::provenance_hooks::hash_blake3(&training_system_prompt)
            };
            crate::nodes::ai::provenance_hooks::record_chat_provenance(
                obs.as_ref(),
                &training_session,
                &trace_id,
                &model_used_for_obs,
                &system_hash,
                Some(&training_agent),
            );
            let duration_ms =
                stream_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
            crate::nodes::ai::provenance_hooks::record_chat_metadata(
                obs.as_ref(),
                &training_session,
                &trace_id,
                &training_agent,
                "ai.chat.stream.complete",
                &model_used_for_obs,
                duration_ms,
                prompt_tokens_for_obs
                    .map(|p| p as u64 + completion_tokens_for_obs.unwrap_or(0) as u64),
                stream_success,
            );
        }
    };
    Ok(Box::pin(mapped))
}

#[allow(clippy::too_many_arguments)]
async fn handle_chat(
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    memory_dispatcher: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>>,
    soul_cache: SoulCache,
    skills_cache: skills::SkillMatcher,
    input_guardrail: guardrails::InputGuardrail,
    tool_dispatcher: Option<Arc<crate::nodes::tool::dispatcher::ToolDispatcher>>,
    tool_mesh: Arc<tokio::sync::OnceCell<Arc<dyn execution::ToolMeshDispatcher>>>,
    metrics_sink: Option<Arc<dyn crate::metrics::MetricsSink>>,
    interaction_sink: Option<Arc<dyn crate::training::InteractionSink>>,
    skill_extractor: Option<Arc<skill_extractor::SkillExtractor>>,
    observability: Option<Arc<crate::observability::ObservabilityContext>>,
    provider_name: String,
    routing_router: tier_routing::TierRouter,
    sc_cfg: crate::confidence::SelfConsistencyConfig,
    sc_stats: crate::confidence::SelfConsistencyStats,
    belief_tracker: belief_state::BeliefStateTracker,
    judge_runtime_cfg: judge::JudgeConfig,
    judge_recorder: judge::JudgeRecorder,
    judge_turns: judge::SessionTurnCounter,
    ctx: InvocationCtx,
) -> HandlerOutcome {
    let chat_started_at = std::time::Instant::now();
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("ai.chat arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    // SEC PART 5: JSON-first, pipe-fallback. See
    // [`parse_ai_chat_args`].
    let (session_id_owned, prompt_owned, history_owned) = match parse_ai_chat_args(&ctx.args) {
        Ok(t) => t,
        Err(env) => return HandlerOutcome::Err(env),
    };
    let session_id = session_id_owned.as_str();
    let prompt = prompt_owned.as_str();
    let history = history_owned.as_str();

    // Input guardrail. Runs before any model work so a
    // blocked prompt costs nothing past the substring scan +
    // a few regex find_iters. The verdict carries a possibly-
    // redacted text (we pass the redacted form to the model
    // when the operator picked redact mode) plus content
    // categories the audit log can surface.
    let guardrail_result = input_guardrail.check(prompt);
    if !guardrail_result.allowed {
        let reason = guardrail_result
            .reason
            .unwrap_or_else(|| "input guardrail rejected prompt".to_string());
        let preview: String = prompt.chars().take(80).collect();
        tracing::warn!(
            session_id,
            preview = %preview,
            reason = %reason,
            "ai.chat: input guardrail blocked prompt"
        );
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::SECURITY_DENIED,
            cause: format!("ai.chat: {reason}"),
            retry_hint: 0,
            retry_after: None,
        });
    }
    if guardrail_result.pii_detected {
        tracing::info!(
            session_id,
            categories = ?guardrail_result.categories,
            "ai.chat: input guardrail redacted PII before model call"
        );
    }
    // Use the (possibly-redacted) text downstream. Borrow as
    // a &str so the rest of the handler can keep its existing
    // `prompt: &str` shape.
    let prompt: &str = &guardrail_result.text;

    // Frozen-snapshot memory injection. Fetch agent + user
    // memory for the caller's subject_id once, build a labeled
    // block, and route it into ChatInput.system_prompt. The
    // dispatcher may be unset (cell empty) if the AI controller
    // wasn't configured with a memory peer or its discovery
    // hasn't finished yet — that's a silent skip per spec.
    //
    // The same dispatcher also serves automatic conversation
    // history (memory.recent_for_session) and optional RAG
    // retrieval (memory.search over the vector store). The
    // final system prompt is the concatenation of two blocks
    // when present: agent memory first, then RAG. Both are
    // silent-skip on any failure — `ai.chat` never fails
    // because memory is unavailable.
    let (system_prompt, merged_history) = if let Some(disp) = memory_dispatcher.get() {
        let subject_id = ctx.caller.subject_id.to_string();
        let agent_block = match disp.fetch(&subject_id).await {
            Some((agent_mem, user_mem)) => {
                memory_dispatcher::format_memory_block(&agent_mem, &user_mem)
            }
            None => None,
        };
        let rag_block = if disp.rag_enabled() {
            embed_and_rag(provider.as_ref(), disp.as_ref(), &subject_id, prompt).await
        } else {
            None
        };
        let sys = combine_system_blocks(agent_block, rag_block);
        let auto_history = disp.fetch_history(session_id).await.unwrap_or_default();
        (sys, merge_history(&auto_history, history))
    } else {
        (None, history.to_string())
    };
    // SOUL.md persona prepend. Runs AFTER memory + RAG so the
    // soul (operator-authored agent personality) sits at the
    // TOP of the system prompt — the model reads the persona
    // first, then the memory blocks, then the user message.
    // The cache hides the disk probe on the hot path; the
    // mtime check means an operator who edits the soul mid-run
    // sees the change on the next call.
    let system_prompt = match soul_cache.current() {
        Some(soul) => Some(soul.into_system_prompt(system_prompt.as_deref())),
        None => system_prompt,
    };
    // Skill hint: keyword-match the user's prompt against the
    // loaded skill library. When a match is found, append the
    // skill body as a system-prompt section so the model sees
    // "you have a procedure for this — use it." `None` (no
    // match / empty library) leaves the prompt unchanged.
    let system_prompt = match skills_cache.matched_hint(prompt).await {
        Some(hint) => Some(match system_prompt {
            Some(existing) => {
                let mut combined = existing;
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push('\n');
                combined.push_str(&hint);
                combined
            }
            None => hint,
        }),
        None => system_prompt,
    };

    // RELIX-7.29 PART 3: prepend the LLM-driven belief block
    // for this (subject_id, session_id) when the tracker is
    // enabled AND `inject_into_prompt` is true. Skipped when
    // the tracker is off or the block is empty.
    let belief_subject = ctx.caller.subject_id.to_string();
    let system_prompt = if belief_tracker.enabled() && belief_tracker.config().inject_into_prompt {
        let beliefs = belief_tracker.get(&belief_subject, session_id);
        let block = belief_state::format_for_system_prompt(&beliefs);
        if block.is_empty() {
            system_prompt
        } else {
            Some(match system_prompt {
                Some(existing) => {
                    let mut combined = block;
                    combined.push_str(&existing);
                    combined
                }
                None => block,
            })
        }
    } else {
        system_prompt
    };

    // RELIX-7.29 PART 1: spec'd `[ai.routing]` tier resolution.
    // Runs AFTER memory + RAG + skills (those use the existing
    // `provider` Arc for embedding etc.) and BEFORE building
    // ChatInput, so the provider Arc + model id swap only
    // affects the actual dispatch call. When `[ai.routing]
    // enabled = false` or all tiers fail health, the resolver
    // returns Unrouted and behaviour is byte-identical to the
    // pre-routing path.
    let mut resolved_model = default_model.clone();
    let mut provider: Arc<dyn ChatProvider> = provider;
    let mut routing_tier_label: Option<&'static str> = None;
    if routing_router.enabled() {
        // session_turns is plumbed via the routing.explain cap +
        // CLI for operator inspection; the hot path treats every
        // ai.chat call as turn 0 because the dispatcher doesn't
        // carry per-session turn counts. The seven other signals
        // (length, code, multi-step, technical, explicit, multi-
        // topic) dominate tier assignment in practice.
        let score = complexity::ComplexityClassifier::new().classify(prompt, 0);
        let decision = routing_router.resolve(&score);
        routing_tier_label = Some(tier_routing::metrics_tier_label(decision.tier));
        if let Some(p_name) = decision.provider.as_deref()
            && let Some(p_arc) = routing_router.registry().get(p_name)
        {
            provider = p_arc;
        }
        if let Some(m) = decision.model.as_deref() {
            resolved_model = m.to_string();
        }
        tracing::info!(
            session_id,
            tier = decision.tier.as_str(),
            provider = decision.provider.as_deref().unwrap_or("default"),
            model = decision.model.as_deref().unwrap_or("default"),
            fell_back = decision.fell_back,
            reasoning = %decision.reasoning,
            "ai.chat: ai.routing tier resolved"
        );
    }

    let input = ChatInput {
        session_id: session_id.to_string(),
        prompt: prompt.to_string(),
        history: merged_history,
        model: resolved_model,
        // Clone for the GAP 13 provenance hook below — the
        // ChatInput consumes the original Option.
        system_prompt: system_prompt.clone(),
        ..ChatInput::default()
    };
    let system_prompt_for_obs = system_prompt;
    // Extract an optional `approval_token=<value>` field
    // from the request args. The token presence flips
    // `RequiresApproval` plans to `Approved` so an operator
    // who has already decided "yes, run the plan" can resume
    // via the same `ai.chat` call. A future commit will
    // validate the token against the coordinator's approval
    // registry; today its presence is the operator's
    // co-signed signal.
    let approval_token = extract_approval_token(s);
    // RELIX-7.15: snapshot fields used by the training record
    // BEFORE we move `input` into `generate_reply`. Bound here
    // so both the Ok branch and the Err branches can emit a
    // record with the same context.
    let training_system_prompt = input.system_prompt.clone().unwrap_or_default();
    let training_user_message = input.prompt.clone();
    let training_session = session_id.to_string();
    let training_agent = ctx.caller.name.clone();
    // `input.model` already holds the resolved model — the
    // upstream code populated it from either the caller's
    // override or this node's default. Use it as the fallback
    // for the training record when the provider doesn't echo
    // back a model.
    let training_model_default = input.model.clone();
    // RELIX-7.29 PART 2: clone the input template BEFORE the
    // baseline call consumes it. Used as the seed for the N
    // parallel SC samples when SC fires for this method.
    let sc_template = ChatInput {
        session_id: input.session_id.clone(),
        prompt: input.prompt.clone(),
        history: input.history.clone(),
        model: input.model.clone(),
        system_prompt: input.system_prompt.clone(),
        ..ChatInput::default()
    };
    match provider.generate_reply(input).await {
        Ok(mut output) => {
            // RELIX-7.11: hand the provider-reported token usage
            // to the metrics collector. The dispatch bridge
            // records the metric row AFTER this handler returns;
            // the collector's join cache pulls this hint out by
            // request_id and merges it into the row before
            // persisting. Silently no-op when no provider usage
            // is available (mock provider, providers that don't
            // ship usage on errors), or when no metrics sink is
            // wired.
            if let (Some(sink), Some(usage)) = (metrics_sink.as_ref(), output.usage.as_ref()) {
                sink.attach_ai_usage(crate::metrics::AiUsageHint {
                    request_id: ctx.request_id,
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    model: output.model.clone(),
                    routing_tier: routing_tier_label.map(|s| s.to_string()),
                });
            } else if let Some(sink) = metrics_sink.as_ref()
                && let Some(label) = routing_tier_label
            {
                // RELIX-7.29 PART 1: even when the provider
                // didn't ship a usage row, attach the routing tier
                // so the dispatch metric still records WHICH tier
                // serviced the call.
                sink.attach_ai_usage(crate::metrics::AiUsageHint {
                    request_id: ctx.request_id,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    model: String::new(),
                    routing_tier: Some(label.to_string()),
                });
            }
            // RELIX-7.19 GAP 3: side-channel provider signals
            // (finish_reason + logprob) keyed by request_id so
            // the dispatch bridge's ConfidenceScorer can read
            // them without parsing the response body.
            if let Some(sink) = metrics_sink.as_ref()
                && (output.finish_reason.is_some() || output.logprob.is_some())
            {
                sink.attach_provider_signals(crate::metrics::AiProviderSignalsHint {
                    request_id: ctx.request_id,
                    finish_reason: output.finish_reason.clone(),
                    logprob: output.logprob,
                });
            }
            // RELIX-7.29 PART 2: adaptive self-consistency. Fires
            // only when:
            //   1) `[confidence.self_consistency] enabled = true`,
            //   2) the capability matches `capability_patterns`,
            //   3) the baseline confidence (cheap length + coherence
            //      heuristic) is BELOW `min_score_to_enable`.
            // When it fires, the handler:
            //   * runs `sample_count - 1` extra `generate_reply`s
            //     in parallel,
            //   * embeds each sample's "core answer",
            //   * averages pairwise cosine into the SC score,
            //   * REPLACES the response body with the highest-
            //     coherence sample (spec),
            //   * attaches an [`AiSelfConsistencyHint`] so the
            //     dispatch bridge's `ConfidenceScorer` substitutes
            //     the score for `provider_signal`.
            //
            // PART 3: cost guards. Three new gates run BEFORE the
            // SC fire decision and propagate to the judge + belief
            // stages further down:
            //   * `is_disabled` — a prior guard trip has SC paused.
            //   * `per_request_budget_usd` — estimated SC + judge +
            //     belief cost > config cap; cascade-skip all three.
            //   * after SC runs, record the decision in the rolling
            //     ring and the cost in the rolling-hour window so
            //     subsequent requests see updated guard state.
            let now_unix_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let baseline_prompt_tokens =
                output.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0);
            let baseline_completion_tokens = output
                .usage
                .as_ref()
                .map(|u| u.completion_tokens)
                .unwrap_or(0);
            let model_for_cost = if output.model.is_empty() {
                default_model.as_str()
            } else {
                output.model.as_str()
            };
            let estimated_optional_cost_usd = sc_stats.estimate_optional_cost_usd(
                baseline_prompt_tokens,
                baseline_completion_tokens,
                model_for_cost,
                sc_cfg.sample_count.max(1),
            );
            let cost_budget_exceeded = estimated_optional_cost_usd
                .map(|c| c > sc_cfg.per_request_budget_usd)
                .unwrap_or(false);
            let sc_disabled_by_guard = sc_stats.is_disabled(now_unix_secs);
            if cost_budget_exceeded {
                tracing::warn!(
                    session_id,
                    estimated_optional_cost_usd,
                    per_request_budget_usd = sc_cfg.per_request_budget_usd,
                    "ai.chat: per-request cost budget exceeded; skipping SC + judge + belief"
                );
            }
            if sc_cfg.matches_capability("ai.chat")
                && !cost_budget_exceeded
                && !sc_disabled_by_guard
                && sc_cfg.should_trigger(baseline_confidence(&output.text))
            {
                let baseline_text = output.text.clone();
                if let Some((outcome, sample_texts)) =
                    run_self_consistency(&provider, baseline_text, sc_template.clone(), &sc_cfg)
                        .await
                {
                    let best_index = outcome.best_index.min(sample_texts.len() - 1);
                    if best_index != 0 {
                        // Swap to the highest-coherence sample.
                        output.text = sample_texts[best_index].clone();
                    }
                    if let Some(sink) = metrics_sink.as_ref() {
                        sink.attach_self_consistency(crate::metrics::AiSelfConsistencyHint {
                            request_id: ctx.request_id,
                            score: outcome.score,
                            sample_count: outcome.samples.len() as u32,
                            best_sample_index: best_index as u32,
                        });
                    }
                    sc_stats.record(outcome.score, outcome.samples.len());
                    sc_stats.record_decision(true, now_unix_secs, &sc_cfg);
                    // SC cost ≈ (sample_count - 1) extra calls
                    // priced like the baseline call.
                    if let Some(base_unit) = sc_stats.estimate_optional_cost_usd(
                        baseline_prompt_tokens,
                        baseline_completion_tokens,
                        model_for_cost,
                        2, // ⇒ one extra unit
                    ) {
                        // `estimate_optional_cost_usd(_, _, _, 2)`
                        // returns `1 + 2 = 3` baseline units; we
                        // want `(sample_count - 1)` of those units.
                        let per_unit = base_unit / 3.0;
                        let sc_cost = per_unit * outcome.samples.len().saturating_sub(1) as f64;
                        sc_stats.record_sc_cost_usd(sc_cost, now_unix_secs, &sc_cfg);
                    }
                    tracing::info!(
                        session_id,
                        sc_score = outcome.score,
                        sample_count = outcome.samples.len(),
                        best_sample = best_index,
                        "ai.chat: self-consistency outcome recorded"
                    );
                }
            } else if sc_cfg.matches_capability("ai.chat") {
                // PART 3: even when we DON'T fire SC (high
                // baseline confidence, cost-budget exceeded, or
                // gate disabled), record the decision so the
                // rolling trigger-rate window reflects ground
                // truth.
                sc_stats.record_decision(false, now_unix_secs, &sc_cfg);
            }
            // RELIX-7.29 PART 3: non-blocking belief update. The
            // belief tracker reads existing beliefs for
            // (subject_id, session_id), spawns a task that calls
            // the belief model with the structured update prompt,
            // parses the JSON response, and writes the new
            // belief list back to the tracker. The spawned task
            // is fire-and-forget so the caller never waits on
            // it; any failure (provider error, JSON parse) logs
            // a warning and leaves the previous beliefs intact.
            // RELIX-7.29 PART 4: judge model. Activation gate
            // requires ALL of:
            //   * `[ai.judge] enabled = true`
            //   * final confidence < `judge_threshold`
            //   * response has tool call OR structured marker
            //   * session has ≥ 2 prior turns
            // When the gate fires, we dispatch a second
            // provider call against `judge_model_name` capped
            // at `max_judge_latency_ms` and record the verdict
            // in the recorder. A timeout produces a synthetic
            // `proceed` verdict so the handler never stalls.
            let prior_turns = judge_turns.bump(session_id);
            let chat_confidence = baseline_confidence(&output.text);
            // PART 3: per-request cost guard. When the cost
            // estimate exceeded `per_request_budget_usd` above
            // we cascade-skip judge after SC.
            if !cost_budget_exceeded
                && judge::should_invoke(
                    &judge_runtime_cfg,
                    chat_confidence,
                    &output.text,
                    prior_turns,
                )
            {
                let judge_input = judge::build_judge_input(
                    &judge_runtime_cfg,
                    session_id,
                    &training_user_message,
                    &output.text,
                );
                let judge_provider = provider.clone();
                let timeout = judge::timeout_for(&judge_runtime_cfg);
                let judged =
                    tokio::time::timeout(timeout, judge_provider.generate_reply(judge_input)).await;
                let (verdict, timed_out) = match judged {
                    Ok(Ok(reply)) => match judge::parse_judge_response(&reply.text) {
                        Ok(v) => (v, false),
                        Err(e) => {
                            tracing::warn!(
                                session_id,
                                error = %e,
                                "ai.chat: judge response parse failed; falling back to proceed"
                            );
                            (judge::JudgeVerdict::proceed_default("parse-failed"), false)
                        }
                    },
                    Ok(Err(e)) => {
                        tracing::warn!(
                            session_id,
                            error = %e,
                            "ai.chat: judge call failed; falling back to proceed"
                        );
                        (
                            judge::JudgeVerdict::proceed_default("provider-error"),
                            false,
                        )
                    }
                    Err(_) => {
                        tracing::warn!(
                            session_id,
                            timeout_ms = judge_runtime_cfg.max_judge_latency_ms,
                            "ai.chat: judge call timed out; falling back to proceed"
                        );
                        (judge::JudgeVerdict::proceed_default("timeout"), true)
                    }
                };
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let block_decision = matches!(verdict.verdict, judge::JudgeAction::Block);
                let modify_decision = matches!(verdict.verdict, judge::JudgeAction::Modify);
                judge_recorder.record(judge::VerdictRecord {
                    agent: ctx.caller.name.clone(),
                    session_id: session_id.to_string(),
                    timestamp_ms: now_ms,
                    final_confidence: chat_confidence,
                    timed_out,
                    verdict: verdict.clone(),
                });
                tracing::info!(
                    session_id,
                    verdict = verdict.verdict.as_str(),
                    timed_out,
                    "ai.chat: judge verdict recorded"
                );
                if block_decision {
                    return HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::POLICY_DENIED,
                        cause: format!(
                            "ai.chat: judge blocked response (factual_errors={:?})",
                            verdict.factual_errors
                        ),
                        retry_hint: 0,
                        retry_after: None,
                    });
                }
                if modify_decision {
                    let mut combined = output.text;
                    combined.push_str("\n\n[Judge: please revise — ");
                    if verdict.factual_errors.is_empty() {
                        combined.push_str("see verdict log");
                    } else {
                        combined.push_str(&verdict.factual_errors.join("; "));
                    }
                    combined.push(']');
                    output.text = combined;
                }
            }
            // PART 3: per-request cost guard. Skip belief update
            // when the budget has been exceeded; otherwise honour
            // the existing `belief_tracker.enabled()` gate.
            if !cost_budget_exceeded && belief_tracker.enabled() {
                let provider_for_belief = provider.clone();
                let tracker_for_belief = belief_tracker.clone();
                let subject_for_belief = belief_subject.clone();
                let session_for_belief = session_id.to_string();
                let user_msg_for_belief = training_user_message.clone();
                let assistant_for_belief = output.text.clone();
                tokio::spawn(async move {
                    let existing = tracker_for_belief.get(&subject_for_belief, &session_for_belief);
                    let update_input = belief_state::build_update_input(
                        tracker_for_belief.config(),
                        &session_for_belief,
                        &existing,
                        &user_msg_for_belief,
                        &assistant_for_belief,
                    );
                    match provider_for_belief.generate_reply(update_input).await {
                        Ok(reply) => match belief_state::parse_update_response(&reply.text) {
                            Ok(items) => {
                                tracker_for_belief.set(
                                    &subject_for_belief,
                                    &session_for_belief,
                                    items,
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    session_id = %session_for_belief,
                                    error = %e,
                                    "ai.chat: belief update parse failed; keeping previous beliefs"
                                );
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                session_id = %session_for_belief,
                                error = %e,
                                "ai.chat: belief model call failed; keeping previous beliefs"
                            );
                        }
                    }
                });
            }
            let plan = execution::Planner::parse_response(&output.text);
            let policy = execution::PolicyEngine::default_policy();
            let initial_verdict = policy.evaluate(&plan);
            let verdict = match (&initial_verdict, approval_token.as_deref()) {
                (execution::PolicyVerdict::RequiresApproval { .. }, Some(_)) => {
                    execution::PolicyVerdict::Approved
                }
                _ => initial_verdict,
            };
            let approved_by = approval_token
                .as_deref()
                .map(|t| format!("approval_token:{}", t.chars().take(16).collect::<String>()));
            // Walk the parsed plan step-by-step. ModelCall
            // steps record the provider's raw reply. ToolCall
            // steps route through the per-controller
            // ToolDispatcher (broker → secret resolve →
            // handler → output guard → gateway record). When
            // the dispatcher denies a step the runner returns
            // a JSON-shaped error and the chat response gains
            // a `[tool-dispatch-errors]` trailer so the caller
            // sees a structured error rather than a silent
            // drop. Memory / approval step kinds are skipped
            // here; they're handled elsewhere in the pipeline.
            let mut state = execution::ExecutionState::new(plan.clone());
            let tool_step_results: Vec<execution::StepResult> = match tool_dispatcher.as_ref() {
                Some(disp) => {
                    let mesh = tool_mesh.get().cloned();
                    execution::dispatch_planner_tool_calls(disp, &ctx.caller.name, &plan, mesh)
                        .await
                }
                None => Vec::new(),
            };
            let mut tool_iter = tool_step_results.iter();
            let mut model_recorded = false;
            let mut tool_dispatch_errors: Vec<String> = Vec::new();
            for step in plan.steps.iter() {
                let result = match step {
                    execution::PlanStep::ModelCall { .. } => {
                        if !model_recorded {
                            model_recorded = true;
                            execution::StepResult::Ok {
                                output: output.text.clone(),
                            }
                        } else {
                            execution::StepResult::Skipped {
                                reason: "subsequent ModelCall steps are not executed in this turn"
                                    .to_string(),
                            }
                        }
                    }
                    execution::PlanStep::ToolCall { .. } => match tool_iter.next() {
                        Some(r) => r.clone(),
                        None => execution::StepResult::Skipped {
                            reason: "no tool dispatcher configured for this AI controller"
                                .to_string(),
                        },
                    },
                    execution::PlanStep::MemoryRead { .. }
                    | execution::PlanStep::MemoryWrite { .. } => execution::StepResult::Skipped {
                        reason: "memory step not yet wired into ai.chat handler".to_string(),
                    },
                    execution::PlanStep::HumanApproval { .. } => execution::StepResult::Skipped {
                        reason: "human approval handled via policy verdict".to_string(),
                    },
                };
                if let execution::StepResult::Err { reason } = &result {
                    tool_dispatch_errors.push(reason.clone());
                }
                execution::Executor::advance(&mut state, result);
            }
            let evidence = execution::EvidenceRecord::from_state(
                &state,
                session_id,
                session_id,
                approved_by.clone(),
            );
            tracing::info!(
                session_id,
                evidence = %evidence.to_json(),
                "ai.chat: execution evidence captured"
            );
            // Compose the response body: provider reply +
            // (optionally) a trailer that lists every
            // dispatcher-rejected tool call as JSON. Callers
            // get a structured error per failed step rather
            // than a silent drop.
            let body_text = if tool_dispatch_errors.is_empty() {
                output.text.clone()
            } else {
                let mut b = output.text.clone();
                if !b.is_empty() && !b.ends_with('\n') {
                    b.push('\n');
                }
                b.push_str("\n[tool-dispatch-errors]\n");
                for err in &tool_dispatch_errors {
                    b.push_str(err);
                    b.push('\n');
                }
                b
            };
            // RELIX-7.15: build & emit one training record from
            // the planner's tool-call results + the provider's
            // response text + token usage. Tool calls reflect
            // dispatcher outcomes (success/failure + step
            // latency) so the operator gets full agent
            // observability per turn.
            let tool_records: Vec<crate::training::ToolCallRecord> = tool_step_results
                .iter()
                .filter_map(|sr| match sr {
                    execution::StepResult::Ok { output: out } => {
                        Some(crate::training::ToolCallRecord {
                            tool: "tool.invoke".into(),
                            input: String::new(),
                            output: out.clone(),
                            success: true,
                            latency_ms: 0,
                            error_kind: None,
                        })
                    }
                    execution::StepResult::Err { reason } => {
                        Some(crate::training::ToolCallRecord {
                            tool: "tool.invoke".into(),
                            input: String::new(),
                            output: String::new(),
                            success: false,
                            latency_ms: 0,
                            error_kind: Some(reason.clone()),
                        })
                    }
                    execution::StepResult::Skipped { .. } => None,
                })
                .collect();
            let (prompt_tokens, completion_tokens, model_used) = match output.usage.as_ref() {
                Some(u) => (
                    Some(u.prompt_tokens),
                    Some(u.completion_tokens),
                    output.model.clone(),
                ),
                None => (None, None, output.model.clone()),
            };
            let inner_outcome = match verdict {
                execution::PolicyVerdict::Approved => {
                    HandlerOutcome::Ok(body_text.clone().into_bytes())
                }
                execution::PolicyVerdict::RequiresApproval { reason } => {
                    let body = format!("[approval-required] {reason}\n{body_text}");
                    tracing::info!(
                        session_id,
                        reason = %reason,
                        steps = plan.steps.len(),
                        "ai.chat: plan requires approval"
                    );
                    HandlerOutcome::Ok(body.into_bytes())
                }
                execution::PolicyVerdict::Denied { reason } => {
                    tracing::warn!(
                        session_id,
                        reason = %reason,
                        "ai.chat: plan denied by policy"
                    );
                    HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::SECURITY_DENIED,
                        cause: format!("ai.chat: policy denied plan ({reason})"),
                        retry_hint: 0,
                        retry_after: None,
                    })
                }
            };
            let success = matches!(inner_outcome, HandlerOutcome::Ok(_));
            let duration_secs = chat_started_at.elapsed().as_secs() as i64;
            if let Some(sink) = interaction_sink.as_ref() {
                let error_kind = match &inner_outcome {
                    HandlerOutcome::Err(env) => {
                        Some(crate::dispatch::error_kind_to_str(env.kind).to_string())
                    }
                    _ => None,
                };
                let rec = crate::training::InteractionRecord::new(
                    crate::training::InteractionId::from_request(&ctx.request_id),
                    training_session.clone(),
                    training_agent.clone(),
                    model_used.clone(),
                    provider_name.clone(),
                    training_system_prompt.clone(),
                    training_user_message.clone(),
                    output.text.clone(),
                    tool_records.clone(),
                    prompt_tokens,
                    completion_tokens,
                    chat_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    success,
                    error_kind,
                    0,
                );
                sink.record_interaction(rec);
            }
            // GAP 4: spawn the skill extractor on successful
            // completions. Non-blocking — the spawn future
            // returns the inner_outcome immediately and the
            // extractor runs on its own tokio task.
            if let Some(extractor) = skill_extractor.as_ref()
                && success
            {
                let tool_call_names: Vec<String> = tool_records
                    .iter()
                    .filter(|r| r.success)
                    .map(|r| r.tool.clone())
                    .collect();
                let response_text = output.text.clone();
                let response_word_count = response_text.split_whitespace().count();
                let asked_for_structured = skill_extractor::detect_structured_output(prompt);
                let task = skill_extractor::TaskCompletion {
                    session_id: session_id.to_string(),
                    agent_name: ctx.caller.name.clone(),
                    prompt: prompt.to_string(),
                    response: response_text,
                    response_word_count,
                    tool_calls: tool_call_names,
                    asked_for_structured_output: asked_for_structured,
                    duration_secs,
                    // Session-turn counting is approximate at
                    // the handler boundary — `history` carries
                    // the conversation so far and `\n\n`
                    // separated turns is the documented wire
                    // shape downstream of `memory.recent_for_session`.
                    session_turns: history.lines().filter(|l| !l.trim().is_empty()).count(),
                    success,
                };
                let ex = extractor.clone();
                // Detached fire-and-forget; the spawned task
                // owns its own JoinHandle and will run to
                // completion on its own. `drop` is the explicit
                // form clippy's `let_underscore_future` lint
                // is happy with.
                drop(ex.spawn(task));
            }
            // GAP 13 + 14: post-flight provenance + observability
            // metadata write. Runs after the response is built
            // so a wedge here doesn't delay the caller; it WILL
            // delay the handler's tokio task, but the work is a
            // few SQLite inserts so the cost is bounded.
            if let Some(obs) = observability.as_ref() {
                let trace_id = format!("{:032x}", u128::from_le_bytes(ctx.trace_id.0));
                let system_hash = system_prompt_for_obs
                    .as_deref()
                    .map(crate::nodes::ai::provenance_hooks::hash_blake3)
                    .unwrap_or_default();
                crate::nodes::ai::provenance_hooks::record_chat_provenance(
                    obs.as_ref(),
                    session_id,
                    &trace_id,
                    &model_used,
                    &system_hash,
                    Some(&ctx.caller.name),
                );
                let duration_ms =
                    chat_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
                crate::nodes::ai::provenance_hooks::record_chat_metadata(
                    obs.as_ref(),
                    session_id,
                    &trace_id,
                    &ctx.caller.name,
                    "ai.chat.complete",
                    &model_used,
                    duration_ms,
                    prompt_tokens.map(|p| p as u64 + completion_tokens.unwrap_or(0) as u64),
                    success,
                );
            }
            inner_outcome
        }
        Err(ProviderError::Transient(c)) => {
            let env = ErrorEnvelope {
                kind: error_kinds::RESPONDER_OVERLOADED,
                cause: format!("ai.chat: {c}"),
                retry_hint: 1,
                retry_after: None,
            };
            if let Some(sink) = interaction_sink.as_ref() {
                let rec = crate::training::InteractionRecord::new(
                    crate::training::InteractionId::from_request(&ctx.request_id),
                    training_session.clone(),
                    training_agent.clone(),
                    training_model_default.clone(),
                    provider_name.clone(),
                    training_system_prompt.clone(),
                    training_user_message.clone(),
                    String::new(),
                    vec![],
                    None,
                    None,
                    chat_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    false,
                    Some(crate::dispatch::error_kind_to_str(env.kind).to_string()),
                    0,
                );
                sink.record_interaction(rec);
            }
            HandlerOutcome::Err(env)
        }
        Err(ProviderError::Permanent(c)) => {
            let env = ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("ai.chat: {c}"),
                retry_hint: 2,
                retry_after: None,
            };
            if let Some(sink) = interaction_sink.as_ref() {
                let rec = crate::training::InteractionRecord::new(
                    crate::training::InteractionId::from_request(&ctx.request_id),
                    training_session.clone(),
                    training_agent.clone(),
                    training_model_default.clone(),
                    provider_name.clone(),
                    training_system_prompt.clone(),
                    training_user_message.clone(),
                    String::new(),
                    vec![],
                    None,
                    None,
                    chat_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    false,
                    Some(crate::dispatch::error_kind_to_str(env.kind).to_string()),
                    0,
                );
                sink.record_interaction(rec);
            }
            HandlerOutcome::Err(env)
        }
    }
}

/// RELIX-7.29 PART 2: dispatch N parallel `generate_reply`
/// calls with the *same* prompt, embed each sample's core
/// answer, and return the [`crate::confidence::SelfConsistencyOutcome`].
///
/// The caller pre-supplies sample 0 (the baseline already
/// produced by `handle_chat`). This function adds
/// `sample_count.saturating_sub(1)` more parallel samples,
/// embeds all N answers in a single batched call, and runs
/// the pairwise evaluation.
///
/// Failures inside the join are tolerated: if any sample
/// fails, it's dropped from the evaluation. With fewer than
/// two surviving samples the function returns `None` —
/// caller treats that as "SC could not run" and proceeds
/// with the baseline.
async fn run_self_consistency(
    provider: &Arc<dyn ChatProvider>,
    baseline_text: String,
    baseline_input_template: ChatInput,
    sc_cfg: &crate::confidence::SelfConsistencyConfig,
) -> Option<(crate::confidence::SelfConsistencyOutcome, Vec<String>)> {
    let extra = sc_cfg.sample_count.saturating_sub(1);
    if extra == 0 {
        return None;
    }
    let mut tasks: Vec<_> = Vec::with_capacity(extra);
    for _ in 0..extra {
        let p = provider.clone();
        let input = ChatInput {
            session_id: baseline_input_template.session_id.clone(),
            prompt: baseline_input_template.prompt.clone(),
            history: baseline_input_template.history.clone(),
            model: baseline_input_template.model.clone(),
            system_prompt: baseline_input_template.system_prompt.clone(),
            ..ChatInput::default()
        };
        tasks.push(tokio::spawn(async move {
            p.generate_reply(input).await.ok().map(|o| o.text)
        }));
    }
    let mut sample_texts: Vec<String> = Vec::with_capacity(sc_cfg.sample_count);
    sample_texts.push(baseline_text);
    for jh in tasks {
        if let Ok(Some(text)) = jh.await {
            sample_texts.push(text);
        }
    }
    if sample_texts.len() < 2 {
        return None;
    }
    let core_answers: Vec<String> = sample_texts
        .iter()
        .map(|t| crate::confidence::extract_core_answer(t, 100))
        .collect();
    let embed_input = EmbedInput {
        model: String::new(),
        texts: core_answers.clone(),
    };
    let embed_output = match provider.generate_embeddings(embed_input).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ai.chat: self-consistency embed failed; skipping SC"
            );
            return None;
        }
    };
    if embed_output.vectors.len() != core_answers.len() {
        tracing::warn!(
            got = embed_output.vectors.len(),
            want = core_answers.len(),
            "ai.chat: embed vector count mismatch; skipping SC"
        );
        return None;
    }
    let outcome = crate::confidence::evaluate_samples(&core_answers, &embed_output.vectors);
    Some((outcome, sample_texts))
}

/// RELIX-7.29 (post-rebuild) — self-consistency for the
/// streaming path. Runs N unary `generate_reply` calls in
/// parallel via `tokio::spawn`, embeds each sample's core
/// answer, scores them, and returns the winning text +
/// outcome + aggregated token counts.
///
/// Returns `None` when fewer than two samples survive (the
/// caller falls back to the normal streaming path), the embed
/// step fails, or the vector count doesn't line up.
async fn run_self_consistency_stream(
    provider: &Arc<dyn ChatProvider>,
    template: ChatInput,
    sc_cfg: &crate::confidence::SelfConsistencyConfig,
) -> Option<StreamSelfConsistencyResult> {
    let n = sc_cfg.sample_count.max(2);
    let mut tasks: Vec<_> = Vec::with_capacity(n);
    for _ in 0..n {
        let p = provider.clone();
        let input = ChatInput {
            session_id: template.session_id.clone(),
            prompt: template.prompt.clone(),
            history: template.history.clone(),
            model: template.model.clone(),
            system_prompt: template.system_prompt.clone(),
            ..ChatInput::default()
        };
        tasks.push(tokio::spawn(
            async move { p.generate_reply(input).await.ok() },
        ));
    }
    let mut sample_texts: Vec<String> = Vec::with_capacity(n);
    let mut total_prompt: u64 = 0;
    let mut total_completion: u64 = 0;
    let mut model_used: String = template.model.clone();
    for jh in tasks {
        if let Ok(Some(output)) = jh.await {
            if let Some(u) = output.usage.as_ref() {
                total_prompt = total_prompt.saturating_add(u.prompt_tokens as u64);
                total_completion = total_completion.saturating_add(u.completion_tokens as u64);
            }
            if !output.model.is_empty() {
                model_used = output.model.clone();
            }
            sample_texts.push(output.text);
        }
    }
    if sample_texts.len() < 2 {
        return None;
    }
    let core_answers: Vec<String> = sample_texts
        .iter()
        .map(|t| crate::confidence::extract_core_answer(t, 100))
        .collect();
    let embed_input = EmbedInput {
        model: String::new(),
        texts: core_answers.clone(),
    };
    let embed_output = match provider.generate_embeddings(embed_input).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ai.chat.stream: self-consistency embed failed; falling back to normal stream"
            );
            return None;
        }
    };
    if embed_output.vectors.len() != core_answers.len() {
        tracing::warn!(
            got = embed_output.vectors.len(),
            want = core_answers.len(),
            "ai.chat.stream: embed vector count mismatch; falling back to normal stream"
        );
        return None;
    }
    let outcome = crate::confidence::evaluate_samples(&core_answers, &embed_output.vectors);
    let best_index = outcome.best_index.min(sample_texts.len() - 1);
    let winner = sample_texts[best_index].clone();
    Some(StreamSelfConsistencyResult {
        outcome,
        winner_text: winner,
        best_index,
        total_prompt_tokens: total_prompt,
        total_completion_tokens: total_completion,
        model: model_used,
    })
}

/// Bundle returned by [`run_self_consistency_stream`] —
/// surfaces both the SC outcome and the aggregated usage so
/// the streaming handler can attach a single AiUsageHint with
/// the total token cost of all N samples.
struct StreamSelfConsistencyResult {
    outcome: crate::confidence::SelfConsistencyOutcome,
    winner_text: String,
    best_index: usize,
    total_prompt_tokens: u64,
    total_completion_tokens: u64,
    model: String,
}

/// RELIX-7.29 (post-rebuild) — chunk a winning self-
/// consistency response into wire-sized frames so the caller
/// receives a streaming experience even though the underlying
/// SC pipeline runs unary. Yields one chunk per
/// whitespace-separated token plus the trailing whitespace.
/// `into_bytes` is fine because we control the source string
/// and never split inside a UTF-8 code point.
pub(crate) fn chunk_for_stream(text: &str) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut acc = String::new();
    for ch in text.chars() {
        acc.push(ch);
        if ch.is_whitespace() {
            out.push(std::mem::take(&mut acc).into_bytes());
        }
    }
    if !acc.is_empty() {
        out.push(acc.into_bytes());
    }
    out
}

/// RELIX-7.29 PART 2: cheap baseline confidence approximation
/// used to gate the adaptive SC trigger. Mirrors the scorer's
/// `response_length` × `response_coherence` shape but skips
/// the rolling-window history (we haven't recorded this call
/// yet). The result is in `[0, 1]`.
fn baseline_confidence(text: &str) -> f32 {
    let len = response_length_for_sc(text);
    let coh = response_coherence_for_sc(text);
    ((len * 0.45) + (coh * 0.55)).clamp(0.0, 1.0)
}

/// Simplified copy of the scorer's `response_length_score` —
/// kept inline so the SC trigger doesn't take the scorer's
/// lock or pay the trait-object overhead. The exact ramp shape
/// matches the scorer's so the trigger threshold stays
/// meaningful.
fn response_length_for_sc(text: &str) -> f32 {
    let chars = text.trim().chars().count();
    if chars == 0 {
        return 0.0;
    }
    let est_tokens = (chars / 4).max(1);
    if est_tokens < 10 {
        return 0.3 + 0.7 * (est_tokens as f32 / 10.0);
    }
    if est_tokens <= 500 {
        return 1.0;
    }
    if est_tokens >= 2000 {
        return 0.7;
    }
    let frac = (est_tokens as f32 - 500.0) / (2000.0 - 500.0);
    1.0 - 0.3 * frac
}

/// Simplified copy of the scorer's `response_coherence_score`
/// for the SC trigger. Two heuristics: ends-with-punctuation +
/// repeated-trigram-ratio penalty.
fn response_coherence_for_sc(text: &str) -> f32 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    let ends_well = trimmed
        .chars()
        .last()
        .map(|c| matches!(c, '.' | '!' | '?'))
        .unwrap_or(false);
    let punct_bonus: f32 = if ends_well { 0.0 } else { -0.2 };
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 3 {
        return (1.0 + punct_bonus).clamp(0.0, 1.0);
    }
    let mut trigrams = std::collections::HashMap::<(String, String, String), u32>::new();
    for w in tokens.windows(3) {
        let k = (w[0].to_string(), w[1].to_string(), w[2].to_string());
        *trigrams.entry(k).or_insert(0) += 1;
    }
    let total: u32 = trigrams.values().sum();
    let repeated: u32 = trigrams.values().filter(|c| **c > 1).sum();
    let ratio = if total > 0 {
        repeated as f32 / total as f32
    } else {
        0.0
    };
    (1.0 - ratio + punct_bonus).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    // ── SEC PART 5: JSON-first arg parsing ────────────────

    #[test]
    fn parse_ai_chat_args_pipe_form_round_trips() {
        let (sid, p, h) = parse_ai_chat_args(b"sess-1|hello world|prior turns").unwrap();
        assert_eq!(sid, "sess-1");
        assert_eq!(p, "hello world");
        assert_eq!(h, "prior turns");
    }

    #[test]
    fn parse_ai_chat_args_json_form_round_trips() {
        let body = br#"{"session_id":"sess-1","prompt":"hello","history":"prior"}"#;
        let (sid, p, h) = parse_ai_chat_args(body).unwrap();
        assert_eq!(sid, "sess-1");
        assert_eq!(p, "hello");
        assert_eq!(h, "prior");
    }

    #[test]
    fn pipe_in_session_id_corrupts_pipe_form_but_json_is_clean() {
        // Pre-fix: pipe-delimited form puts the `|` inside
        // session_id into the prompt position. JSON args
        // are unambiguous.
        let (sid_pipe, p_pipe, _) = parse_ai_chat_args(b"sess|pwned|hello").unwrap();
        assert_eq!(sid_pipe, "sess");
        assert_eq!(p_pipe, "pwned");
        let body = br#"{"session_id":"sess|pwned","prompt":"hello"}"#;
        let (sid_json, p_json, _) = parse_ai_chat_args(body).unwrap();
        assert_eq!(sid_json, "sess|pwned");
        assert_eq!(p_json, "hello");
    }

    #[test]
    fn parse_ai_chat_args_rejects_empty_session_id() {
        let err = parse_ai_chat_args(b"|prompt").unwrap_err();
        assert!(err.cause.contains("session_id required"));
        let err = parse_ai_chat_args(br#"{"session_id":"","prompt":"x"}"#).unwrap_err();
        assert!(err.cause.contains("session_id required"));
    }

    fn ctx(args: &[u8]) -> InvocationCtx {
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"alice"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["chat-users".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
            tenant_id: None,
        }
    }

    fn empty_mem() -> Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> {
        Arc::new(tokio::sync::OnceCell::new())
    }

    /// A canned MemoryFetcher used to exercise the injection
    /// path without a live mesh.
    struct StubFetcher {
        agent: String,
        user: String,
    }

    #[async_trait::async_trait]
    impl MemoryFetcher for StubFetcher {
        async fn fetch(&self, _subject_id: &str) -> Option<(String, String)> {
            Some((self.agent.clone(), self.user.clone()))
        }
    }

    /// A MemoryFetcher that always reports unavailable. Models
    /// the "memory node unreachable" silent-skip path.
    struct UnavailableFetcher;

    #[async_trait::async_trait]
    impl MemoryFetcher for UnavailableFetcher {
        async fn fetch(&self, _subject_id: &str) -> Option<(String, String)> {
            None
        }
    }

    /// A ChatProvider that records the last ChatInput it saw so
    /// tests can verify what was sent to the provider.
    struct RecordingProvider {
        last: Arc<std::sync::Mutex<Option<ChatInput>>>,
    }

    #[async_trait::async_trait]
    impl ChatProvider for RecordingProvider {
        async fn generate_reply(
            &self,
            input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            *self.last.lock().unwrap() = Some(input.clone());
            Ok(provider::ChatOutput {
                text: "recorded".to_string(),
                provider: "recording",
                model: input.model.clone(),
                usage: None,
                finish_reason: None,
                logprob: None,
            })
        }
        fn provider_name(&self) -> &'static str {
            "recording"
        }
        /// Tests that drive the RAG path need an embed-capable
        /// provider — return a fixed 4-dim vector per input so
        /// fetch_rag sees a non-empty `query_vec`.
        async fn generate_embeddings(
            &self,
            input: provider::EmbedInput,
        ) -> Result<provider::EmbedOutput, ProviderError> {
            let vectors = input
                .texts
                .iter()
                .map(|_| vec![0.1f32, 0.2, 0.3, 0.4])
                .collect();
            Ok(provider::EmbedOutput {
                model: "recording-embed".into(),
                vectors,
            })
        }
    }

    #[tokio::test]
    async fn mock_provider_is_deterministic_with_and_without_history() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r1 = handle_chat(
            p.clone(),
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hello|"),
        )
        .await;
        let r2 = handle_chat(
            p.clone(),
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hello|"),
        )
        .await;
        match (r1, r2) {
            (HandlerOutcome::Ok(a), HandlerOutcome::Ok(b)) => assert_eq!(a, b),
            _ => panic!("expected both ok"),
        }
        let r3 = handle_chat(
            p,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hello|user: prior\n"),
        )
        .await;
        match r3 {
            HandlerOutcome::Ok(body) => {
                let t = String::from_utf8(body).unwrap();
                assert!(
                    t.contains("history=12 chars"),
                    "expected 'history=12 chars' in: {t}"
                );
            }
            HandlerOutcome::Err(e) => panic!("unexpected error: {}", e.cause),
        }
    }

    #[tokio::test]
    async fn missing_prompt_rejected() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r = handle_chat(
            p,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"only-session-id"),
        )
        .await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    #[tokio::test]
    async fn empty_session_rejected() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r = handle_chat(
            p,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"|hello|"),
        )
        .await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    // ── SOUL.md wiring ──────────────────────────────────────

    /// Make a SoulCache that resolves to a tempfile-backed soul
    /// + return the TempFile guard so the caller controls its
    ///   lifetime.
    fn make_soul_cache_with_tempfile(body: &str) -> (tempfile::NamedTempFile, SoulCache) {
        use std::io::Write;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp.as_file(), "{body}").unwrap();
        let agent = AgentConfig {
            name: String::new(),
            soul_path: Some(tmp.path().to_path_buf()),
        };
        let cache = SoulCache::from_config(Some(&agent));
        (tmp, cache)
    }

    #[tokio::test]
    async fn soul_content_prepended_to_system_prompt_when_cache_resolves() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let (_tmp, soul) = make_soul_cache_with_tempfile("# Alice\nFriendly and concise.");
        let r = handle_chat(
            rec_provider,
            String::new(),
            empty_mem(),
            soul,
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hi|"),
        )
        .await;
        match r {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("unexpected err: {}", e.cause),
        }
        let captured = recorded.lock().unwrap().clone().unwrap();
        let sp = captured.system_prompt.expect("system_prompt set");
        // Soul body lands at the top of the system prompt.
        assert!(sp.contains("# Alice"), "soul header missing: {sp}");
        assert!(
            sp.contains("Friendly and concise."),
            "soul body missing: {sp}"
        );
    }

    #[tokio::test]
    async fn soul_prepends_in_front_of_memory_block() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let stub: Arc<dyn MemoryFetcher> = Arc::new(StubFetcher {
            agent: "AGENT-FACT".into(),
            user: "USER-PREFERENCE".into(),
        });
        cell.set(stub).ok();
        let (_tmp, soul) = make_soul_cache_with_tempfile("SOUL-PERSONA");
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            soul,
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        let sp = captured.system_prompt.expect("system_prompt set");
        // Soul comes first, then memory blocks. Find the
        // indices and assert ordering.
        let soul_pos = sp.find("SOUL-PERSONA").expect("soul body present");
        let agent_pos = sp.find("AGENT-FACT").expect("agent memory present");
        assert!(
            soul_pos < agent_pos,
            "soul must precede memory in prompt: {sp}"
        );
    }

    #[tokio::test]
    async fn soul_cache_no_op_leaves_prompt_unchanged() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let r = handle_chat(
            rec_provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        // No memory + no soul → no system prompt at all.
        assert!(captured.system_prompt.is_none());
    }

    #[tokio::test]
    async fn skill_hint_appended_to_system_prompt_when_match_found() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let skill = skills::Skill {
            name: "deploy-staging".into(),
            path: std::path::PathBuf::from("dummy"),
            body: "# deploy-staging\n\nRun ./scripts/deploy-staging.sh.".into(),
            title: "deploy-staging".into(),
        };
        let cache = skills::SkillMatcher::keyword_only(skills::SkillsCache::from_vec(vec![skill]));
        let guardrail = guardrails::InputGuardrail::permissive();
        let r = handle_chat(
            rec_provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            cache,
            guardrail,
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|please deploy staging now|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        let sp = captured.system_prompt.expect("system_prompt set");
        assert!(
            sp.contains("## Skill: deploy-staging"),
            "skill heading missing: {sp}"
        );
        assert!(
            sp.contains("./scripts/deploy-staging.sh"),
            "skill body missing: {sp}"
        );
    }

    #[tokio::test]
    async fn memory_injection_when_dispatcher_populated_sends_system_prompt() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let stub: Arc<dyn MemoryFetcher> = Arc::new(StubFetcher {
            agent: "rust uses cargo".into(),
            user: "prefers concise replies".into(),
        });
        cell.set(stub).ok();
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hello|"),
        )
        .await;
        match r {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("unexpected err: {}", e.cause),
        }
        let captured = recorded.lock().unwrap().clone().unwrap();
        let sp = captured.system_prompt.expect("system_prompt set");
        assert!(sp.contains("--- AGENT MEMORY ---"));
        assert!(sp.contains("rust uses cargo"));
        assert!(sp.contains("--- USER MEMORY ---"));
        assert!(sp.contains("prefers concise replies"));
        assert!(sp.ends_with("--------------------"));
    }

    #[tokio::test]
    async fn memory_injection_silent_skip_when_dispatcher_unavailable() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let unavail: Arc<dyn MemoryFetcher> = Arc::new(UnavailableFetcher);
        cell.set(unavail).ok();
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hello|"),
        )
        .await;
        match r {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("unexpected err: {}", e.cause),
        }
        // System prompt remains None — memory peer silently
        // skipped, provider received the chat input verbatim.
        let captured = recorded.lock().unwrap().clone().unwrap();
        assert!(captured.system_prompt.is_none());
    }

    /// A canned MemoryFetcher that returns a fixed history block so
    /// tests can verify the auto-history path.
    struct HistoryFetcher {
        history: String,
        last_session_seen: std::sync::Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl MemoryFetcher for HistoryFetcher {
        async fn fetch(&self, _subject_id: &str) -> Option<(String, String)> {
            // Not exercising agent+user memory in these tests.
            None
        }
        async fn fetch_history(&self, session_id: &str) -> Option<String> {
            *self.last_session_seen.lock().unwrap() = Some(session_id.to_string());
            if self.history.is_empty() {
                None
            } else {
                Some(self.history.clone())
            }
        }
    }

    #[tokio::test]
    async fn auto_history_is_injected_into_chat_input() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let stub: Arc<dyn MemoryFetcher> = Arc::new(HistoryFetcher {
            history: "user: prior question\nassistant: prior reply\n".into(),
            last_session_seen: std::sync::Mutex::new(None),
        });
        cell.set(stub).ok();
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess1|new question|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        assert_eq!(captured.session_id, "sess1");
        assert!(
            captured.history.contains("user: prior question"),
            "history not propagated: {:?}",
            captured.history
        );
        assert!(captured.history.contains("assistant: prior reply"));
    }

    #[tokio::test]
    async fn auto_history_merges_with_caller_supplied_history() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let stub: Arc<dyn MemoryFetcher> = Arc::new(HistoryFetcher {
            history: "user: auto-1\nassistant: auto-2\n".into(),
            last_session_seen: std::sync::Mutex::new(None),
        });
        cell.set(stub).ok();
        // Caller-supplied history is the third pipe-delimited
        // field; the merged value should put auto first, caller
        // second.
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess1|q|user: caller-1\n"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        let auto_pos = captured.history.find("user: auto-1").expect("auto present");
        let caller_pos = captured
            .history
            .find("user: caller-1")
            .expect("caller present");
        assert!(
            auto_pos < caller_pos,
            "auto-fetched history must come before caller-supplied: {:?}",
            captured.history
        );
    }

    #[tokio::test]
    async fn auto_history_silently_skipped_when_fetcher_returns_none() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        // HistoryFetcher with empty history returns None — models
        // both "memory peer unreachable" and "session has no
        // turns yet" cases.
        let stub: Arc<dyn MemoryFetcher> = Arc::new(HistoryFetcher {
            history: String::new(),
            last_session_seen: std::sync::Mutex::new(None),
        });
        cell.set(stub).ok();
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess1|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        assert!(
            captured.history.is_empty(),
            "history should be empty on None fetch: {:?}",
            captured.history
        );
    }

    /// A MemoryFetcher that records every call and returns
    /// canned hits / config so RAG paths can be exercised
    /// without a real memory peer. `embedding_seen` is shared
    /// with the test so it can verify whether `fetch_rag` was
    /// invoked without downcasting the Arc<dyn Trait>.
    struct RagFetcher {
        rag_on: bool,
        top_k: usize,
        min_score: f32,
        canned: Option<String>,
        embedding_seen: Arc<std::sync::Mutex<Option<Vec<f32>>>>,
    }

    #[async_trait::async_trait]
    impl MemoryFetcher for RagFetcher {
        async fn fetch(&self, _subject_id: &str) -> Option<(String, String)> {
            None
        }
        async fn fetch_history(&self, _session_id: &str) -> Option<String> {
            None
        }
        fn rag_enabled(&self) -> bool {
            self.rag_on
        }
        fn rag_top_k(&self) -> usize {
            self.top_k
        }
        fn rag_min_score(&self) -> f32 {
            self.min_score
        }
        async fn fetch_rag(
            &self,
            _subject_id: &str,
            embedding: &[f32],
            _top_k: usize,
            _min_score: f32,
        ) -> Option<String> {
            *self.embedding_seen.lock().unwrap() = Some(embedding.to_vec());
            self.canned.clone()
        }
    }

    /// A provider that fails embedding. Used to verify the
    /// "provider doesn't support embeddings" silent-skip path.
    struct NoEmbedProvider {
        last: Arc<std::sync::Mutex<Option<ChatInput>>>,
    }

    #[async_trait::async_trait]
    impl ChatProvider for NoEmbedProvider {
        async fn generate_reply(
            &self,
            input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            *self.last.lock().unwrap() = Some(input.clone());
            Ok(provider::ChatOutput {
                text: "no-embed".to_string(),
                provider: "no-embed",
                model: input.model.clone(),
                usage: None,
                finish_reason: None,
                logprob: None,
            })
        }
        fn provider_name(&self) -> &'static str {
            "no-embed"
        }
        async fn generate_embeddings(
            &self,
            _input: provider::EmbedInput,
        ) -> Result<provider::EmbedOutput, ProviderError> {
            Err(ProviderError::Permanent(
                "no-embed provider does not support embeddings".into(),
            ))
        }
    }

    #[tokio::test]
    async fn rag_block_injected_into_system_prompt_when_enabled() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let stub: Arc<dyn MemoryFetcher> = Arc::new(RagFetcher {
            rag_on: true,
            top_k: 5,
            min_score: 0.7,
            canned: Some(memory_dispatcher::format_rag_block(&[
                memory_dispatcher::RagHit {
                    score: 0.92,
                    target: "agent",
                    chunk: "deadline is Friday".into(),
                },
                memory_dispatcher::RagHit {
                    score: 0.81,
                    target: "user",
                    chunk: "prefers concise replies".into(),
                },
            ])),
            embedding_seen: Arc::new(std::sync::Mutex::new(None)),
        });
        cell.set(stub).ok();
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess1|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        let sp = captured.system_prompt.expect("system_prompt present");
        assert!(
            sp.contains("--- Relevant context from memory ---"),
            "missing RAG header: {sp}"
        );
        assert!(sp.contains("[score: 0.92]"));
        assert!(sp.contains("deadline is Friday"));
    }

    #[tokio::test]
    async fn rag_omitted_when_dispatcher_returns_none() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        // RAG enabled, but every hit is below min_score → dispatcher
        // returns None — modelled by `canned: None` here. Same
        // outcome covers "memory peer unreachable" since both paths
        // resolve to `fetch_rag → None`.
        let stub: Arc<dyn MemoryFetcher> = Arc::new(RagFetcher {
            rag_on: true,
            top_k: 5,
            min_score: 0.99,
            canned: None,
            embedding_seen: Arc::new(std::sync::Mutex::new(None)),
        });
        cell.set(stub).ok();
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess1|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        assert!(
            captured.system_prompt.is_none(),
            "expected no system prompt: {:?}",
            captured.system_prompt
        );
    }

    #[tokio::test]
    async fn rag_silently_skipped_when_provider_lacks_embeddings() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let provider: Arc<dyn ChatProvider> = Arc::new(NoEmbedProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let embedding_seen = Arc::new(std::sync::Mutex::new(None));
        let stub: Arc<dyn MemoryFetcher> = Arc::new(RagFetcher {
            rag_on: true,
            top_k: 5,
            min_score: 0.5,
            // canned would be returned IF fetch_rag were called.
            // The test asserts it is NOT called by checking the
            // recorded embedding is None below.
            canned: Some("--- Relevant context from memory ---\n[score: 0.90] x\n---".into()),
            embedding_seen: embedding_seen.clone(),
        });
        cell.set(stub).ok();
        let r = handle_chat(
            provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess1|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        assert!(
            captured.system_prompt.is_none(),
            "embedding failure must skip RAG entirely: {:?}",
            captured.system_prompt
        );
        assert!(
            embedding_seen.lock().unwrap().is_none(),
            "fetch_rag must not be invoked when embedding fails"
        );
    }

    #[tokio::test]
    async fn rag_disabled_skips_embedding_and_search() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let embedding_seen = Arc::new(std::sync::Mutex::new(None));
        let stub: Arc<dyn MemoryFetcher> = Arc::new(RagFetcher {
            rag_on: false,
            top_k: 5,
            min_score: 0.5,
            canned: Some("--- Relevant context from memory ---\n[score: 0.95] x\n---".into()),
            embedding_seen: embedding_seen.clone(),
        });
        cell.set(stub).ok();
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess1|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        assert!(
            captured.system_prompt.is_none(),
            "rag_enabled=false must not produce a system prompt"
        );
        assert!(
            embedding_seen.lock().unwrap().is_none(),
            "rag_enabled=false must not call fetch_rag"
        );
    }

    #[tokio::test]
    async fn agent_memory_precedes_rag_block_in_system_prompt() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        // Dual stub: provides BOTH agent/user memory AND RAG hits.
        struct DualStub;
        #[async_trait::async_trait]
        impl MemoryFetcher for DualStub {
            async fn fetch(&self, _: &str) -> Option<(String, String)> {
                Some(("rust uses cargo".into(), "prefers concise replies".into()))
            }
            async fn fetch_history(&self, _: &str) -> Option<String> {
                None
            }
            fn rag_enabled(&self) -> bool {
                true
            }
            fn rag_top_k(&self) -> usize {
                3
            }
            fn rag_min_score(&self) -> f32 {
                0.5
            }
            async fn fetch_rag(&self, _: &str, _: &[f32], _: usize, _: f32) -> Option<String> {
                Some(memory_dispatcher::format_rag_block(&[
                    memory_dispatcher::RagHit {
                        score: 0.88,
                        target: "agent",
                        chunk: "rag-chunk-1".into(),
                    },
                ]))
            }
        }
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let stub: Arc<dyn MemoryFetcher> = Arc::new(DualStub);
        cell.set(stub).ok();
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess1|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        let sp = captured.system_prompt.expect("system_prompt present");
        let agent_pos = sp
            .find("--- AGENT MEMORY ---")
            .expect("agent memory header present");
        let rag_pos = sp
            .find("--- Relevant context from memory ---")
            .expect("rag header present");
        assert!(
            agent_pos < rag_pos,
            "agent memory must precede RAG block: agent_pos={agent_pos} rag_pos={rag_pos}\n{sp}"
        );
        // Conversation history goes into ChatInput.history, not
        // into system_prompt.
        assert!(captured.history.is_empty() || !captured.history.contains("---"));
    }

    #[test]
    fn rag_block_format_matches_spec() {
        let hits = vec![
            memory_dispatcher::RagHit {
                score: 0.923,
                target: "agent",
                chunk: "what's the deadline for X?".into(),
            },
            memory_dispatcher::RagHit {
                score: 0.871,
                target: "user",
                chunk: "the deadline is Friday".into(),
            },
        ];
        let block = memory_dispatcher::format_rag_block(&hits);
        assert!(block.starts_with("--- Relevant context from memory ---\n"));
        assert!(block.contains("[score: 0.92] (agent) what's the deadline for X?"));
        assert!(block.contains("[score: 0.87] (user) the deadline is Friday"));
        assert!(block.ends_with("---"));
    }

    #[test]
    fn parse_rag_hits_filters_by_min_score_and_skips_count_line() {
        let body = b"id1\t0.92\thigh score\nid2\t0.55\tlow score\nid3\t0.75\tmid score\ncount=3\n";
        let mut out = Vec::new();
        memory_dispatcher::parse_rag_hits(body, "agent", 0.70, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].score, 0.92);
        assert_eq!(out[0].chunk, "high score");
        assert_eq!(out[0].target, "agent");
        assert_eq!(out[1].score, 0.75);
        assert_eq!(out[1].chunk, "mid score");
    }

    #[test]
    fn combine_system_blocks_renders_two_section_layout() {
        let only_agent = combine_system_blocks(Some("AGENT".into()), None);
        assert_eq!(only_agent.as_deref(), Some("AGENT"));
        let only_rag = combine_system_blocks(None, Some("RAG".into()));
        assert_eq!(only_rag.as_deref(), Some("RAG"));
        let both = combine_system_blocks(Some("AGENT".into()), Some("RAG".into())).unwrap();
        assert!(both.starts_with("AGENT"));
        assert!(both.ends_with("RAG"));
        // Blank line between the two blocks.
        assert!(both.contains("AGENT\n\nRAG"));
        assert!(combine_system_blocks(None, None).is_none());
    }

    #[test]
    fn merge_history_concatenates_with_normalised_newline() {
        // Auto without trailing newline gets one inserted before
        // caller content so the boundary lines stay distinct.
        let m = merge_history("user: a", "user: b\n");
        assert_eq!(m, "user: a\nuser: b\n");
        let m = merge_history("user: a\n", "user: b\n");
        assert_eq!(m, "user: a\nuser: b\n");
        // Either side empty → the other side wins verbatim.
        assert_eq!(merge_history("", "x"), "x");
        assert_eq!(merge_history("x", ""), "x");
        assert_eq!(merge_history("", ""), "");
    }

    #[tokio::test]
    async fn memory_injection_skipped_when_dispatcher_cell_empty() {
        let recorded = Arc::new(std::sync::Mutex::new(None));
        let rec_provider: Arc<dyn ChatProvider> = Arc::new(RecordingProvider {
            last: recorded.clone(),
        });
        // OnceCell never populated — exercises the unconfigured
        // path (AI controller booted without an [ai.memory_peer]
        // section).
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let r = handle_chat(
            rec_provider,
            String::new(),
            cell,
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"s1|hello|"),
        )
        .await;
        match r {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("unexpected err: {}", e.cause),
        }
        let captured = recorded.lock().unwrap().clone().unwrap();
        assert!(captured.system_prompt.is_none());
    }

    #[tokio::test]
    async fn embed_handler_returns_model_and_b64_vectors() {
        use crate::nodes::ai::provider::MOCK_EMBED_DIMS;
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        // arg: model|text1§text2 with empty model → mock-embed.
        let r = handle_embed(p, ctx(b"|hello there\xc2\xa7second one")).await;
        let bytes = match r {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("unexpected err: {}", e.cause),
        };
        let text = std::str::from_utf8(&bytes).unwrap().trim_end_matches('\n');
        let mut parts = text.split('|');
        let model = parts.next().unwrap();
        assert_eq!(model, "mock-embed");
        let vecs: Vec<&str> = parts.collect();
        assert_eq!(vecs.len(), 2);
        // Each base64-decoded chunk is MOCK_EMBED_DIMS * 4 bytes.
        use base64::Engine;
        for v in &vecs {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(v.as_bytes())
                .unwrap();
            assert_eq!(raw.len(), MOCK_EMBED_DIMS * 4);
        }
    }

    #[tokio::test]
    async fn embed_handler_rejects_arg_without_pipe() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r = handle_embed(p, ctx(b"no-pipe-here")).await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    #[tokio::test]
    async fn embed_handler_rejects_no_texts() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r = handle_embed(p, ctx(b"model|")).await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    #[test]
    fn build_provider_defaults_to_mock() {
        let cfg = AiConfig::default();
        match build_provider(&cfg) {
            Ok(p) => assert_eq!(p.provider_name(), "mock"),
            Err(e) => panic!("default config should build: {e}"),
        }
    }

    #[test]
    fn build_provider_requires_per_provider_section() {
        let cfg = AiConfig {
            provider: "openrouter".into(),
            model: String::new(),
            providers: ProviderEntries::new(),
            memory_peer: None,
            agent: None,
            routing: None,
            belief_state: None,
            judge: None,
            perception_security: None,
        };
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(
                e.to_string().contains("[ai.providers.openrouter]"),
                "msg: {e}"
            ),
        }
    }

    #[test]
    fn build_provider_rejects_unknown_provider() {
        let cfg = AiConfig {
            provider: "rumple".into(),
            model: String::new(),
            providers: ProviderEntries::new(),
            memory_peer: None,
            agent: None,
            routing: None,
            belief_state: None,
            judge: None,
            perception_security: None,
        };
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.to_string().contains("unknown provider")),
        }
    }

    #[test]
    fn build_provider_anthropic_signals_missing_key_env() {
        let mut providers = ProviderEntries::new();
        providers.insert(
            "anthropic".into(),
            ProviderEntry {
                base_url: None,
                api_key_env: None, // no env var named at all → clear error
                default_model: None,
                timeout_secs: 30,
            },
        );
        let cfg = AiConfig {
            provider: "anthropic".into(),
            model: String::new(),
            providers,
            memory_peer: None,
            agent: None,
            routing: None,
            belief_state: None,
            judge: None,
            perception_security: None,
        };
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.to_string().contains("missing api_key_env"), "msg: {e}"),
        }
    }

    #[test]
    fn build_provider_local_no_key_ok() {
        let mut providers = ProviderEntries::new();
        providers.insert(
            "local".into(),
            ProviderEntry {
                base_url: Some("http://localhost:11434/v1".into()),
                api_key_env: None,
                default_model: Some("llama3:8b".into()),
                timeout_secs: 30,
            },
        );
        let cfg = AiConfig {
            provider: "local".into(),
            model: String::new(),
            providers,
            memory_peer: None,
            agent: None,
            routing: None,
            belief_state: None,
            judge: None,
            perception_security: None,
        };
        match build_provider(&cfg) {
            Ok(p) => assert_eq!(p.provider_name(), "local"),
            Err(e) => panic!("local should build without key: {e}"),
        }
    }

    // ── Task 2: reversibility + evidence wiring ──────────

    /// A ChatProvider that returns a fixed reply — used to
    /// drive the irreversible-plan branch of `handle_chat`.
    struct CannedProvider {
        reply: String,
    }

    #[async_trait::async_trait]
    impl ChatProvider for CannedProvider {
        async fn generate_reply(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            Ok(provider::ChatOutput {
                text: self.reply.clone(),
                provider: "canned",
                model: String::new(),
                usage: None,
                finish_reason: None,
                logprob: None,
            })
        }
        fn provider_name(&self) -> &'static str {
            "canned"
        }
        async fn generate_embeddings(
            &self,
            _input: provider::EmbedInput,
        ) -> Result<provider::EmbedOutput, ProviderError> {
            Err(ProviderError::Permanent("not supported".into()))
        }
    }

    #[tokio::test]
    async fn irreversible_plan_without_token_returns_approval_required_body() {
        let irreversible_reply = "<plan>\ntool: email.send\nargs: to=ops\n</plan>";
        let provider: Arc<dyn ChatProvider> = Arc::new(CannedProvider {
            reply: irreversible_reply.into(),
        });
        let r = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-1|please email ops|"),
        )
        .await;
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("unexpected err: {}", e.cause),
        };
        assert!(
            body.starts_with("[approval-required]"),
            "expected approval-required prefix, got: {body}"
        );
        assert!(body.contains("irreversible"));
    }

    #[tokio::test]
    async fn irreversible_plan_with_token_in_args_proceeds_normally() {
        let irreversible_reply = "<plan>\ntool: email.send\nargs: to=ops\n</plan>";
        let provider: Arc<dyn ChatProvider> = Arc::new(CannedProvider {
            reply: irreversible_reply.into(),
        });
        // Token rides on the history field (after the second
        // pipe) — extract_approval_token scans the whole arg
        // buffer so its placement is flexible.
        let r = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-1|please email ops|approval_token=abc123"),
        )
        .await;
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("unexpected err: {}", e.cause),
        };
        assert!(
            !body.starts_with("[approval-required]"),
            "approval token must clear the gate; got: {body}"
        );
        assert!(body.contains("email.send"));
    }

    #[test]
    fn extract_approval_token_parses_from_args() {
        assert_eq!(
            extract_approval_token("sess-1|prompt|approval_token=xyz"),
            Some("xyz".to_string())
        );
        // Token with trailing pipe — bounded by the
        // separator.
        assert_eq!(
            extract_approval_token("approval_token=xyz|extra"),
            Some("xyz".to_string())
        );
        // Whitespace-bounded.
        assert_eq!(
            extract_approval_token("hi approval_token=xyz then more"),
            Some("xyz".to_string())
        );
        // Empty value yields None.
        assert!(extract_approval_token("approval_token= ").is_none());
        // Absent → None.
        assert!(extract_approval_token("nothing here").is_none());
    }

    #[test]
    fn evidence_record_round_trip_after_handle_chat_shape() {
        // Construct an EvidenceRecord the way handle_chat
        // does and confirm the JSON wire shape so a future
        // schema break here would fail this test.
        use crate::nodes::ai::execution::{
            EvidenceRecord, ExecutionState, Executor, Planner, StepResult,
        };
        let plan = Planner::parse_response("hello world");
        let mut state = ExecutionState::new(plan);
        Executor::advance(
            &mut state,
            StepResult::Ok {
                output: "hello world".into(),
            },
        );
        let rec = EvidenceRecord::from_state(
            &state,
            "sess-1",
            "sess-1",
            Some("approval_token:abc".into()),
        );
        let json = rec.to_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["task_id"], "sess-1");
        assert_eq!(parsed["session_id"], "sess-1");
        assert_eq!(parsed["reversibility"], "reversible");
        assert_eq!(parsed["approved_by"], "approval_token:abc");
        let plan_steps = parsed["plan_steps"].as_array().unwrap();
        assert_eq!(plan_steps.len(), 1);
        let step_results = parsed["step_results"].as_array().unwrap();
        assert_eq!(step_results.len(), 1);
    }

    // ─────────── W1 — ToolDispatcher integration ────────────

    fn build_dispatcher(
        deny: &[&str],
        secrets: &[(&str, &str)],
    ) -> Arc<crate::nodes::tool::dispatcher::ToolDispatcher> {
        use crate::nodes::execution::broker::{AccessPolicy, AgentAccessBroker};
        use crate::nodes::execution::secrets::SecretStore;
        use std::collections::BTreeMap;
        let store = {
            let mut m = BTreeMap::new();
            for (k, v) in secrets {
                m.insert((*k).to_string(), (*v).to_string());
            }
            Arc::new(SecretStore::from_map(m))
        };
        let broker = if deny.is_empty() {
            Arc::new(AgentAccessBroker::empty())
        } else {
            Arc::new(AgentAccessBroker::new(vec![AccessPolicy {
                agent: "alice".into(),
                allowed_capabilities: Vec::new(),
                denied_capabilities: deny.iter().map(|s| (*s).to_string()).collect(),
                max_calls_per_minute: 60,
                max_cost_cents_per_hour: 500,
            }]))
        };
        Arc::new(crate::nodes::tool::dispatcher::ToolDispatcher::new(
            store, broker,
        ))
    }

    #[tokio::test]
    async fn handle_chat_dispatches_planner_tool_call_through_dispatcher_on_allow() {
        // The canned reply carries a `<plan>` block with a
        // single web.fetch step. The dispatcher's broker is
        // empty so the call is admitted and lands in the
        // gateway as `completed=1`.
        let reply = "<plan>\ntool: web.fetch\nargs: https://example.com\n</plan>";
        let provider: Arc<dyn ChatProvider> = Arc::new(CannedProvider {
            reply: reply.into(),
        });
        let dispatcher = build_dispatcher(&[], &[]);
        let r = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(dispatcher.clone()),
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-1|fetch the example|"),
        )
        .await;
        // Even though the plan was reversible, the handler
        // returns Ok with the model reply. The gateway has the
        // record.
        match r {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("expected Ok, got err: {}", e.cause),
        }
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=1 failed=0"), "snap={snap}");
        assert!(snap.contains("web.fetch"), "snap={snap}");
    }

    #[tokio::test]
    async fn handle_chat_returns_structured_error_when_tool_dispatch_denied() {
        // Plan asks for `tool.terminal`. Broker denies that
        // capability for alice. The handler must NOT execute
        // the tool and must surface a structured error in the
        // response body.
        let reply = "<plan>\ntool: tool.terminal\nargs: rm -rf /\n</plan>";
        let provider: Arc<dyn ChatProvider> = Arc::new(CannedProvider {
            reply: reply.into(),
        });
        let dispatcher = build_dispatcher(&["tool.terminal"], &[]);
        let r = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(dispatcher.clone()),
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-1|delete everything|approval_token=ok"),
        )
        .await;
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok with error trailer, got: {}", e.cause),
        };
        assert!(
            body.contains("[tool-dispatch-errors]"),
            "missing error trailer: {body}"
        );
        // The structured error per the dispatcher contract.
        assert!(body.contains("\"kind\":\"access_denied\""), "body={body}");
        assert!(body.contains("tool.terminal"), "body={body}");
        // Gateway must not show any executed call.
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=0 failed=0"), "snap={snap}");
    }

    #[tokio::test]
    async fn handle_chat_resolves_secret_placeholder_in_tool_call_args() {
        // The plan's tool args contain `{{secret:github_token}}`.
        // The dispatcher resolves it before invoking the
        // admission handler; the gateway records the call so
        // the test can confirm the dispatcher actually ran the
        // secret pass.
        let reply =
            "<plan>\ntool: web.fetch\nargs: Authorization: Bearer {{secret:github_token}}\n</plan>";
        let provider: Arc<dyn ChatProvider> = Arc::new(CannedProvider {
            reply: reply.into(),
        });
        let dispatcher = build_dispatcher(&[], &[("github_token", "ghp_real")]);
        let r = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(dispatcher.clone()),
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-1|fetch with auth|approval_token=ok"),
        )
        .await;
        assert!(
            matches!(r, HandlerOutcome::Ok(_)),
            "handle_chat should succeed when the broker admits the call"
        );
        // The gateway records the resolved args verbatim. The
        // resolved Authorization line is 30 chars long, so the
        // admission stub records `args_len=30`. The snapshot
        // shows the call as completed.
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=1 failed=0"), "snap={snap}");
        assert!(snap.contains("args_len=30"), "snap={snap}");
    }

    // ───────────────────── RELIX-2 step 3 ────────────────────
    //
    // `ai.chat.stream` handler tests. These verify the
    // pre-flight (guardrails, wire-arg parsing) reuses the
    // shared `build_chat_preflight` helper, and that the
    // streaming-shape errors / chunks are emitted in the
    // expected sequence.

    /// Provider whose streaming impl yields a fixed sequence
    /// of chunks. Used to verify the AI node pipes provider
    /// chunks through to the dispatcher's HandlerStream
    /// without buffering.
    struct ChunkedStreamProvider {
        chunks: Vec<String>,
    }

    #[async_trait::async_trait]
    impl ChatProvider for ChunkedStreamProvider {
        async fn generate_reply(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            Ok(provider::ChatOutput {
                text: self.chunks.join(""),
                provider: "chunked-stream",
                model: String::new(),
                usage: None,
                finish_reason: None,
                logprob: None,
            })
        }
        fn provider_name(&self) -> &'static str {
            "chunked-stream"
        }
        async fn generate_reply_stream(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatStream, ProviderError> {
            let owned: Vec<String> = self.chunks.clone();
            let s = futures::stream::iter(
                owned
                    .into_iter()
                    .map(|t| Ok(provider::StreamingChunk::Text(t))),
            );
            Ok(Box::pin(s))
        }
    }

    /// Provider whose `generate_reply_stream` returns Err
    /// immediately — exercises the upfront-error path.
    struct FailingStreamProvider;

    #[async_trait::async_trait]
    impl ChatProvider for FailingStreamProvider {
        async fn generate_reply(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            Err(ProviderError::Transient("test transient".into()))
        }
        fn provider_name(&self) -> &'static str {
            "failing-stream"
        }
        async fn generate_reply_stream(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatStream, ProviderError> {
            Err(ProviderError::Permanent(
                "provider stream init failed".into(),
            ))
        }
    }

    #[tokio::test]
    async fn chat_stream_yields_provider_chunks_in_order() {
        let provider: Arc<dyn ChatProvider> = Arc::new(ChunkedStreamProvider {
            chunks: vec!["alpha".into(), "beta".into(), "gamma".into()],
        });
        let stream = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"session-1|hello|"),
        )
        .await
        .expect("preflight + provider stream init must succeed");
        use futures::StreamExt;
        let collected: Vec<String> = stream
            .map(|item| match item {
                Ok(bytes) => String::from_utf8(bytes).expect("utf-8 chunk"),
                Err(e) => panic!("unexpected stream error: {}", e.cause),
            })
            .collect()
            .await;
        assert_eq!(collected, vec!["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn chat_stream_returns_invalid_args_when_prompt_missing() {
        let provider: Arc<dyn ChatProvider> = Arc::new(ChunkedStreamProvider {
            chunks: vec!["should-not-reach".into()],
        });
        let outcome = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"only-session-id"),
        )
        .await;
        match outcome {
            Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            Ok(_) => panic!("expected upfront ErrorEnvelope, got a stream"),
        }
    }

    #[tokio::test]
    async fn chat_stream_returns_security_denied_when_guardrail_blocks() {
        let provider: Arc<dyn ChatProvider> = Arc::new(ChunkedStreamProvider {
            chunks: vec!["should-not-reach".into()],
        });
        // Hidden-Unicode check is always-on regardless of
        // config (see `hidden_unicode_reason` in
        // guardrails::input). A zero-width space (U+200B) in
        // the prompt triggers the block under any guardrail
        // mode, including `permissive`. The streaming
        // pre-flight must surface this as
        // `error_kinds::SECURITY_DENIED` without ever
        // invoking the provider.
        let prompt = "session-1|hello\u{200B}world|";
        let outcome = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(prompt.as_bytes()),
        )
        .await;
        match outcome {
            Err(e) => assert_eq!(e.kind, error_kinds::SECURITY_DENIED),
            Ok(_) => panic!("expected guardrail-blocked ErrorEnvelope, got a stream"),
        }
    }

    #[tokio::test]
    async fn chat_stream_surfaces_provider_init_failure_as_upfront_error() {
        let provider: Arc<dyn ChatProvider> = Arc::new(FailingStreamProvider);
        let outcome = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"session-1|hello|"),
        )
        .await;
        match outcome {
            Err(e) => {
                assert_eq!(e.kind, error_kinds::RESPONDER_INTERNAL);
                assert!(
                    e.cause.contains("provider stream init failed"),
                    "cause should propagate provider message: {}",
                    e.cause
                );
            }
            Ok(_) => panic!("expected upfront ErrorEnvelope when provider init fails"),
        }
    }

    // ── RELIX-7.11 GAP 1: AI handler → metrics sink ───────

    /// Stub `MetricsSink` that records every `attach_ai_usage`
    /// call into a shared `Mutex` so the GAP-1 tests can assert
    /// what arrived.
    #[derive(Default)]
    struct RecordingMetricsSink {
        hints: std::sync::Mutex<Vec<crate::metrics::AiUsageHint>>,
    }

    impl crate::metrics::MetricsSink for RecordingMetricsSink {
        fn record_invocation(&self, _: crate::metrics::InvocationMetric) {
            // The dispatch bridge owns this path; the AI handler
            // tests only exercise the AI-side enrichment hook.
        }
        fn attach_ai_usage(&self, hint: crate::metrics::AiUsageHint) {
            self.hints.lock().unwrap().push(hint);
        }
    }

    /// Provider that ships a fixed `ChatOutput` carrying token
    /// usage. Used to verify the AI handler forwards usage to
    /// the metrics sink.
    struct ProviderWithUsage {
        text: String,
        model: &'static str,
        usage: provider::TokenUsage,
    }

    #[async_trait::async_trait]
    impl ChatProvider for ProviderWithUsage {
        async fn generate_reply(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            Ok(provider::ChatOutput {
                text: self.text.clone(),
                provider: "test-with-usage",
                model: self.model.to_string(),
                usage: Some(self.usage),
                finish_reason: None,
                logprob: None,
            })
        }
        fn provider_name(&self) -> &'static str {
            "test-with-usage"
        }
    }

    /// Same shape as `ProviderWithUsage` but ships
    /// `usage: None`.
    struct ProviderNoUsage {
        text: String,
    }

    #[async_trait::async_trait]
    impl ChatProvider for ProviderNoUsage {
        async fn generate_reply(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            Ok(provider::ChatOutput {
                text: self.text.clone(),
                provider: "test-no-usage",
                model: "doesnt-matter".to_string(),
                usage: None,
                finish_reason: None,
                logprob: None,
            })
        }
        fn provider_name(&self) -> &'static str {
            "test-no-usage"
        }
    }

    fn ctx_with_request_id(args: &[u8], rid: RequestId) -> InvocationCtx {
        let mut c = ctx(args);
        c.request_id = rid;
        c
    }

    #[tokio::test]
    async fn ai_handler_forwards_token_usage_to_metrics_sink() {
        let provider: Arc<dyn ChatProvider> = Arc::new(ProviderWithUsage {
            text: "hello back".into(),
            model: "gpt-4o-mini",
            usage: provider::TokenUsage {
                prompt_tokens: 50,
                completion_tokens: 17,
                total_tokens: 67,
            },
        });
        let sink = Arc::new(RecordingMetricsSink::default());
        let sink_dyn: Arc<dyn crate::metrics::MetricsSink> = sink.clone();
        let rid = RequestId([7u8; 16]);
        let outcome = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            Some(sink_dyn),
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx_with_request_id(b"sess1|please respond|", rid),
        )
        .await;
        assert!(matches!(outcome, HandlerOutcome::Ok(_)));
        let hints = sink.hints.lock().unwrap();
        assert_eq!(hints.len(), 1, "expected exactly one usage hint");
        let h = &hints[0];
        assert_eq!(h.request_id, rid, "request_id must thread through ctx");
        assert_eq!(h.prompt_tokens, 50);
        assert_eq!(h.completion_tokens, 17);
        assert_eq!(h.model, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn ai_handler_skips_attach_when_provider_omits_usage() {
        let provider: Arc<dyn ChatProvider> = Arc::new(ProviderNoUsage { text: "ack".into() });
        let sink = Arc::new(RecordingMetricsSink::default());
        let sink_dyn: Arc<dyn crate::metrics::MetricsSink> = sink.clone();
        let outcome = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            Some(sink_dyn),
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess2|hi|"),
        )
        .await;
        assert!(matches!(outcome, HandlerOutcome::Ok(_)));
        let hints = sink.hints.lock().unwrap();
        assert!(
            hints.is_empty(),
            "no provider usage → no attach_ai_usage call (got {} hints)",
            hints.len()
        );
    }

    /// End-to-end through the collector — verifies the usage
    /// hint actually lands on the dispatch row when both the
    /// handler attaches and the dispatch records.
    #[tokio::test]
    async fn ai_usage_round_trips_through_collector_to_store() {
        use crate::metrics::{
            AiUsageHint, InvocationMetric, MetricsCollector, MetricsSink, MetricsStore, PriceTable,
            RetentionConfig,
        };
        let store = MetricsStore::in_memory().unwrap();
        let prices = PriceTable::with_defaults();
        let (col, handles) = MetricsCollector::new(store.clone(), prices);
        let _spawned = handles.spawn(RetentionConfig {
            retention_days: 30,
            sweep_interval: std::time::Duration::from_secs(3600),
        });
        let rid = RequestId([42u8; 16]);
        // Simulate the AI handler: attach usage before the
        // dispatch records.
        col.attach_ai_usage(AiUsageHint {
            request_id: rid,
            prompt_tokens: 100,
            completion_tokens: 200,
            model: "gpt-4o-mini".into(),
            routing_tier: None,
        });
        // Simulate the dispatch hot path: record the metric.
        col.record_invocation(InvocationMetric {
            agent_name: "alice".into(),
            tenant_id: "default".into(),
            peer_alias: "ai".into(),
            method: "ai.chat".into(),
            timestamp_ms: 1_700_000_000_000,
            latency_ms: 12,
            success: true,
            error_kind: None,
            token_count: None,
            cost_micros: None,
            input_bytes: 32,
            output_bytes: 64,
            model: None,
            confidence_score: None,
            routing_tier: None,
            request_id: Some(rid),
        });
        // Let the drain loop flush.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let (tokens, cost, model): (Option<i64>, Option<i64>, Option<String>) = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT token_count, cost_micros, model FROM metrics_invocations",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
            })
            .unwrap();
        assert_eq!(tokens, Some(300));
        assert!(cost.unwrap() > 0);
        assert_eq!(model.as_deref(), Some("gpt-4o-mini"));
    }

    // ── RELIX-7.11 GAP 2: streaming token enrichment ─────

    /// Streaming provider that yields one text chunk followed by
    /// a usage frame. Verifies the `ChatStream`'s new
    /// `StreamingChunk::Usage` plumbing all the way through
    /// `handle_chat_stream` to `attach_ai_usage`.
    struct StreamingProviderWithUsage {
        text: String,
        usage: provider::StreamingUsage,
    }

    #[async_trait::async_trait]
    impl ChatProvider for StreamingProviderWithUsage {
        async fn generate_reply(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            Ok(provider::ChatOutput {
                text: self.text.clone(),
                provider: "streaming-with-usage",
                model: self.usage.model.clone(),
                usage: None,
                finish_reason: None,
                logprob: None,
            })
        }
        async fn generate_reply_stream(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatStream, ProviderError> {
            let text = self.text.clone();
            let u = self.usage.clone();
            let s = async_stream::stream! {
                yield Ok(provider::StreamingChunk::Text(text));
                yield Ok(provider::StreamingChunk::Usage(u));
            };
            Ok(Box::pin(s))
        }
        fn provider_name(&self) -> &'static str {
            "streaming-with-usage"
        }
    }

    /// Same shape but yields only Text chunks (no Usage).
    struct StreamingProviderNoUsage {
        text: String,
    }

    #[async_trait::async_trait]
    impl ChatProvider for StreamingProviderNoUsage {
        async fn generate_reply(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            Ok(provider::ChatOutput {
                text: self.text.clone(),
                provider: "streaming-no-usage",
                model: "doesnt-matter".to_string(),
                usage: None,
                finish_reason: None,
                logprob: None,
            })
        }
        async fn generate_reply_stream(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatStream, ProviderError> {
            let text = self.text.clone();
            let s = async_stream::stream! {
                yield Ok(provider::StreamingChunk::Text(text));
            };
            Ok(Box::pin(s))
        }
        fn provider_name(&self) -> &'static str {
            "streaming-no-usage"
        }
    }

    #[tokio::test]
    async fn streaming_handler_forwards_text_and_attaches_usage_to_sink() {
        use futures::StreamExt;
        let provider: Arc<dyn ChatProvider> = Arc::new(StreamingProviderWithUsage {
            text: "hello stream".into(),
            usage: provider::StreamingUsage {
                prompt_tokens: 30,
                completion_tokens: 70,
                model: "gpt-4o-mini".into(),
            },
        });
        let sink = Arc::new(RecordingMetricsSink::default());
        let sink_dyn: Arc<dyn crate::metrics::MetricsSink> = sink.clone();
        let rid = RequestId([9u8; 16]);
        let stream = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(sink_dyn),
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx_with_request_id(b"sess1|please reply|", rid),
        )
        .await
        .expect("preflight + stream init");
        // The wire stream MUST only carry text bytes — the
        // Usage frame is consumed by the handler.
        let collected: Vec<String> = stream
            .map(|item| match item {
                Ok(bytes) => String::from_utf8(bytes).expect("utf-8 chunk"),
                Err(e) => panic!("unexpected stream error: {}", e.cause),
            })
            .collect()
            .await;
        let assembled: String = collected.join("");
        assert_eq!(assembled, "hello stream");
        let hints = sink.hints.lock().unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].request_id, rid);
        assert_eq!(hints[0].prompt_tokens, 30);
        assert_eq!(hints[0].completion_tokens, 70);
        assert_eq!(hints[0].model, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn streaming_handler_skips_attach_when_provider_emits_no_usage() {
        use futures::StreamExt;
        let provider: Arc<dyn ChatProvider> =
            Arc::new(StreamingProviderNoUsage { text: "ack".into() });
        let sink = Arc::new(RecordingMetricsSink::default());
        let sink_dyn: Arc<dyn crate::metrics::MetricsSink> = sink.clone();
        let stream = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(sink_dyn),
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess2|hi|"),
        )
        .await
        .expect("stream");
        let _: Vec<_> = stream.collect().await;
        let hints = sink.hints.lock().unwrap();
        assert!(hints.is_empty(), "no usage frame → no attach call");
    }

    #[tokio::test]
    async fn streaming_usage_round_trips_to_metrics_store() {
        use crate::metrics::{
            InvocationMetric, MetricsCollector, MetricsSink as MetricsSinkTrait, MetricsStore,
            PriceTable, RetentionConfig,
        };
        use futures::StreamExt;
        let store = MetricsStore::in_memory().unwrap();
        let prices = PriceTable::with_defaults();
        let (col, handles) = MetricsCollector::new(store.clone(), prices);
        let _spawned = handles.spawn(RetentionConfig {
            retention_days: 30,
            sweep_interval: std::time::Duration::from_secs(3600),
        });
        let col_dyn: Arc<dyn MetricsSinkTrait> = Arc::new(col.clone());
        let provider: Arc<dyn ChatProvider> = Arc::new(StreamingProviderWithUsage {
            text: "x".into(),
            usage: provider::StreamingUsage {
                prompt_tokens: 100,
                completion_tokens: 200,
                model: "gpt-4o-mini".into(),
            },
        });
        let rid = RequestId([55u8; 16]);
        let stream = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(col_dyn.clone()),
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx_with_request_id(b"sess|hi|", rid),
        )
        .await
        .expect("stream");
        // Drain the wire stream so the handler reaches the
        // Usage frame and calls attach_ai_usage.
        let _: Vec<_> = stream.collect().await;
        // Simulate the dispatch bridge's post-handler metric
        // record (the bridge calls record_invocation after the
        // stream closes).
        col.record_invocation(InvocationMetric {
            agent_name: "alice".into(),
            tenant_id: "default".into(),
            peer_alias: "ai".into(),
            method: "ai.chat.stream".into(),
            timestamp_ms: 1_700_000_000_000,
            latency_ms: 12,
            success: true,
            error_kind: None,
            token_count: None,
            cost_micros: None,
            input_bytes: 32,
            output_bytes: 1,
            model: None,
            confidence_score: None,
            routing_tier: None,
            request_id: Some(rid),
        });
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let (tokens, cost, model): (Option<i64>, Option<i64>, Option<String>) = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT token_count, cost_micros, model FROM metrics_invocations",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
            })
            .unwrap();
        assert_eq!(tokens, Some(300));
        assert!(cost.unwrap() > 0, "cost must compute from price table");
        assert_eq!(model.as_deref(), Some("gpt-4o-mini"));
    }

    // ── GAP 13 + 14: handler-side provenance + observability ──

    #[tokio::test]
    async fn handle_chat_records_provenance_and_metadata_when_observability_wired() {
        let provider: Arc<dyn ChatProvider> = Arc::new(CannedProvider { reply: "ok".into() });
        let obs = std::sync::Arc::new(crate::observability::ObservabilityContext::in_memory());
        let r = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            Some(obs.clone()),
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-obs|hello|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        // Metadata event lands in Sink A.
        let rows = obs.metadata.query(Some("sess-obs"), None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "ai.chat.complete");
        assert!(rows[0].success);
        // No content in Sink B (mesh-internal calls are
        // metadata-only by design).
        assert!(obs.content.get(&rows[0].event_id).unwrap().is_none());
    }

    #[tokio::test]
    async fn handle_chat_skips_observability_when_no_context() {
        let provider: Arc<dyn ChatProvider> = Arc::new(CannedProvider { reply: "ok".into() });
        let r = handle_chat(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            None,
            Arc::new(tokio::sync::OnceCell::new()),
            None,
            None,
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            crate::confidence::SelfConsistencyConfig::default(),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-noobs|hi|"),
        )
        .await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        // No panic, no overhead beyond the existing pipeline —
        // this test is the regression guard that an absent
        // ObservabilityContext is a true no-op.
    }

    // ── RELIX-7.29 (post-rebuild) — SC on streaming ─────

    /// Sink that records SC + usage hints for assertion.
    #[derive(Default)]
    struct ScStreamSink {
        usage: std::sync::Mutex<Vec<crate::metrics::AiUsageHint>>,
        sc: std::sync::Mutex<Vec<crate::metrics::AiSelfConsistencyHint>>,
    }
    impl crate::metrics::MetricsSink for ScStreamSink {
        fn record_invocation(&self, _: crate::metrics::InvocationMetric) {}
        fn attach_ai_usage(&self, hint: crate::metrics::AiUsageHint) {
            self.usage.lock().unwrap().push(hint);
        }
        fn attach_self_consistency(&self, hint: crate::metrics::AiSelfConsistencyHint) {
            self.sc.lock().unwrap().push(hint);
        }
    }

    /// Deterministic provider for the SC-streaming tests:
    /// every call returns the same body + usage. Embeddings
    /// are flat-1 so cosine = 1 and best_index = 0.
    struct ScSampleProvider {
        text: String,
    }

    #[async_trait::async_trait]
    impl ChatProvider for ScSampleProvider {
        async fn generate_reply(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatOutput, ProviderError> {
            Ok(provider::ChatOutput {
                text: self.text.clone(),
                provider: "sc-sample",
                model: "sc-sample-model".to_string(),
                usage: Some(provider::TokenUsage {
                    prompt_tokens: 7,
                    completion_tokens: 11,
                    total_tokens: 18,
                }),
                finish_reason: None,
                logprob: None,
            })
        }
        fn provider_name(&self) -> &'static str {
            "sc-sample"
        }
        async fn generate_reply_stream(
            &self,
            _input: ChatInput,
        ) -> Result<provider::ChatStream, ProviderError> {
            // Fallback path emits a distinct token so a test can
            // detect when SC was skipped vs fired.
            let owned = vec!["STREAM-FALLBACK".to_string()];
            let s = futures::stream::iter(
                owned
                    .into_iter()
                    .map(|t| Ok(provider::StreamingChunk::Text(t))),
            );
            Ok(Box::pin(s))
        }
        async fn generate_embeddings(
            &self,
            input: provider::EmbedInput,
        ) -> Result<provider::EmbedOutput, ProviderError> {
            let vectors: Vec<Vec<f32>> = input.texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect();
            Ok(provider::EmbedOutput {
                model: "sc-embed".into(),
                vectors,
            })
        }
    }

    fn sc_streaming_cfg(sample_count: usize) -> crate::confidence::SelfConsistencyConfig {
        crate::confidence::SelfConsistencyConfig {
            enabled: true,
            sample_count,
            min_score_to_enable: 1.0,
            capability_patterns: vec!["ai.chat.stream".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn chat_stream_runs_sc_when_enabled_and_sample_count_three() {
        let sink_typed: Arc<ScStreamSink> = Arc::new(ScStreamSink::default());
        let sink: Arc<dyn crate::metrics::MetricsSink> = sink_typed.clone();
        let provider: Arc<dyn ChatProvider> = Arc::new(ScSampleProvider {
            text: "winning sample text".to_string(),
        });
        let stream = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(sink),
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            sc_streaming_cfg(3),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-sc-stream|hello|"),
        )
        .await
        .expect("stream init");
        use futures::StreamExt;
        let collected: Vec<String> = stream
            .map(|item| match item {
                Ok(bytes) => String::from_utf8(bytes).expect("utf-8 chunk"),
                Err(e) => panic!("unexpected stream error: {}", e.cause),
            })
            .collect()
            .await;
        let joined: String = collected.join("");
        assert_eq!(joined, "winning sample text");
        assert!(
            !collected.iter().any(|c| c.contains("STREAM-FALLBACK")),
            "SC should have fired; got {collected:?}"
        );
        let recorded_sc = sink_typed.sc.lock().unwrap().clone();
        let recorded_usage = sink_typed.usage.lock().unwrap().clone();
        assert_eq!(recorded_sc.len(), 1, "expected exactly one SC hint");
        assert_eq!(recorded_sc[0].sample_count, 3);
        assert_eq!(recorded_sc[0].best_sample_index, 0);
        assert!((recorded_sc[0].score - 1.0).abs() < 1e-5);
        assert_eq!(
            recorded_usage.len(),
            1,
            "expected one aggregated usage hint"
        );
        assert_eq!(recorded_usage[0].prompt_tokens, 21);
        assert_eq!(recorded_usage[0].completion_tokens, 33);
        assert_eq!(recorded_usage[0].model, "sc-sample-model");
    }

    #[tokio::test]
    async fn chat_stream_skips_sc_when_sample_count_one() {
        let sink_typed: Arc<ScStreamSink> = Arc::new(ScStreamSink::default());
        let sink: Arc<dyn crate::metrics::MetricsSink> = sink_typed.clone();
        let provider: Arc<dyn ChatProvider> = Arc::new(ScSampleProvider {
            text: "irrelevant".to_string(),
        });
        let stream = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(sink),
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            sc_streaming_cfg(1),
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-sc-skip|hi|"),
        )
        .await
        .expect("stream init");
        use futures::StreamExt;
        let collected: Vec<String> = stream
            .map(|item| match item {
                Ok(bytes) => String::from_utf8(bytes).expect("utf-8"),
                Err(e) => panic!("stream err: {}", e.cause),
            })
            .collect()
            .await;
        assert_eq!(collected, vec!["STREAM-FALLBACK".to_string()]);
        let recorded_sc = sink_typed.sc.lock().unwrap().clone();
        assert!(
            recorded_sc.is_empty(),
            "SC must NOT fire with sample_count=1"
        );
    }

    #[tokio::test]
    async fn chat_stream_skips_sc_when_disabled() {
        let sink_typed: Arc<ScStreamSink> = Arc::new(ScStreamSink::default());
        let sink: Arc<dyn crate::metrics::MetricsSink> = sink_typed.clone();
        let provider: Arc<dyn ChatProvider> = Arc::new(ScSampleProvider {
            text: "irrelevant".to_string(),
        });
        let mut cfg = sc_streaming_cfg(5);
        cfg.enabled = false;
        let stream = handle_chat_stream(
            provider,
            String::new(),
            empty_mem(),
            SoulCache::no_op(),
            skills::SkillMatcher::keyword_only(skills::SkillsCache::empty()),
            guardrails::InputGuardrail::permissive(),
            Some(sink),
            None,
            None,
            "mock".to_string(),
            tier_routing::TierRouter::default(),
            cfg,
            crate::confidence::SelfConsistencyStats::new(),
            belief_state::BeliefStateTracker::default(),
            judge::JudgeConfig::default(),
            judge::JudgeRecorder::default(),
            judge::SessionTurnCounter::new(),
            ctx(b"sess-sc-disabled|hi|"),
        )
        .await
        .expect("stream init");
        use futures::StreamExt;
        let collected: Vec<String> = stream
            .map(|item| match item {
                Ok(bytes) => String::from_utf8(bytes).expect("utf-8"),
                Err(e) => panic!("stream err: {}", e.cause),
            })
            .collect()
            .await;
        assert_eq!(collected, vec!["STREAM-FALLBACK".to_string()]);
        let recorded_sc = sink_typed.sc.lock().unwrap().clone();
        assert!(recorded_sc.is_empty());
    }

    #[test]
    fn chunk_for_stream_splits_on_whitespace_and_preserves_text() {
        let chunks = chunk_for_stream("hello world");
        let joined: String = chunks
            .iter()
            .map(|c| std::str::from_utf8(c).unwrap().to_string())
            .collect();
        assert_eq!(joined, "hello world");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], b"hello ");
        assert_eq!(chunks[1], b"world");
    }
}
