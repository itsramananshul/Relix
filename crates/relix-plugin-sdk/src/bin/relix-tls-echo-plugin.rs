//! SEC §11b: reference plugin used by the end-to-end transport
//! test, and a minimal worked example for plugin authors.
//!
//! It registers a single `echo.say` capability and serves it over
//! the hardened loopback-TLS transport. All transport material
//! (TLS cert/key, bearer) comes from the environment the host
//! loader sets; with that env absent, `serve()` fails closed and
//! this process exits non-zero — there is no plaintext fallback.

use relix_plugin_sdk::{InvokeRequest, PluginError, PluginServer};

#[tokio::main]
async fn main() {
    let mut server = PluginServer::new();
    server.register("echo.say", |req: InvokeRequest| async move {
        if req.args.is_empty() {
            return Err(PluginError::invalid_args(
                "echo.say requires non-empty args",
            ));
        }
        Ok(format!("echo:{}", req.args))
    });
    if let Err(e) = server.serve().await {
        eprintln!("relix-tls-echo-plugin: serve failed: {e}");
        std::process::exit(1);
    }
}
