//! ogygia-irisd: Peer-to-peer Nix binary cache with bloom filter peer lookup

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use ogygia_nixutils::Nix;
use tokio::signal;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

mod bloom;
mod config;
mod multifetch;
mod nix;
mod server;
mod store;

#[derive(Parser)]
#[command(
    name = "ogygia-irisd",
    version,
    about = "Peer-to-peer Nix binary cache"
)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Disable store watching for new paths
    #[arg(long)]
    no_watch: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let config = Arc::new(config::load_config(cli.config.as_deref())?);
    tracing::info!("Loaded configuration");

    tracing::info!("ogygia-irisd starting");
    tracing::info!("HTTP listen: {:?}", config.server.listen);
    tracing::info!("Peers: {:?}", config.peers.urls);
    // Initialize bloom filters
    let local_bloom = Arc::new(bloom::local::LocalBloom::new(
        config.bloom.false_positive_rate,
        config.bloom.rebuild_threshold,
    ));
    let peer_blooms = Arc::new(bloom::peers::PeerBlooms::new(
        Duration::from_secs(config.bloom.peer_bloom_ttl_secs),
        Duration::from_secs(config.bloom.max_age_secs()),
        local_bloom.num_bits(),
        local_bloom.num_hashes(),
    ));

    // HTTP client for peer requests
    let http_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client");

    // Initialize NAR disk cache
    let nar_cache: Arc<nix::cache::NarCache> = Arc::new(
        nix::cache::NarCache::new(&config.cache)
            .await
            .context("Failed to initialize NAR cache")?,
    );
    tracing::info!("NAR cache dir: {}", config.cache.dir.display());
    tracing::info!(
        "NAR cache limits: max_size={}B, tti={}s",
        config.cache.max_size_bytes,
        config.cache.time_to_idle_secs,
    );

    let recovered: usize = nar_cache.recover().await.unwrap_or_else(|e| {
        tracing::warn!("NAR cache recovery failed: {}", e);
        0
    });
    if recovered > 0 {
        tracing::info!("NAR cache: recovered {} entries from disk", recovered);
    }

    // Handle to the local Nix installation, cheaply cloned across the HTTP
    // server, scanner, and watcher. The read-only database is opened lazily on
    // first use and its connection pool shared across every clone.
    let nix = Nix::default();

    // Cancellation token for coordinated shutdown
    let token = CancellationToken::new();

    // Signal handler: cancel the token on SIGTERM/SIGINT
    {
        let token = token.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            token.cancel();
        });
    }

    // Rebuild channel: watcher sends rebuild requests, scanner receives
    let (rebuild_tx, rebuild_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Spawn background tasks
    let mut tasks: Vec<JoinHandle<Result<()>>> = Vec::new();

    // HTTP server: started before the store scan so we serve peer lookups
    // immediately. GET /bloom returns 503 until the initial scan completes.
    tasks.push(tokio::spawn(server::start(
        config,
        Arc::clone(&local_bloom),
        Arc::clone(&peer_blooms),
        http_client,
        nar_cache,
        nix.clone(),
        token.clone(),
    )));

    {
        let scanner =
            store::scanner::StoreScanner::new(Arc::clone(&local_bloom), nix.clone(), rebuild_rx);
        let local_bloom = Arc::clone(&local_bloom);

        tasks.push(tokio::spawn(async move {
            let stats = scanner.scan("Store scan").await?;
            tracing::info!("Store scan: {} indexed", stats.indexed);
            local_bloom.finish_rebuild();
            scanner.run_rebuild_loop().await
        }));
    }

    // Store watcher (optional, runs until cancelled)
    if !cli.no_watch {
        let bloom = Arc::clone(&local_bloom);
        let nix = nix.clone();
        let token = token.clone();
        tasks.push(tokio::spawn(async move {
            let watcher = Arc::new(store::watcher::StoreWatcher::new(bloom, nix, rebuild_tx));
            watcher.start(token).await
        }));
    }

    // Peer bloom eviction (runs until cancelled)
    {
        let peer_blooms = Arc::clone(&peer_blooms);
        let token = token.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let sleep_dur = peer_blooms
                    .next_eviction_time()
                    .await
                    .map(|t| t.saturating_duration_since(Instant::now()))
                    .unwrap_or(peer_blooms.max_age());
                tokio::select! {
                    _ = token.cancelled() => {
                        tracing::info!("Peer bloom eviction shutting down");
                        return Ok(());
                    }
                    _ = tokio::time::sleep(sleep_dur) => {
                        peer_blooms.evict_expired().await;
                    }
                }
            }
        }));
    }

    // Wait for all tasks — if any returns an error, propagate it.
    let futs = tasks.into_iter().map(|h| async move {
        h.await
            .map_err(|e| anyhow::anyhow!("task panicked: {}", e))?
    });
    futures::future::try_join_all(futs).await?;

    tracing::info!("ogygia-irisd stopped");
    Ok(())
}

async fn shutdown_signal() {
    let mut sigterm =
        signal::unix::signal(signal::unix::SignalKind::terminate()).expect("install SIGTERM");
    tokio::select! {
        _ = signal::ctrl_c() => tracing::info!("Received SIGINT, shutting down"),
        _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down"),
    }
}
