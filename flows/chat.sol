// flows/chat.sol — conversational agent orchestration (M7).
//
// Routing lives entirely in SOL: this file is the only place that knows
// the order of operations. Rust code in the controller does not encode
// this ordering anywhere; it just dispatches the registered capabilities.
// That preserves the architectural invariant that orchestration is in SOL
// flows, not in Rust/Python glue.
//
// The AI peer's provider is selected by config (`[ai] provider = "mock"`
// or `"anthropic"`). The SOL flow does not change between providers —
// SIMP-016 says `ai.chat` is `session_id|prompt|history → str`.

function start() -> str {
    let session: str  = "chat-session";
    let user_msg: str = "hello from alice";

    // Conversational state machine:
    //   1) persist the user turn FIRST so recent-history readback
    //      naturally includes it, and so a crash mid-flow does not
    //      lose the user input;
    //   2) read recent history (alpha default N=10, oldest first);
    //   3) hand history + new prompt to the AI peer;
    //   4) persist the assistant reply.

    // 1. Persist user turn.
    remote_call("memory", "memory.write_turn", "chat-session|user|" + user_msg);

    // 2. Read history (now includes the just-written user turn).
    let history: str = remote_call("memory", "memory.recent_for_session", "chat-session");

    // 3. AI call: prompt + history concatenated per SIMP-016 string contract.
    let reply: str = remote_call("ai", "ai.chat", "chat-session|" + user_msg + "|" + history);

    // 4. Persist assistant turn.
    remote_call("memory", "memory.write_turn", "chat-session|assistant|" + reply);

    print(reply);
    return reply;
}
