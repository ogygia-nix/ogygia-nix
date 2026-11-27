//! Shared application state and peer lookup logic.

use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use ogygia_nixutils::NarInfo;
use ogygia_nixutils::Nix;

use crate::bloom::local::LocalBloom;
use crate::bloom::peers::PeerBlooms;
use crate::config::Config;
use crate::nix::cache::NarCache;

/// Shared application state
pub struct AppState {
    pub config: Arc<Config>,
    pub local_bloom: Arc<LocalBloom>,
    pub peer_blooms: Arc<PeerBlooms>,
    pub http_client: reqwest::Client,
    pub nar_cache: Arc<NarCache>,
    pub nix: Nix,
}

impl AppState {
    /// Try to fetch narinfo from peer via bloom filter lookup.
    ///
    /// Streams bloom lookups concurrently with narinfo fetches: as each
    /// peer's bloom becomes available and matches the hash, a narinfo fetch
    /// is started immediately — without waiting for all blooms to arrive.
    pub(super) async fn try_peer_narinfo(&self, hash: &str) -> Option<(NarInfo, String)> {
        let peer_urls = &self.config.peers.urls;
        if peer_urls.is_empty() {
            return None;
        }

        let trusted_keys = &self.config.trust.trusted_keys;

        let mut candidate_rx = self
            .peer_blooms
            .lookup_stream(peer_urls, hash, &self.http_client)
            .await;

        let client = self.http_client.clone();
        let mut narinfo_futs = FuturesUnordered::new();

        loop {
            tokio::select! {
                biased;

                Some(result) = narinfo_futs.next() => {
                    let Some((peer_url, narinfo)): Option<(String, NarInfo)> = result else {
                        continue;
                    };

                    if !narinfo.is_trusted(trusted_keys) {
                        tracing::debug!(
                            "narinfo {} from {} is not trusted (no CA field or trusted signature), skipping",
                            hash,
                            peer_url
                        );
                        continue;
                    }

                    if narinfo.ca.is_some() {
                        tracing::info!("narinfo {} fetched from peer {} (content-addressed)", hash, peer_url);
                    } else {
                        tracing::info!("narinfo {} fetched from peer {}", hash, peer_url);
                    }
                    return Some((narinfo, peer_url));
                }

                Some(peer_url) = candidate_rx.recv() => {
                    narinfo_futs.push(fetch_narinfo_from_peer(
                        client.clone(),
                        peer_url,
                        hash.to_string(),
                    ));
                }

                else => break,
            }
        }

        None
    }

    /// Try to proxy a NAR from a peer found via bloom filter lookup.
    ///
    /// `nar_path` is the request path below `nar/`, whose NarHash came from a
    /// trusted narinfo; peers only serve a NAR whose NarHash matches it.
    /// Peers that already have the compressed NAR cached are preferred.
    pub(super) async fn try_peer_nar(&self, hash: &str, nar_path: &str) -> Response {
        let peer_urls = &self.config.peers.urls;
        if peer_urls.is_empty() {
            return (StatusCode::NOT_FOUND, "Not found").into_response();
        }

        let mut candidate_rx = self
            .peer_blooms
            .lookup_stream(peer_urls, hash, &self.http_client)
            .await;

        let nar_url =
            |peer_url: &str| format!("{}/local/nar/{}", peer_url.trim_end_matches('/'), nar_path);

        // Candidates not yet sent a plain request, in arrival order.
        let mut untried = Vec::new();
        let mut probes = FuturesUnordered::new();

        loop {
            tokio::select! {
                biased;

                Some(probe) = probes.next() => {
                    let (peer_url, cached): (String, bool) = probe;
                    if !cached {
                        continue;
                    }
                    untried.retain(|p| p != &peer_url);
                    if let Some(response) = stream_nar_from_url(&self.http_client, &nar_url(&peer_url)).await {
                        tracing::info!("Proxying cached NAR {} from peer {}", hash, peer_url);
                        return response;
                    }
                }

                Some(peer_url) = candidate_rx.recv() => {
                    untried.push(peer_url.clone());
                    let client = self.http_client.clone();
                    let url = nar_url(&peer_url);
                    probes.push(async move {
                        let cached = is_cached_on_peer(&client, &url).await;
                        (peer_url, cached)
                    });
                }

                else => break,
            }
        }

        for peer_url in untried {
            if let Some(response) =
                stream_nar_from_url(&self.http_client, &nar_url(&peer_url)).await
            {
                tracing::info!("Proxying NAR {} from peer {}", hash, peer_url);
                return response;
            }
        }

        (StatusCode::NOT_FOUND, "Not found").into_response()
    }
}

