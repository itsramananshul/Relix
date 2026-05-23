//! Example Relix plugin: `web_lookup.fetch`.
//!
//! Demonstrates how a plugin wraps a piece of external surface
//! into a relix-plugin-v1 capability. The capability takes a URL
//! and returns the first 500 chars of the response body.
//!
//! The plugin itself doesn't need to know anything about the
//! Relix mesh — the SDK handles port announcement and protocol
//! framing.

use relix_plugin_sdk::{InvokeRequest, PluginError, PluginServer};

const MAX_BODY_CHARS: usize = 500;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let mut server = PluginServer::new();
    server.register("web_lookup.fetch", move |req: InvokeRequest| {
        let http = http.clone();
        async move {
            let url = req.args.trim();
            if url.is_empty() {
                return Err(PluginError::invalid_args("missing url"));
            }
            // Only http(s) — refuse file:// and other schemes so
            // a misuse can't read local files via this plugin.
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(PluginError::invalid_args(
                    "url must start with http:// or https://",
                ));
            }
            let resp = http
                .get(url)
                .send()
                .await
                .map_err(|e| PluginError::overloaded(format!("fetch {url}: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(PluginError::internal(format!("fetch {url}: HTTP {status}")));
            }
            let body = resp
                .text()
                .await
                .map_err(|e| PluginError::internal(format!("read body: {e}")))?;
            let preview: String = body.chars().take(MAX_BODY_CHARS).collect();
            Ok(preview)
        }
    });

    tracing::info!("relix-plugin-web-lookup ready");
    server.serve().await?;
    Ok(())
}
