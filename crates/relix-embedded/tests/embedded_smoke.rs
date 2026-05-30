//! End-to-end embedded smoke tests using the in-tree `MockProvider`.

use std::sync::Arc;

use relix_embedded::{ChatInput, MemoryIngestInput, MemorySearchInput, RelixEmbedded};
use relix_runtime::nodes::ai::provider::MockProvider;
use tempfile::tempdir;

async fn mock_runtime() -> RelixEmbedded {
    RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .build()
        .await
        .expect("build embedded runtime with mock provider")
}

#[tokio::test]
async fn builder_requires_a_provider() {
    let result = RelixEmbedded::builder().build().await;
    let Err(err) = result else {
        panic!("should reject build without provider");
    };
    let msg = err.to_string();
    assert!(
        msg.contains("provider"),
        "error should call out the missing provider; got {msg:?}"
    );
}

#[tokio::test]
async fn builder_succeeds_with_minimal_config() {
    let r = mock_runtime().await;
    assert!(r.default_model().is_empty());
    assert_eq!(r.provider().provider_name(), "mock");
}

#[tokio::test]
async fn chat_returns_mock_provider_reply_and_persists_turns() {
    let r = mock_runtime().await;
    let response = r
        .chat(ChatInput {
            session_id: "user-1".into(),
            message: "hello world".into(),
            agent_name: "assistant".into(),
            model: None,
            system_prompt: None,
            tenant_id: None,
        })
        .await
        .expect("chat ok");
    assert_eq!(response.provider, "mock");
    assert!(response.text.contains("hello world"));
    // Two turns (user + assistant) land in the in-process history.
    assert_eq!(r.session_turn_count("user-1"), 2);
    // And both persist to the memory store as raw rows. The store's
    // text_search is the simplest cross-check available.
    let hits = r
        .memory_store()
        .text_search("hello world", 10)
        .expect("text_search ok");
    assert!(
        hits.iter()
            .any(|h| h.text.starts_with("user:") && h.text.contains("hello world")),
        "should have a user turn matching the prompt; got {hits:?}"
    );
}

#[tokio::test]
async fn chat_history_grows_across_sequential_turns_in_same_session() {
    let r = mock_runtime().await;
    for n in 0..3 {
        r.chat(ChatInput {
            session_id: "u".into(),
            message: format!("msg {n}"),
            agent_name: "alpha".into(),
            model: None,
            system_prompt: None,
            tenant_id: None,
        })
        .await
        .expect("chat ok");
    }
    // 3 prompts × (user + assistant) = 6 turns.
    assert_eq!(r.session_turn_count("u"), 6);
}

#[tokio::test]
async fn chat_rejects_empty_session_id_and_empty_message() {
    let r = mock_runtime().await;
    let err = r
        .chat(ChatInput {
            session_id: "".into(),
            message: "hi".into(),
            agent_name: "a".into(),
            model: None,
            system_prompt: None,
            tenant_id: None,
        })
        .await
        .expect_err("empty session_id should fail");
    assert!(format!("{err}").contains("session_id"));

    let err = r
        .chat(ChatInput {
            session_id: "u".into(),
            message: "".into(),
            agent_name: "a".into(),
            model: None,
            system_prompt: None,
            tenant_id: None,
        })
        .await
        .expect_err("empty message should fail");
    assert!(format!("{err}").contains("message"));
}

