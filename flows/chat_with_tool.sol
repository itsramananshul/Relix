// flows/chat_with_tool.sol — chat flow with web.fetch tool integration.
//
// Alpha tool-call convention (SIMP-010): AI replies containing
//   <tool>web.fetch url="..."</tool>
// are detected by the controller's flow runner, which calls the tool peer,
// splices the result into the conversation, and re-invokes ai.chat.
//
// The text-marker convention is a one-evening implementation that exercises
// the architecture (AI -> SOL -> tool node -> AI). Real Anthropic tool-use
// integration is post-alpha.

import Memory.RecentForSession
import Memory.WriteTurn
import Ai.Chat
import Tool.WebFetch

fn chat_with_tool(session_id: string, user_text: string) -> string {
    let history: string = remote_call("memory", "memory.recent_for_session", session_id);
    let prompt: string = history + "\n\nuser: " + user_text;

    // First AI pass — may emit <tool>...</tool> marker.
    let first_reply: string = remote_call("ai", "ai.chat", prompt);

    // (The host coordinator inspects first_reply for the marker and, if present,
    // performs: tool.web_fetch -> ai.chat re-prompt -> final reply. The SOL flow
    // surface here is what the host re-invokes with the tool result inlined.)
    //
    // For the alpha, this flow exists as a placeholder; the host's
    // chat_with_tool dispatcher (relix-runtime::nodes::web_bridge) handles the
    // detect/splice loop and writes the same flow events to the log so audit
    // still reflects the full sequence.

    remote_call("memory", "memory.write_turn", session_id + "|user|" + user_text);
    remote_call("memory", "memory.write_turn", session_id + "|assistant|" + first_reply);
    return first_reply;
}
