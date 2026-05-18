// flows/chained_health.sol — multi-peer sequential SOL orchestration (M6/S7).
//
// Two `remote_call`s in order: first to the memory peer, then to the ai
// peer. Both call the same built-in capability (`node.health`) so this flow
// can run without the memory / AI node implementations being complete; M7
// swaps the methods for the real `memory.search` / `ai.chat` capabilities.
//
// Each `remote_call` runs the responder's full M5 admission pipeline
// (identity → policy → handler → audit) independently. The flow log records
// FlowStarted → RemoteCallIssued(memory) → RemoteCallCompleted(memory) →
// RemoteCallIssued(ai) → RemoteCallCompleted(ai) → FlowCompleted, in order.
// trace_id is constant across all events (carried by the flow); each call
// gets a fresh request_id that correlates back to the responder's audit.

function start() -> str {
    let memory: str = remote_call("memory", "node.health", "");
    let ai:     str = remote_call("ai",     "node.health", "");
    // SOL strings are taken verbatim — there is no `\n` escape — and the
    // bodies already end with newlines. A plain visible separator is enough.
    let sep: str = "--- ai ---";
    let result: str = memory + sep + ai;
    print(result);
    return result;
}
