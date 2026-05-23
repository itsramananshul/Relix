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

pub mod failover;
pub mod memory_dispatcher;
pub mod provider;
pub mod router;

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
    OpenAICompatibleProvider, ProviderEntries, ProviderEntry, ProviderError,
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
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: String::new(),
            providers: ProviderEntries::new(),
            memory_peer: None,
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
pub fn register(
    bridge: &mut DispatchBridge,
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    memory_dispatcher: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>>,
) {
    let provider_for_chat = provider.clone();
    let model_for_chat = default_model.clone();
    bridge.register(
        "ai.chat",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let p = provider_for_chat.clone();
            let model = model_for_chat.clone();
            let mem = memory_dispatcher.clone();
            async move { handle_chat(p, model, mem, ctx).await }
        })),
    );
    let provider_for_embed = provider.clone();
    bridge.register(
        "ai.embed",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let p = provider_for_embed.clone();
            async move { handle_embed(p, ctx).await }
        })),
    );
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

/// Combine auto-fetched conversation history with the caller-
/// supplied `history` field on the wire. Auto-fetched lines come
/// first (they're the older context), caller-supplied lines are
/// appended after. A single trailing newline is normalised on
/// the auto-fetched block so the two segments meet cleanly.
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

async fn handle_chat(
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    memory_dispatcher: Arc<tokio::sync::OnceCell<Arc<dyn MemoryFetcher>>>,
    ctx: InvocationCtx,
) -> HandlerOutcome {
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
    // session_id | prompt | history. `history` may contain `|` so splitn(3).
    let mut parts = s.splitn(3, '|');
    let session_id = parts.next().unwrap_or("");
    let prompt = parts.next();
    let history = parts.next().unwrap_or("");
    let Some(prompt) = prompt else {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "ai.chat arg must be `session_id|prompt[|history]`".to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    };
    if session_id.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "ai.chat: session_id required".to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    }

    // Frozen-snapshot memory injection. Fetch agent + user
    // memory for the caller's subject_id once, build a labeled
    // block, and route it into ChatInput.system_prompt. The
    // dispatcher may be unset (cell empty) if the AI controller
    // wasn't configured with a memory peer or its discovery
    // hasn't finished yet — that's a silent skip per spec.
    //
    // The same dispatcher also serves automatic conversation
    // history: if `session_id` is non-empty AND the cell is
    // populated, call `memory.recent_for_session` and merge the
    // result with any caller-supplied history. Failure to fetch
    // is silent — `ai.chat` never fails because memory is
    // unavailable.
    let (system_prompt, merged_history) = if let Some(disp) = memory_dispatcher.get() {
        let subject_id = ctx.caller.subject_id.to_string();
        let sys = match disp.fetch(&subject_id).await {
            Some((agent_mem, user_mem)) => {
                memory_dispatcher::format_memory_block(&agent_mem, &user_mem)
            }
            None => None,
        };
        let auto_history = disp.fetch_history(session_id).await.unwrap_or_default();
        (sys, merge_history(&auto_history, history))
    } else {
        (None, history.to_string())
    };

    let input = ChatInput {
        session_id: session_id.to_string(),
        prompt: prompt.to_string(),
        history: merged_history,
        model: default_model,
        system_prompt,
        ..ChatInput::default()
    };
    match provider.generate_reply(input).await {
        Ok(output) => HandlerOutcome::Ok(output.text.into_bytes()),
        Err(ProviderError::Transient(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_OVERLOADED,
            cause: format!("ai.chat: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
        Err(ProviderError::Permanent(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("ai.chat: {c}"),
            retry_hint: 2,
            retry_after: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

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
            })
        }
        fn provider_name(&self) -> &'static str {
            "recording"
        }
    }

    #[tokio::test]
    async fn mock_provider_is_deterministic_with_and_without_history() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r1 = handle_chat(p.clone(), String::new(), empty_mem(), ctx(b"s1|hello|")).await;
        let r2 = handle_chat(p.clone(), String::new(), empty_mem(), ctx(b"s1|hello|")).await;
        match (r1, r2) {
            (HandlerOutcome::Ok(a), HandlerOutcome::Ok(b)) => assert_eq!(a, b),
            _ => panic!("expected both ok"),
        }
        let r3 = handle_chat(
            p,
            String::new(),
            empty_mem(),
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
        let r = handle_chat(p, String::new(), empty_mem(), ctx(b"only-session-id")).await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    #[tokio::test]
    async fn empty_session_rejected() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r = handle_chat(p, String::new(), empty_mem(), ctx(b"|hello|")).await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
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
        let r = handle_chat(rec_provider, String::new(), cell, ctx(b"s1|hello|")).await;
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
        let r = handle_chat(rec_provider, String::new(), cell, ctx(b"s1|hello|")).await;
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
        let r = handle_chat(rec_provider, String::new(), cell, ctx(b"sess1|hi|")).await;
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let captured = recorded.lock().unwrap().clone().unwrap();
        assert!(
            captured.history.is_empty(),
            "history should be empty on None fetch: {:?}",
            captured.history
        );
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
        let r = handle_chat(rec_provider, String::new(), cell, ctx(b"s1|hello|")).await;
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
        };
        match build_provider(&cfg) {
            Ok(p) => assert_eq!(p.provider_name(), "local"),
            Err(e) => panic!("local should build without key: {e}"),
        }
    }
}
