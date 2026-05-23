// flows/chat_template.sol — bridge-rendered chat flow.
//
// Conversation history is fetched automatically by the AI node when
// `[ai.memory_peer]` is configured — no `memory.recent_for_session`
// step here. The AI node uses the session id to pull recent turns
// from the memory peer and merges them with any caller-supplied
// history field. See docs/memory.md.
//
// Substitution markers:
//   {{SESSION}}   →  session_id from POST /chat JSON
//   {{MESSAGE}}   →  message    from POST /chat JSON

function start() -> str {
    let user_msg: str = "{{MESSAGE}}";

    // 1. Persist user turn first. The AI node's auto-fetch picks
    //    this up on the next step.
    remote_call("memory", "memory.write_turn", "{{SESSION}}|user|" + user_msg);

    // 2. AI call. Wire format is `session_id|prompt|history` —
    //    we leave the history field empty and let the AI node
    //    fetch it from memory automatically.
    let reply: str = remote_call("ai", "ai.chat", "{{SESSION}}|" + user_msg + "|");

    // 3. Persist assistant turn.
    remote_call("memory", "memory.write_turn", "{{SESSION}}|assistant|" + reply);

    return reply;
}
