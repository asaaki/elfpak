//! Integration fixture: a production-shaped Axum service.
//!
//! It exercises everything the `web` preset promises: DNS resolution through
//! glibc, outbound HTTPS with no application-side CA configuration, a writable
//! `/tmp`, and running as a non-root user.

use axum::{Json, Router, routing::get};
use serde_json::{Value, json};
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/health", get(health))
        .route("/whoami", get(whoami))
        .route("/tmp", get(tmp))
        .route("/dns", get(dns))
        .route("/outbound", get(outbound))
        .route("/outbound/pinned", get(outbound_pinned));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind 8080");
    eprintln!("listening on {addr}");
    axum::serve(listener, app).await.expect("serve");
}

async fn health() -> &'static str {
    "ok"
}

/// Reads the real uid/gid from procfs, which needs no extra libraries.
async fn whoami() -> Json<Value> {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("?")
            .to_string()
    };
    Json(json!({ "uid": field("Uid:"), "gid": field("Gid:") }))
}

async fn tmp() -> Json<Value> {
    let path = "/tmp/elfpak-smoke";
    match std::fs::write(path, b"written by the smoke test")
        .and_then(|()| std::fs::read_to_string(path))
    {
        Ok(contents) => Json(json!({ "ok": true, "bytes": contents.len() })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Uses the system resolver, so it depends on nsswitch and glibc NSS.
async fn dns() -> Json<Value> {
    match tokio::net::lookup_host("example.com:443").await {
        Ok(addrs) => {
            let addrs: Vec<String> = addrs.map(|a| a.to_string()).collect();
            Json(json!({ "ok": !addrs.is_empty(), "addresses": addrs }))
        }
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Outbound HTTPS the way an ordinary application writes it: no CA
/// configuration whatsoever.
///
/// The client trusts the platform store, and no roots are compiled into the
/// binary, so this succeeds only because the bundle placed the system CA
/// certificates in the image. Applications should not have to think about it.
async fn outbound() -> Json<Value> {
    match reqwest::get("https://example.com").await {
        Ok(response) => Json(json!({ "ok": true, "status": response.status().as_u16() })),
        Err(e) => Json(json!({ "ok": false, "error": e.to_string() })),
    }
}

const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// The opt-in variant, for applications that want to trust exactly one bundle
/// and nothing else. Shown here to prove the bundled file is usable directly.
async fn outbound_pinned() -> Json<Value> {
    match pinned_get("https://example.com").await {
        Ok(status) => Json(json!({ "ok": true, "status": status })),
        Err(e) => Json(json!({ "ok": false, "error": e })),
    }
}

async fn pinned_get(url: &str) -> Result<u16, String> {
    let pem = std::fs::read(CA_BUNDLE).map_err(|e| format!("{CA_BUNDLE}: {e}"))?;
    let roots = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| e.to_string())?;
    // `tls_certs_only` disables the platform store and every built-in root.
    let client = reqwest::Client::builder()
        .tls_certs_only(roots)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    Ok(response.status().as_u16())
}
