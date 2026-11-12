//! Optional TLS support: automatic certificate issuance and renewal via ACME.
//!
//! `--ssl <domain>` makes the service obtain a Let's Encrypt certificate on its own and
//! renew it in the background, so a cheap VPS needs no certbot, no cron job and no
//! reverse proxy just to serve HTTPS.
//!
//! The challenge used is TLS-ALPN-01, which means port 443 has to reach this process
//! directly. Behind a reverse proxy that terminates TLS, use the proxy's own ACME
//! support (Caddy, nginx + certbot) instead — see `docs/self-host/`.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::Router;
use rustls_acme::caches::DirCache;
use rustls_acme::AcmeConfig;
use tokio_stream::StreamExt;

/// Serve the application over HTTPS, managing the certificate with ACME.
pub async fn serve(
    app: Router,
    domain: String,
    email: Option<String>,
    cache_dir: PathBuf,
    addr: SocketAddr,
) -> Result<()> {
    std::fs::create_dir_all(&cache_dir).with_context(|| {
        format!("creating the ACME cache directory {}", cache_dir.display())
    })?;

    let mut state = AcmeConfig::new([domain.clone()])
        .contact(
            email
                .map(|email| format!("mailto:{email}"))
                .into_iter()
                .collect::<Vec<_>>(),
        )
        .cache_option(Some(DirCache::new(cache_dir.clone())))
        // Let's Encrypt production.
        .directory_lets_encrypt(true)
        .state();

    let acceptor = state.axum_acceptor(state.default_rustls_config());

    // Drive the ACME state machine: this is what acquires the certificate and renews it
    // before it expires. The server keeps serving while this runs.
    tokio::spawn(async move {
        while let Some(event) = state.next().await {
            match event {
                Ok(event) => tracing::info!(event = ?format!("{event:?}"), "acme"),
                Err(err) => tracing::error!(error = ?format!("{err:?}"), "acme"),
            }
        }
    });

    tracing::info!(domain, ?addr, "serving https, requesting a certificate from Let's Encrypt");

    axum_server::bind(addr)
        .acceptor(acceptor)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .context("serving https")
}
