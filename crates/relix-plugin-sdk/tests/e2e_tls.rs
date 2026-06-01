//! SEC §11b — end-to-end proof that the two halves of the hardened
//! plugin transport meet: a REAL plugin built from `relix-plugin-sdk`
//! (the `relix-tls-echo-plugin` bin) is launched by the PRODUCTION
//! `relix-runtime` loader, becomes healthy, and is invoked
//! successfully over loopback TLS. Also proves a wrong bearer is
//! rejected and an unpinned cert fails the handshake against that
//! same live plugin.
//!
//! The loader fails closed on platforms where the sandbox cannot be
//! enforced (e.g. Windows) when caps are configured; this test uses
//! a no-cap `SandboxLimits` because it exercises the TRANSPORT, not
//! the sandbox.

use std::time::Duration;

use relix_runtime::plugin::{
    InvokeRequest, PluginDispatcher, PluginEndpoint, PluginInvokeError, PluginLoader,
    PluginManifest, SandboxLimits,
};

const PLUGIN_BIN: &str = env!("CARGO_BIN_EXE_relix-tls-echo-plugin");

fn no_caps() -> SandboxLimits {
    // No resource caps requested → nothing to enforce → the loader
    // does not fail closed on non-Unix. We are testing transport.
    SandboxLimits {
        max_memory_mb: 0,
        max_cpu_secs: 0,
        max_open_fds: 0,
    }
}

/// Write a plugin.toml in a fresh tempdir pointing at the real
/// SDK-built echo plugin, and load+validate it.
fn manifest_for_echo(dir: &std::path::Path) -> (PluginManifest, std::path::PathBuf) {
    // TOML literal string (single quotes) so Windows backslashes in
    // the binary path need no escaping.
    let toml = format!(
        r#"
[plugin]
name        = "echo-plugin"
version     = "0.1.0"
description = "reference TLS echo plugin"

[[plugin.capabilities.provides]]
method      = "echo.say"
description = "echoes its args back"
risk_level  = "low"

[plugin.runtime]
kind                = "subprocess"
binary              = '{bin}'
protocol            = "relix-plugin-v1"
invoke_timeout_secs = 30
"#,
        bin = PLUGIN_BIN
    );
    let path = dir.join("plugin.toml");
    std::fs::write(&path, toml).unwrap();
    let manifest = PluginManifest::load_from_path(&path).expect("manifest loads");
    (manifest, path)
}

fn req(method: &str, args: &str) -> InvokeRequest {
    InvokeRequest {
        method: method.to_string(),
        args: args.to_string(),
        trace_id: "trace".to_string(),
        request_id: "req".to_string(),
        caller_subject_id: "subject".to_string(),
        deadline_unix: 0,
    }
}

#[tokio::test]
async fn real_sdk_plugin_loads_and_invokes_over_tls() {
    // SEC §11b criterion 2: real SDK plugin, launched by the runtime
    // loader, completes the health probe, and is invoked over TLS.
    let dir = tempfile::tempdir().unwrap();
    let (manifest, manifest_path) = manifest_for_echo(dir.path());

    let loaded = PluginLoader::spawn(manifest, manifest_path, 15, 15, no_caps())
        .await
        .expect("plugin launches + becomes healthy over hardened TLS transport");

    // Invoke over the hardened transport.
    let out = loaded
        .dispatcher
        .invoke(req("echo.say", "hello-tls"))
        .await
        .expect("invoke succeeds over TLS");
    assert_eq!(out, "echo:hello-tls");

    loaded.shutdown().await;
}

#[tokio::test]
async fn unpinned_cert_fails_handshake_against_live_plugin() {
    // SEC §11b criterion 3 (cert half): against the SAME live
    // plugin, a dispatcher pinned to a DIFFERENT (unpinned) cert
    // cannot complete the TLS handshake — proving the channel is
    // authenticated, not just encrypted. (The wrong-bearer/401 half
    // is proven over TLS by the in-crate unit test
    // `invoke_with_wrong_bearer_rejected` against the live SDK
    // server.)
    let dir = tempfile::tempdir().unwrap();
    let (manifest, manifest_path) = manifest_for_echo(dir.path());
    let loaded = PluginLoader::spawn(manifest, manifest_path, 15, 15, no_caps())
        .await
        .expect("plugin launches");

    let address = loaded.dispatcher.endpoint_address().to_string();
    let (wrong_cert_der, _k) = mint_unpinned_cert();
    let unpinned = PluginDispatcher::connect(
        PluginEndpoint::new(address, wrong_cert_der),
        2,
        "any-bearer".to_string(),
    );
    for _ in 0..10 {
        assert!(
            !matches!(unpinned.health().await, Ok(true)),
            "handshake succeeded against the live plugin with an UNPINNED cert"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let err = unpinned.invoke(req("echo.say", "x")).await.unwrap_err();
    assert!(
        matches!(err, PluginInvokeError::Transport(_)),
        "expected TLS transport error with unpinned cert, got {err:?}"
    );

    loaded.shutdown().await;
}

/// A self-signed cert NOT trusted by the live plugin (different
/// keypair). Used to prove pinning rejects an impostor cert.
fn mint_unpinned_cert() -> (Vec<u8>, Vec<u8>) {
    use rcgen::{CertificateParams, KeyPair, SanType};
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.subject_alt_names = vec![SanType::IpAddress(std::net::IpAddr::V4(
        std::net::Ipv4Addr::new(127, 0, 0, 1),
    ))];
    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.der().to_vec(), key_pair.serialize_der())
}
