// flows/chat_with_tool.sol — chat flow with a `tool.web_fetch` step (M9).
//
// Bridge-rendered template (same substitution mechanism as flows/chat_template.sol).
// Three placeholders the web bridge substitutes at request time:
//
//   {{SESSION}}    →  session id
//   {{MESSAGE}}    →  user's message
//   {{TOOL_URL}}   →  validated https URL (validate_input enforces character set)
//
// Orchestration is here, in SOL. The bridge selects the template and renders
// the substitutions; it does NOT plan or execute the fetch — the tool peer
// runs its own admission pipeline (identity → policy → SSRF check → fetch
// → audit) just like memory and ai.
//
// Order of operations (everything in SOL — no Rust glue):
//
//   1. Persist user turn (memory.write_turn).
//   2. Read recent history (memory.recent_for_session).
//   3. Fetch the URL (tool.web_fetch, capped at 16 KiB so the prompt stays small).
//   4. Build a prompt that includes the fetched body verbatim.
//   5. AI call (ai.chat) with prompt + history.
//   6. Persist assistant reply.
//
// If any step fails (e.g. tool.web_fetch returns policy_denied for an
// SSRF-rejected URL), the VM halts with VM_ERROR_SENTINEL and the bridge
// surfaces a 502/400; subsequent steps do not run — confirmed by the
// existing `first_call_failure_short_circuits_chain` test in
// crates/relix-runtime/src/flow_runner.rs.

function start() -> str {
    let user_msg: str = "{{MESSAGE}}";

    // 1. Persist user turn FIRST so recent-history readback includes it and
    //    so a crash between steps does not lose the user input.
    remote_call("memory", "memory.write_turn", "{{SESSION}}|user|" + user_msg);

    // 2. Read recent history (now includes the just-written user turn).
    let history: str = remote_call("memory", "memory.recent_for_session", "{{SESSION}}");

    // 3. Fetch external URL. Targets the capability instead of a hard-coded
    //    peer alias (M10): the dispatcher consults the bridge's manifest
    //    cache to find a peer that advertises `tool.web_fetch`. Static
    //    aliases for memory/ai elsewhere in this flow keep working.
    //    The "|16384" suffix asks the tool node to cap the body at 16 KiB.
    let fetched: str = remote_call("capability:tool.web_fetch", "tool.web_fetch", "{{TOOL_URL}}|16384");

    // 4. Build a single prompt string carrying the user message, the URL,
    //    and the fetched body verbatim. SOL string literals have no escapes
    //    (SIMP-016 alpha) so we use plain ASCII delimiters.
    let prompt: str = "user asked: " + user_msg
        + "  ---  fetched_from {{TOOL_URL}}: "
        + fetched;

    // 5. AI call: session_id | prompt | history (SIMP-016).
    let reply: str = remote_call("ai", "ai.chat", "{{SESSION}}|" + prompt + "|" + history);

    // 6. Persist assistant turn.
    remote_call("memory", "memory.write_turn", "{{SESSION}}|assistant|" + reply);

    return reply;
}
