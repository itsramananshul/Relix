// flows/chat.sol — canonical Relix alpha agent flow.
//
// Signature (logical, alpha): chat(session_id: string, user_text: string) -> string
//
// Behavior:
//   1. Retrieve recent conversation history from the Memory peer.
//   2. Invoke the AI peer's ai.chat with the assembled prompt.
//   3. Stream the AI's response back to the caller and accumulate it.
//   4. Persist the user turn and the assistant turn to the Memory peer.
//
// Alpha simplifications (SIMP-001, SIMP-006): SOL `remote_call` is synchronous;
// streaming is consumed via a yield loop that blocks the VM thread. Imports here
// are placeholders — until the SOL compiler's remote_call resolution lands in M6,
// the controller's dispatch bridge invokes the equivalent Rust-coded path.

import Memory.RecentForSession
import Memory.WriteTurn
import Ai.Chat

fn chat(session_id: string, user_text: string) -> string {
    // 1. Recent history (alpha: returns concatenated history as a string).
    let history: string = remote_call("memory", "memory.recent_for_session", session_id);

    // 2. Compose the prompt for the AI.
    let prompt: string = history + "\n\nuser: " + user_text;

    // 3. Invoke AI; stream consumption returns the accumulated reply.
    let reply: string = remote_call("ai", "ai.chat", prompt);

    // 4. Persist both turns.
    remote_call("memory", "memory.write_turn", session_id + "|user|" + user_text);
    remote_call("memory", "memory.write_turn", session_id + "|assistant|" + reply);

    return reply;
}