#[tokio::test]
async fn memory_ingest_chunks_and_search_finds_a_keyword_in_the_subject() {
    let r = mock_runtime().await;
    let body = "Pricing tier A is $49 per month.\n\n\
                Pricing tier B is $99 per month with priority support.\n\n\
                Cancellations process at the end of the billing cycle.";
    let result = r
        .memory_ingest_document(MemoryIngestInput {
            subject_id: "u1".into(),
            content: body.into(),
            content_type: "markdown".into(),
            source: "notes.md".into(),
            tenant_id: None,
        })
        .await
        .expect("ingest ok");
    assert!(result.chunks_created >= 1);
    assert_eq!(result.subject_id, "u1");
    assert_eq!(result.content_type, "markdown");

    let hits = r
        .memory_search(MemorySearchInput {
            query: "Pricing tier B".into(),
            subject_id: "u1".into(),
            limit: 5,
            tenant_id: None,
        })
        .await
        .expect("search ok");
    assert!(!hits.is_empty(), "should find at least one hit");
    assert!(
        hits.iter().any(|h| h.text.contains("$99")),
        "should match the chunk with the price; got {hits:?}"
    );
    assert!(hits.iter().all(|h| h.source == "u1"));
}

#[tokio::test]
async fn memory_search_filters_to_subject_when_set() {
    let r = mock_runtime().await;
    r.memory_ingest_document(MemoryIngestInput {
        subject_id: "alice".into(),
        content: "Alice loves pricing tier B".into(),
        content_type: "markdown".into(),
        source: "n.md".into(),
        tenant_id: None,
    })
    .await
    .expect("ingest alice");
    r.memory_ingest_document(MemoryIngestInput {
        subject_id: "bob".into(),
        content: "Bob also loves pricing tier B".into(),
        content_type: "markdown".into(),
        source: "n.md".into(),
        tenant_id: None,
    })
    .await
    .expect("ingest bob");

    let only_alice = r
        .memory_search(MemorySearchInput {
            query: "tier B".into(),
            subject_id: "alice".into(),
            limit: 10,
            tenant_id: None,
        })
        .await
        .expect("search ok");
    assert!(only_alice.iter().all(|h| h.source == "alice"));
    assert!(only_alice.iter().any(|h| h.text.contains("Alice")));
    assert!(!only_alice.iter().any(|h| h.text.contains("Bob")));
}

#[tokio::test]
async fn memory_ingest_rejects_unsupported_content_type() {
    let r = mock_runtime().await;
    let err = r
        .memory_ingest_document(MemoryIngestInput {
            subject_id: "u".into(),
            content: "%PDF-...".into(),
            content_type: "pdf".into(),
            source: "x.pdf".into(),
            tenant_id: None,
        })
        .await
        .expect_err("pdf is not embedded-mode supported");
    assert!(format!("{err}").contains("content_type"));
}