/// Whether a peer answers a cache-only `HEAD` for `url` with success.
async fn is_cached_on_peer(client: &reqwest::Client, url: &str) -> bool {
    match client
        .head(url)
        .header(reqwest::header::CACHE_CONTROL, "only-if-cached")
        .send()
        .await
    {
        Ok(r) => {
            tracing::debug!("Cache-only probe of {} returned {}", url, r.status());
            r.status().is_success()
        }
        Err(e) => {
            tracing::debug!("Cache-only probe of {} failed: {}", url, e);
            false
        }
    }
}

/// Fetch and parse a narinfo from a single peer.
async fn fetch_narinfo_from_peer(
    client: reqwest::Client,
    peer_url: String,
    hash: String,
) -> Option<(String, NarInfo)> {
    let url = format!("{}/local/{}.narinfo", peer_url.trim_end_matches('/'), hash);
    let response = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::debug!(
                "Peer {} returned {} for narinfo {}",
                peer_url,
                r.status(),
                hash
            );
            return None;
        }
        Err(e) => {
            tracing::warn!("Failed to fetch narinfo from {}: {}", peer_url, e);
            return None;
        }
    };
    let body = match response.text().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("Failed to read narinfo from {}: {}", peer_url, e);
            return None;
        }
    };
    match body.parse::<NarInfo>() {
        Ok(narinfo) => Some((peer_url, narinfo)),
        Err(e) => {
            tracing::warn!("Failed to parse narinfo from {}: {}", peer_url, e);
            None
        }
    }
}

/// Fetch a NAR from a URL and return it as a streaming response.
///
/// On stream failure, retries the same URL using `Range` requests as long
/// as each attempt makes progress (receives at least 1 byte). The
/// `x-ogygia-nar-file-hash` header is validated on retries to ensure the
/// peer is still serving the same file.
async fn stream_nar_from_url(client: &reqwest::Client, url: &str) -> Option<Response> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/x-nix-nar")
        .to_string();

    let expected_file_hash: String = response
        .headers()
        .get("x-ogygia-nar-file-hash")
        .and_then(|v| v.to_str().ok())
        .map(String::from)?;

    let expected_total_size: u64 = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())?;

    let body = Body::from_stream(stream_with_retry(
        client.clone(),
        url.to_string(),
        expected_file_hash,
        expected_total_size,
        response,
    ));

    let builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", &content_type)
        .header("content-length", expected_total_size);

    Some(builder.body(body).unwrap().into_response())
}

type ByteStream = Pin<Box<dyn futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// State for the retrying NAR stream.
struct RetryStreamState {
    client: reqwest::Client,
    url: String,
    expected_file_hash: String,
    expected_total_size: u64,
    bytes_received: u64,
    bytes_at_last_retry: u64,
    stream: ByteStream,
}

