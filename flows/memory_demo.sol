// flows/memory_demo.sol — first SOL flow against a real memory node (M7).
//
// Writes one user turn and one assistant turn to a fresh session, then
// reads the recent history back. All three `remote_call`s hit a real
// SQLite + FTS5 backend on the memory peer, behind the M5 admission
// pipeline (identity → policy → handler → audit).
//
// SOL string args use `|` as the field separator per SIMP-016 because
// SOL strings are taken verbatim (no JSON or CBOR plumbing yet). The
// memory node parses with splitn(3) so the body field may contain `|`.

function start() -> str {
    let session: str = "demo-session";

    // Write a user turn, then an assistant turn.
    let w1: str = remote_call("memory", "memory.write_turn", "demo-session|user|hello memory");
    let w2: str = remote_call("memory", "memory.write_turn", "demo-session|assistant|hi back");

    // Read the recent history.
    let history: str = remote_call("memory", "memory.recent_for_session", "demo-session");

    print(history);
    return history;
}
