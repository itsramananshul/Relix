// flows/chat_template.sol — bridge-rendered chat flow (M8).
//
// Identical to flows/chat.sol but the literal session id and user message
// are placeholders the web bridge substitutes at request time. The bridge
// validates input characters so the substitution stays inside a single SOL
// string literal — see relix-web-bridge::validate_input.
//
// Substitution markers:
//   {{SESSION}}   →  session_id from POST /chat JSON
//   {{MESSAGE}}   →  message    from POST /chat JSON

function start() -> str {
    let user_msg: str = "{{MESSAGE}}";

    // 1. Persist user turn first.
    remote_call("memory", "memory.write_turn", "{{SESSION}}|user|" + user_msg);

    // 2. Read recent history (includes the just-written user turn).
    let history: str = remote_call("memory", "memory.recent_for_session", "{{SESSION}}");

    // 3. AI call with prompt + history per SIMP-016.
    let reply: str = remote_call("ai", "ai.chat", "{{SESSION}}|" + user_msg + "|" + history);

    // 4. Persist assistant turn.
    remote_call("memory", "memory.write_turn", "{{SESSION}}|assistant|" + reply);

    return reply;
}
