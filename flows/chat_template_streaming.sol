// flows/chat_template_streaming.sol — bridge-rendered chat flow
// for `POST /v1/chat/completions` with `stream: true`.
//
// Differs from chat_template.sol only in the dispatch primitive:
// `remote_call_stream("ai", "ai.chat.stream", ...)` instead of
// `remote_call("ai", "ai.chat", ...)`. Same wire format
// (`session_id|prompt|history`); the AI node's `ai.chat.stream`
// runs the same admission pre-flight as `ai.chat` (guardrails,
// memory + RAG, soul, skills) and pipes tokens back over a
// `/relix/rpc/stream/1` substream.
//
// The bridge wires a chunk observer into the flow runner that
// forwards every token to the open SSE response BEFORE the VM
// finishes collecting — the HTTP client sees tokens as they
// arrive from the provider, not after the full response is
// materialised.
//
// Substitution markers:
//   {{SESSION}}   →  session_id from POST /chat JSON
//   {{MESSAGE}}   →  message    from POST /chat JSON

function start() -> str {
    let user_msg: str = "{{MESSAGE}}";

    // 1. Persist user turn first. The AI node's auto-fetch
    //    picks this up on the next streaming step.
    remote_call("memory", "memory.write_turn", "{{SESSION}}|user|" + user_msg);

    // 2. Streaming AI call. Wire format identical to
    //    `ai.chat`. The flow runner's chunk observer fires
    //    for every token as it arrives — the bridge's SSE
    //    response writer ships each chunk to the HTTP
    //    client immediately. The VM still blocks until the
    //    full body is collected so the post-call memory
    //    write below sees the complete reply.
    let reply: str = remote_call_stream("ai", "ai.chat.stream", "{{SESSION}}|" + user_msg + "|");

    // 3. Persist assistant turn (full body, post-stream).
    remote_call("memory", "memory.write_turn", "{{SESSION}}|assistant|" + reply);

    return reply;
}
