//! HTTP server for Nix binary cache protocol

mod handlers;
mod peer_nar;
mod routes;
mod state;

use std::sync::Arc;

use anyhow::Result;
use ogygia_nixutils::Nix;
pub use routes::create_router;
pub use state::AppState;
use tokio_util::sync::CancellationToken;
use tower_http::trace::DefaultMakeSpan;
use tower_http::trace::DefaultOnResponse;
use tower_http::trace::TraceLayer;
use tracing::Level;

use crate::bloom::local::LocalBloom;
use crate::bloom::peers::PeerBlooms;
use crate::config::Config;
use crate::nix::cache::NarCache;

/// Start HTTP servers on all configured listen addresses with graceful shutdown.
pub async fn start(
    config: Arc<Config>,
    local_bloom: Arc<LocalBloom>,
    peer_blooms: Arc<PeerBlooms>,
    http_client: reqwest::Client,
    nar_cache: Arc<NarCache>,
    nix: Nix,
    token: CancellationToken,
) -> Result<()> {
    let state = Arc::new(AppState {
        config: config.clone(),
        local_bloom,
        peer_blooms,
        http_client,
        nar_cache,
        nix,
    });

    let app = create_router(state).layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_response(DefaultOnResponse::new().level(Level::INFO)),
    );

    let mut tasks = Vec::new();
    for addr in &config.server.listen {
        tracing::info!("Listening on {}", addr);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let app = app.clone();
        let shutdown_token = token.clone();
        tasks.push(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_token.cancelled_owned())
                .await
                .map_err(|e| anyhow::anyhow!("server error: {}", e))
        }));
    }

    futures::future::try_join_all(tasks.into_iter().map(|h| async move {
        h.await
            .map_err(|e| anyhow::anyhow!("task panicked: {}", e))?
    }))
    .await?;

    Ok(())
}
