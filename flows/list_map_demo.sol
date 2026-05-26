// flows/list_map_demo.sol — exercises the SOL list & map
// literals plus the F6/F8 built-in surface.
//
// The flow builds a list of named subtasks, walks them with
// `for-in` to dispatch a remote_call per task, captures the
// per-task results in a map, then returns the map's
// `k=v;k=v` stringification as the flow's result.
//
// Wire format note: SIMP-016 says capability args are pipe-
// delimited strings. Building those payloads with `+ "|" +`
// concatenation still works — list / map literals don't
// replace the wire convention. They DO replace the in-flow
// data modeling we used to do with pipe-encoded strings.

function start() -> str {
    // List of work item identifiers. Heterogeneous in
    // principle; here every element is a str.
    let work: list = ["draft", "finalize", "publish"];

    // Map that we'll grow as each step completes. Empty
    // literal is valid — `map_set` adds entries
    // immutably so the seed stays put.
    let results: map = {};

    // Walk the work items. The for-in body sees each
    // element as `str` because the analyzer wires the
    // list-element type that way.
    for item in work {
        // Build a SIMP-016 wire payload for the
        // capability call. `+ "|" +` still does the
        // concat — `list_join` is for when you want a
        // structured list in your flow, not for wire
        // assembly.
        let body: str = remote_call(
            "memory",
            "memory.write_turn",
            "list-map-demo|user|step:" + item
        );

        // Stash the per-item result in the map. The new
        // `results` ref replaces the previous binding
        // (immutable update — the seed `{}` is gone now).
        results = map_set(results, item, body);
    }

    // Return a comma-joined summary of the work items
    // we touched. (SOL's analyzer rejects `str + int` so we
    // can't inline the map_len count without a typed
    // `int_to_str` built-in — noted in the parity doc as a
    // remaining gap.)
    return "demo wrote: " + list_join(work, ", ");
}
