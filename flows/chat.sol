// flows/chat.sol — first end-to-end SOL agent flow (M7).
//
// Routing lives entirely in SOL: this file is the only place that knows
// "fetch recent → call AI → persist both turns." Rust code in the controller
// does not encode this ordering anywhere; it just dispatches the registered
// capabilities. That preserves the architectural invariant that orchestration
// is in SOL flows.
//
// The AI peer runs the M7 stub responder (`ai.chat mode = "stub"`), which
// returns a deterministic placeholder. M8 swaps the stub for an Anthropic
// call without changing this file.

function start() -> str {
    let session: str  = "chat-session";
    let user_msg: str = "hello from alice";

    // 1. Retrieve recent history for context (alpha: SOL has no varargs so
    //    the AI does not yet receive the history; the call still serves to
    //    write the audit record and prove the flow event log shape).
    let history: str = remote_call("memory", "memory.recent_for_session", "chat-session");

    // 2. Invoke the AI peer. Stub responds with a deterministic string.
    let reply: str = remote_call("ai", "ai.chat", "chat-session|" + user_msg);

    // 3. Persist both turns (user first, then assistant).
    remote_call("memory", "memory.write_turn", "chat-session|user|" + user_msg);
    remote_call("memory", "memory.write_turn", "chat-session|assistant|" + reply);

    print(reply);
    return reply;
}
