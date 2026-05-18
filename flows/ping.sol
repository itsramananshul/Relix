// flows/ping.sol — the simplest real distributed SOL flow (M6 demo).
//
// Calls `node.health` on the peer aliased `controller` (resolved from the
// --peers TOML supplied to `relix-cli flow-run`). The remote controller runs
// the full M5 admission pipeline (identity verify → policy → handler →
// audit) before returning the structured node.health body, which arrives
// here as a UTF-8 string and is both printed and returned.

function start() -> str {
    let result: str = remote_call("controller", "node.health", "");
    print(result);
    return result;
}