#[tokio::test]
async fn memory_ingest_rejects_empty_subject_or_content() {
    let r = mock_runtime().await;
    assert!(
        r.memory_ingest_document(MemoryIngestInput {
            subject_id: "".into(),
            content: "x".into(),
            content_type: "markdown".into(),
            source: "n".into(),
            tenant_id: None,
        })
        .await
        .is_err()
    );
    assert!(
        r.memory_ingest_document(MemoryIngestInput {
            subject_id: "u".into(),
            content: "  ".into(),
            content_type: "markdown".into(),
            source: "n".into(),
            tenant_id: None,
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn two_runtimes_with_different_db_paths_are_isolated() {
    let dir = tempdir().expect("tempdir");
    let alpha = RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .memory_db(dir.path().join("alpha.db"))
        .build()
        .await
        .expect("alpha");
    let beta = RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .memory_db(dir.path().join("beta.db"))
        .build()
        .await
        .expect("beta");

    alpha
        .memory_ingest_document(MemoryIngestInput {
            subject_id: "u".into(),
            content: "alpha-only payload".into(),
            content_type: "markdown".into(),
            source: "n".into(),
            tenant_id: None,
        })
        .await
        .expect("ingest alpha");

    let alpha_hits = alpha
        .memory_search(MemorySearchInput {
            query: "alpha-only".into(),
            subject_id: "".into(),
            limit: 5,
            tenant_id: None,
        })
        .await
        .expect("alpha search");
    let beta_hits = beta
        .memory_search(MemorySearchInput {
            query: "alpha-only".into(),
            subject_id: "".into(),
            limit: 5,
            tenant_id: None,
        })
        .await
        .expect("beta search");

    assert!(!alpha_hits.is_empty(), "alpha should see its own row");
    assert!(beta_hits.is_empty(), "beta must be isolated");
}

#[tokio::test]
async fn memory_db_persists_across_runtime_instances_at_the_same_path() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("persistent.db");

    {
        let r1 = RelixEmbedded::builder()
            .provider(Arc::new(MockProvider))
            .memory_db(&path)
            .build()
            .await
            .expect("first runtime");
        r1.memory_ingest_document(MemoryIngestInput {
            subject_id: "u".into(),
            content: "persistent payload across restarts".into(),
            content_type: "markdown".into(),
            source: "n".into(),
            tenant_id: None,
        })
        .await
        .expect("ingest");
    }

    let r2 = RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .memory_db(&path)
        .build()
        .await
        .expect("second runtime");
    let hits = r2
        .memory_search(MemorySearchInput {
            query: "persistent payload".into(),
            subject_id: "".into(),
            limit: 5,
            tenant_id: None,
        })
        .await
        .expect("search");
    assert!(!hits.is_empty(), "second runtime must see first's writes");
}

// ─── PART 6: tenant-isolation in the embedded runtime ────────

async fn isolated_runtime() -> RelixEmbedded {
    RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .tenant_isolation(true)
        .build()
        .await
        .expect("build embedded runtime with tenant_isolation")
}

#[tokio::test]
async fn fix_part6_tenant_isolation_off_by_default() {
    // Pre-PART-6 callers see no behaviour change.
    let r = mock_runtime().await;
    assert!(!r.tenant_isolation_enabled());
    assert!(r.default_tenant_id().is_none());
}

#[tokio::test]
async fn fix_part6_builder_threads_default_tenant_id_and_isolation_flag() {
    let r = RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .default_tenant_id("acme")
        .tenant_isolation(true)
        .build()
        .await
        .expect("build");
    assert!(r.tenant_isolation_enabled());
    assert_eq!(r.default_tenant_id(), Some("acme"));
}

#[tokio::test]
async fn fix_part6_default_tenant_id_filters_whitespace_only_input() {
    // `default_tenant_id("   ")` should NOT bind an empty
    // tenant; it's equivalent to "no default set".
    let r = RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .default_tenant_id("   ")
        .build()
        .await
        .expect("build");
    assert!(r.default_tenant_id().is_none());
}

#[tokio::test]
async fn fix_part6_chat_fails_closed_on_missing_tenant_in_isolation_mode() {
    let r = isolated_runtime().await;
    let err = r
        .chat(ChatInput {
            session_id: "u".into(),
            message: "hi".into(),
            agent_name: "alpha".into(),
            model: None,
            system_prompt: None,
            tenant_id: None,
        })
        .await
        .expect_err("chat should reject missing tenant");
    let msg = err.to_string();
    assert!(
        msg.contains("tenant_id required") && msg.contains("chat"),
        "expected MissingTenant chat error, got: {msg}"
    );
}

#[tokio::test]
async fn fix_part6_chat_per_call_tenant_overrides_default() {
    let r = RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .default_tenant_id("acme")
        .tenant_isolation(true)
        .build()
        .await
        .expect("build");
    let response = r
        .chat(ChatInput {
            session_id: "u".into(),
            message: "ping".into(),
            agent_name: "alpha".into(),
            model: None,
            system_prompt: None,
            tenant_id: Some("globex".into()),
        })
        .await
        .expect("chat with per-call tenant ok");
    assert!(response.text.contains("ping"));
}

#[tokio::test]
async fn fix_part6_chat_default_tenant_satisfies_isolation_gate() {
    let r = RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))
        .default_tenant_id("acme")
        .tenant_isolation(true)
        .build()
        .await
        .expect("build");
    // No per-call tenant → builder default takes over → gate passes.
    let response = r
        .chat(ChatInput {
            session_id: "u".into(),
            message: "ping".into(),
            agent_name: "alpha".into(),
            model: None,
            system_prompt: None,
            tenant_id: None,
        })
        .await
        .expect("default tenant satisfies the gate");
    assert!(response.text.contains("ping"));
}

#[tokio::test]
async fn fix_part6_memory_ingest_and_search_isolate_per_tenant() {
    let r = isolated_runtime().await;
    // Tenant A ingests something Tenant B should NOT see.
    r.memory_ingest_document(MemoryIngestInput {
        subject_id: "user".into(),
        content: "acme tenant payload".into(),
        content_type: "txt".into(),
        source: "doc-a".into(),
        tenant_id: Some("acme".into()),
    })
    .await
    .expect("ingest A");
    r.memory_ingest_document(MemoryIngestInput {
        subject_id: "user".into(),
        content: "globex tenant payload".into(),
        content_type: "txt".into(),
        source: "doc-b".into(),
        tenant_id: Some("globex".into()),
    })
    .await
    .expect("ingest B");
    // Search as A — sees only A's chunk.
    let a_hits = r
        .memory_search(MemorySearchInput {
            query: "tenant payload".into(),
            subject_id: "".into(),
            limit: 10,
            tenant_id: Some("acme".into()),
        })
        .await
        .expect("search A");
    assert_eq!(a_hits.len(), 1);
    assert!(a_hits[0].text.contains("acme"));
    // Search as B — sees only B's chunk.
    let b_hits = r
        .memory_search(MemorySearchInput {
            query: "tenant payload".into(),
            subject_id: "".into(),
            limit: 10,
            tenant_id: Some("globex".into()),
        })
        .await
        .expect("search B");
    assert_eq!(b_hits.len(), 1);
    assert!(b_hits[0].text.contains("globex"));
}

#[tokio::test]
async fn fix_part6_memory_search_fails_closed_on_missing_tenant() {
    let r = isolated_runtime().await;
    let err = r
        .memory_search(MemorySearchInput {
            query: "anything".into(),
            subject_id: "".into(),
            limit: 5,
            tenant_id: None,
        })
        .await
        .expect_err("missing tenant should reject");
    assert!(err.to_string().contains("memory_search"));
}

#[tokio::test]
async fn fix_part6_memory_ingest_fails_closed_on_missing_tenant() {
    let r = isolated_runtime().await;
    let err = r
        .memory_ingest_document(MemoryIngestInput {
            subject_id: "user".into(),
            content: "anything".into(),
            content_type: "txt".into(),
            source: "doc".into(),
            tenant_id: None,
        })
        .await
        .expect_err("missing tenant should reject");
    assert!(err.to_string().contains("memory_ingest_document"));
}

#[tokio::test]
async fn fix_part6_legacy_callers_unaffected_when_isolation_off() {
    // With tenant_isolation = false (default), every operation
    // accepts a missing tenant id and behaves as pre-PART-6.
    let r = mock_runtime().await;
    r.chat(ChatInput {
        session_id: "u".into(),
        message: "hi".into(),
        agent_name: "alpha".into(),
        model: None,
        system_prompt: None,
        tenant_id: None,
    })
    .await
    .expect("legacy chat ok");
    r.memory_ingest_document(MemoryIngestInput {
        subject_id: "user".into(),
        content: "legacy payload".into(),
        content_type: "txt".into(),
        source: "doc".into(),
        tenant_id: None,
    })
    .await
    .expect("legacy ingest ok");
    let hits = r
        .memory_search(MemorySearchInput {
            query: "legacy payload".into(),
            subject_id: "".into(),
            limit: 10,
            tenant_id: None,
        })
        .await
        .expect("legacy search ok");
    assert!(!hits.is_empty());
}
