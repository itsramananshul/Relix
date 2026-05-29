//! Cross-module integration tests for the channel scaffold.
//!
//! These exercise the pieces that survive without the live
//! HTTPS client: identity derivation, the session-store
//! mapping, and the mock Bot API contract. The live controller
//! binary's tests live separately when it lands.

use relix_telegram::{
    BotApi, IncomingMessage, OutgoingMessage, SessionStorage, SessionStore, derive_channel_subject,
    mock::MockBotApi,
};

#[tokio::test]
async fn inbound_message_maps_to_subject_and_session() {
    let api = MockBotApi::new();
    let session = SessionStore::new();

    // Operator publishes a message into the channel.
    let inbound = IncomingMessage {
        update_id: 1,
        chat_id: 100,
        user_id: 42,
        message_id: 7,
        username: "alice".into(),
        text: "do the thing".into(),
        voice_file_id: None,
        callback_query_id: None,
    };
    api.push_update(inbound.clone());

    // Channel loop fetches the update.
    let batch = api.get_updates(0).await.unwrap();
    assert_eq!(batch.len(), 1);
    let msg = &batch[0];

    // Derive the channel subject + (mock) record a task_id.
    let subject = derive_channel_subject(msg.chat_id, msg.user_id);
    assert_eq!(subject.user_id, 42);
    assert_eq!(subject.chat_id, 100);

    let task_id = "deadbeef".to_string();
    session.record(msg.chat_id, msg.message_id, task_id.clone());

    // Some time later (after FlowRunner completes), the async
    // delivery path looks up the chat to reply to.
    let resolved = session
        .lookup(msg.chat_id, msg.message_id)
        .expect("session preserved");
    assert_eq!(resolved, task_id);

    // Send the reply via the BotApi trait surface.
    api.send_message(&OutgoingMessage {
        chat_id: msg.chat_id,
        reply_to_message_id: msg.message_id,
        text: "done!".into(),
        parse_mode: None,
        reply_markup: None,
    })
    .await
    .unwrap();

    let sent = api.sent_messages();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].reply_to_message_id, 7);
    assert_eq!(sent[0].text, "done!");

    // Forget the mapping after delivery.
    session.forget(msg.chat_id, msg.message_id);
    assert!(session.is_empty());
}

#[test]
fn channel_subject_handle_is_log_friendly() {
    let s = derive_channel_subject(100, 42);
    let h = s.display_handle();
    assert!(h.contains("telegram"));
    assert!(h.contains("42"));
    assert!(h.contains("100"));
    // Must not include line-breaking characters that would
    // corrupt log lines.
    assert!(!h.contains('\n'));
    assert!(!h.contains('\t'));
}
