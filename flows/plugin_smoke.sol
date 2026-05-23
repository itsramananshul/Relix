// Smoke flow used during plugin-system live verification.
// Calls hello.greet on the plugin_host peer with a non-empty arg.
function start() -> str {
    let reply: str = remote_call("plugin_host", "hello.greet", "alice");
    print(reply);
    return reply;
}