/// Create a stream that transparently retries a NAR download using Range
/// requests on failure.
///
/// Each call to the underlying stream either yields the next chunk or, on
/// error, attempts a Range request to resume. Gives up if no progress was
/// made since the last retry (zero bytes received).
fn stream_with_retry(
    client: reqwest::Client,
    url: String,
    expected_file_hash: String,
    expected_total_size: u64,
    initial_response: reqwest::Response,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    let state = RetryStreamState {
        client,
        url,
        expected_file_hash,
        expected_total_size,
        bytes_received: 0,
        bytes_at_last_retry: 0,
        stream: Box::pin(initial_response.bytes_stream()),
    };

    futures::stream::try_unfold(state, |mut state| async move {
        loop {
            match state.stream.next().await {
                Some(Ok(chunk)) => {
                    state.bytes_received += chunk.len() as u64;
                    return Ok(Some((chunk, state)));
                }
                Some(Err(e)) => {
                    tracing::warn!(
                        "NAR stream from {} failed at byte {}: {}",
                        state.url,
                        state.bytes_received,
                        e,
                    );
                }
                None => {
                    // Stream ended — check if this was a complete transfer
                    if state.bytes_received >= state.expected_total_size {
                        return Ok(None);
                    }
                    tracing::warn!(
                        "NAR stream from {} ended prematurely at byte {}/{}",
                        state.url,
                        state.bytes_received,
                        state.expected_total_size,
                    );
                }
            }

            // Check progress since last retry — give up if none
            if state.bytes_received == state.bytes_at_last_retry {
                return Err(std::io::Error::other(format!(
                    "NAR stream from {} stalled at byte {} with no progress",
                    state.url, state.bytes_received,
                )));
            }

            tracing::info!(
                "Retrying NAR stream from {} at byte {} (was {})",
                state.url,
                state.bytes_received,
                state.bytes_at_last_retry,
            );
            state.bytes_at_last_retry = state.bytes_received;

            // Brief backoff before retrying — the failure is external and
            // hammering immediately is unlikely to help.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            // Attempt a Range request to resume
            let retry_response = match state
                .client
                .get(&state.url)
                .header("range", format!("bytes={}-", state.bytes_received))
                .send()
                .await
            {
                Ok(r) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => r,
                Ok(r) => {
                    return Err(std::io::Error::other(format!(
                        "Range retry for {} returned {}",
                        state.url,
                        r.status(),
                    )));
                }
                Err(e) => {
                    return Err(std::io::Error::other(format!(
                        "Range retry for {} failed: {}",
                        state.url, e,
                    )));
                }
            };

            // Validate the peer is still serving the same file
            let retry_hash = retry_response
                .headers()
                .get("x-ogygia-nar-file-hash")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if retry_hash != state.expected_file_hash {
                return Err(std::io::Error::other(format!(
                    "file hash changed on retry for {}: expected {}, got {}",
                    state.url, state.expected_file_hash, retry_hash,
                )));
            }

            state.stream = Box::pin(retry_response.bytes_stream());
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    use super::*;

    const FILE_HASH: &str = "sha256-test-hash";
    const PAYLOAD_SIZE: usize = 1024;

    /// Build the test payload — a deterministic 1024-byte sequence.
    fn test_payload() -> Vec<u8> {
        (0..PAYLOAD_SIZE).map(|i| (i % 256) as u8).collect()
    }

    /// Read from a socket until we see `\r\n\r\n` (end of HTTP headers).
    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut buf = vec![0u8; 4096];
        let mut filled = 0;
        loop {
            let n = socket.read(&mut buf[filled..]).await.unwrap();
            assert!(n > 0, "client closed before sending full headers");
            filled += n;
            let s = std::str::from_utf8(&buf[..filled]).unwrap_or("");
            if s.contains("\r\n\r\n") {
                return s.to_string();
            }
        }
    }

    /// Raw TCP mock server that simulates a mid-transfer failure and
    /// successful Range retry.
    ///
    /// - Request 1: sends 200 OK with Content-Length: 1024, streams 512 bytes,
    ///   then closes the connection (simulating a network failure).
    /// - Request 2: sends 206 Partial Content with the remaining 512 bytes.
    async fn run_mock_server(listener: tokio::net::TcpListener, counter: Arc<AtomicU32>) {
        let payload = test_payload();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let payload = payload.clone();
            tokio::spawn(async move {
                read_http_request(&mut socket).await;

                match n {
                    0 => {
                        // Send valid headers claiming 1024 bytes, then only
                        // send 512 and close — exactly like a network failure.
                        let headers = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/zstd\r\n\
                             Content-Length: {PAYLOAD_SIZE}\r\n\
                             X-Ogygia-NAR-File-Hash: {FILE_HASH}\r\n\
                             \r\n"
                        );
                        let _ = socket.write_all(headers.as_bytes()).await;
                        let _ = socket.write_all(&payload[..512]).await;
                        let _ = socket.flush().await;
                        let _ = socket.shutdown().await;
                    }
                    _ => {
                        let remaining = &payload[512..];
                        let headers = format!(
                            "HTTP/1.1 206 Partial Content\r\n\
                             Content-Type: application/zstd\r\n\
                             Content-Length: {}\r\n\
                             Content-Range: bytes 512-{}/{PAYLOAD_SIZE}\r\n\
                             X-Ogygia-NAR-File-Hash: {FILE_HASH}\r\n\
                             \r\n",
                            remaining.len(),
                            PAYLOAD_SIZE - 1,
                        );
                        let _ = socket.write_all(headers.as_bytes()).await;
                        let _ = socket.write_all(remaining).await;
                    }
                }
            });
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stream_with_retry_resumes_after_failure() {
        let counter = Arc::new(AtomicU32::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_counter = counter.clone();
        tokio::spawn(async move {
            run_mock_server(listener, server_counter).await;
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{}/nar/test", addr);

        let response = stream_nar_from_url(&client, &url).await;
        assert!(response.is_some(), "stream_nar_from_url should succeed");

        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), PAYLOAD_SIZE * 2)
            .await
            .expect("should collect full body");

        assert_eq!(body.len(), PAYLOAD_SIZE);
        assert_eq!(body.as_ref(), test_payload().as_slice());
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    mod peer_nar {
        use std::time::Duration;

        use axum::http::HeaderMap;
        use axum::http::Method;
        use axum::routing::get;

        use super::*;
        use crate::config::CacheConfig;

        const HASH: &str = "abc123def456ghi789jkl012mno345pq";
        const NAR_PATH: &str = "0101/abc123def456ghi789jkl012mno345pq-test.nar.zst";

        fn local_bloom() -> LocalBloom {
            let bloom = LocalBloom::new(0.01, 0.1);
            bloom.insert(HASH);
            bloom.finish_rebuild();
            bloom
        }

        #[derive(Clone, Copy)]
        enum Peer {
            /// Has the compressed NAR cached.
            Cached,
            /// Serves the NAR only to a plain request.
            Uncached,
            /// Cannot serve the NAR at all.
            Broken,
        }

        /// A peer advertising `HASH` that answers `/local/nar` with its
        /// `name` as the body. Returns its URL and a count of the plain
        /// `GET`s it received.
        async fn mock_peer(kind: Peer, name: &'static str) -> (String, Arc<AtomicU32>) {
            let plain_gets = Arc::new(AtomicU32::new(0));
            let counter = Arc::clone(&plain_gets);
            let bloom = local_bloom().serialize().unwrap().unwrap();
            let router = axum::Router::new()
                .route("/bloom", get(move || async move { bloom }))
                .route(
                    "/local/nar/{*path}",
                    get(move |method: Method, headers: HeaderMap| async move {
                        let only_if_cached = headers
                            .get("cache-control")
                            .is_some_and(|v| v == "only-if-cached");
                        if !only_if_cached && method == Method::GET {
                            counter.fetch_add(1, Ordering::SeqCst);
                        }
                        match (kind, only_if_cached) {
                            (Peer::Cached, _) | (Peer::Uncached, false) => (
                                StatusCode::OK,
                                [("x-ogygia-nar-file-hash", FILE_HASH)],
                                name,
                            )
                                .into_response(),
                            (Peer::Uncached | Peer::Broken, true) => {
                                StatusCode::GATEWAY_TIMEOUT.into_response()
                            }
                            (Peer::Broken, false) => {
                                StatusCode::INTERNAL_SERVER_ERROR.into_response()
                            }
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, router).await });
            (url, plain_gets)
        }

        /// Fetch the NAR through `try_peer_nar` with `peers` configured,
        /// returning the status and body.
        async fn fetch(peers: Vec<String>) -> (StatusCode, Bytes) {
            let cache_dir = tempfile::tempdir().unwrap();
            let bloom = local_bloom();
            let config = Config {
                server: crate::config::ServerConfig {
                    listen: vec!["127.0.0.1:0".to_string()],
                    priority: 30,
                },
                bloom: Default::default(),
                peers: crate::config::PeersConfig { urls: peers },
                trust: Default::default(),
                cache: CacheConfig {
                    dir: cache_dir.path().to_path_buf(),
                    ..Default::default()
                },
            };
            let state = AppState {
                nar_cache: Arc::new(NarCache::new(&config.cache).await.unwrap()),
                config: Arc::new(config),
                local_bloom: Arc::new(LocalBloom::new(0.01, 0.1)),
                peer_blooms: Arc::new(PeerBlooms::new(
                    Duration::from_secs(300),
                    Duration::from_secs(600),
                    bloom.num_bits(),
                    bloom.num_hashes(),
                )),
                http_client: reqwest::Client::builder().no_proxy().build().unwrap(),
                nix: Nix::in_memory(&[]).await.unwrap(),
            };
            let response = state.try_peer_nar(HASH, NAR_PATH).await;
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            (status, body)
        }

        #[tokio::test]
        async fn test_prefers_cached_peer() {
            let (a, a_gets) = mock_peer(Peer::Uncached, "a").await;
            let (b, b_gets) = mock_peer(Peer::Cached, "b").await;
            let (c, c_gets) = mock_peer(Peer::Uncached, "c").await;

            let (status, body) = fetch(vec![a, b, c]).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body.as_ref(), b"b");
            assert_eq!(a_gets.load(Ordering::SeqCst), 0);
            assert_eq!(b_gets.load(Ordering::SeqCst), 1);
            assert_eq!(c_gets.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn test_falls_back_to_plain_request() {
            let (a, a_gets) = mock_peer(Peer::Uncached, "a").await;
            let (b, b_gets) = mock_peer(Peer::Uncached, "b").await;

            let (status, body) = fetch(vec![a, b]).await;
            assert_eq!(status, StatusCode::OK);
            let gets = [a_gets.load(Ordering::SeqCst), b_gets.load(Ordering::SeqCst)];
            let expected: &[u8] = if gets == [1, 0] { b"a" } else { b"b" };
            assert_eq!(gets.iter().sum::<u32>(), 1);
            assert_eq!(body.as_ref(), expected);
        }

        #[tokio::test]
        async fn test_fails_when_no_peer_can_serve() {
            let (a, a_gets) = mock_peer(Peer::Broken, "a").await;
            let (b, b_gets) = mock_peer(Peer::Broken, "b").await;

            let (status, _) = fetch(vec![a, b]).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(a_gets.load(Ordering::SeqCst), 1);
            assert_eq!(b_gets.load(Ordering::SeqCst), 1);
        }
    }
}
