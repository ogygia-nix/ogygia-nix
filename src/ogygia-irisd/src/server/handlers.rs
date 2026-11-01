//! HTTP request handlers for Nix binary cache protocol

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use ogygia_nixutils::NarInfo;
use ogygia_nixutils::StoreHash;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use super::AppState;
use crate::nix::narinfo::narinfo_from_path_info;

/// GET /nix-cache-info
pub async fn nix_cache_info(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let info = format!(
        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: {}\n",
        state.config.server.priority,
    );

    (
        StatusCode::OK,
        [("content-type", "text/x-nix-cache-info")],
        info,
    )
}

/// GET /bloom — return serialized local bloom filter
pub async fn get_bloom(State(state): State<Arc<AppState>>) -> Response {
    match state.local_bloom.serialize() {
        Ok(Some(data)) => (
            StatusCode::OK,
            [("content-type", "application/octet-stream")],
            data,
        )
            .into_response(),
        Ok(None) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "300")],
            "Store indexing in progress",
        )
            .into_response(),
        Err(e) => {
            tracing::error!("Failed to serialize bloom: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to serialize bloom",
            )
                .into_response()
        }
    }
}

/// GET /{hash}.narinfo
pub async fn get_narinfo(
    State(state): State<Arc<AppState>>,
    Path(narinfo_path): Path<String>,
) -> Response {
    let hash = match narinfo_path.strip_suffix(".narinfo") {
        Some(h) => h,
        None => {
            return (StatusCode::NOT_FOUND, "Not found").into_response();
        }
    };

    // 1. Try local store first
    if let Some(narinfo) = try_local_store_narinfo(&state, hash).await {
        return (
            StatusCode::OK,
            [("content-type", "text/x-nix-narinfo")],
            narinfo.to_string(),
        )
            .into_response();
    }

    // 2. Try peers via bloom lookup
    if let Some((mut narinfo, _)) = state.try_peer_narinfo(hash).await {
        // Rewrite the URL to use our format with the NarHash encoded as hex,
        // so the NAR request will be self-describing regardless of the peer's format.
        let filename = narinfo.url.rsplit('/').next().unwrap_or(&narinfo.url);
        narinfo.url = format!("nar/{}/{}", narinfo.nar_hash.to_hex(), filename);
        return (
            StatusCode::OK,
            [("content-type", "text/x-nix-narinfo")],
            narinfo.to_string(),
        )
            .into_response();
    }

    (StatusCode::NOT_FOUND, "Not found").into_response()
}

/// GET /local/{hash}.narinfo — local-only narinfo (no peer fan-out)
///
/// Used by peers to query this node's local store without triggering
/// cascading fan-out across the cluster.
pub async fn get_local_narinfo(
    State(state): State<Arc<AppState>>,
    Path(narinfo_path): Path<String>,
) -> Response {
    let hash = match narinfo_path.strip_suffix(".narinfo") {
        Some(h) => h,
        None => {
            return (StatusCode::NOT_FOUND, "Not found").into_response();
        }
    };

    match try_local_store_narinfo(&state, hash).await {
        Some(narinfo) => (
            StatusCode::OK,
            [("content-type", "text/x-nix-narinfo")],
            narinfo.to_string(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

/// Try to generate narinfo from local /nix/store path.
///
/// Ensures the NAR is cached so the response includes the real compressed
/// FileHash and FileSize (needed for integrity validation and range requests).
async fn try_local_store_narinfo(state: &AppState, hash: &str) -> Option<NarInfo> {
    let store_hash = hash.parse::<StoreHash>().ok()?;
    let path_info = match state.nix.find_path_info(&store_hash).await {
        Ok(Some(info)) => info,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("Failed to query Nix database for {}: {}", hash, e);
            return None;
        }
    };

    let store_path = PathBuf::from(&path_info.path);
    tracing::debug!("Found store path for {}: {}", hash, store_path.display());

    let mut narinfo = narinfo_from_path_info(&path_info);

    // Generate/retrieve the cached NAR to get real FileHash and FileSize
    match state.nar_cache.ensure(hash, &store_path).await {
        Ok(cached) => {
            narinfo.file_hash = cached.file_hash;
            narinfo.file_size = cached.file_size;
        }
        Err(e) => {
            tracing::error!("Failed to generate NAR for {}: {}", store_path.display(), e);
            return None;
        }
    }

    tracing::info!("Generated narinfo for {} from local store", hash);
    Some(narinfo)
}

/// GET /nar/{path}
///
/// Path format: `nar/{nar_hash}/{store_hash}-{name}.nar.zst`
/// The NarHash in the URL makes requests self-describing so we don't need
/// server-side state to match narinfo responses to NAR content. Supports
/// `Range` requests when the NAR is served from the local store.
pub async fn get_nar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response {
    let Some((nar_hash, hash)) = parse_nar_path(&path) else {
        tracing::warn!("Invalid NAR path format: {}", path);
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };

    let range = parse_byte_range(&headers);
    let local = try_local_store_nar(&state, hash, nar_hash, range).await;
    if local.status() != StatusCode::NOT_FOUND {
        return local;
    }

    // Try peers via bloom lookup
    state.try_peer_nar(hash, nar_hash).await
}

/// GET /local/nar/{path} — local-only NAR download (no peer fan-out)
///
/// Used by peers to fetch NARs from this node's local store without
/// triggering cascading fan-out across the cluster. Supports `Range`
/// requests for resumable downloads.
pub async fn get_local_nar(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response {
    let Some((nar_hash, hash)) = parse_nar_path(&path) else {
        tracing::warn!("Invalid NAR path format: {}", path);
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };

    let range = parse_byte_range(&headers);
    try_local_store_nar(&state, hash, nar_hash, range).await
}

/// Parse a NAR path into `(nar_hash, store_hash)`.
///
/// Expected format: `{nar_hash}/{store_hash}-{name}.nar.{compression}`
fn parse_nar_path(path: &str) -> Option<(&str, &str)> {
    let (nar_hash, filename) = path.split_once('/')?;
    let hash = filename.split('-').next()?;
    (hash.len() == 32).then_some((nar_hash, hash))
}

/// Parse a `Range` header into `(start, Option<end>)`.
///
/// Supports `bytes=start-` (open-ended) and `bytes=start-end` (closed).
/// Multi-range (comma-separated) and suffix ranges (`bytes=-N`) return `None`.
fn parse_byte_range(headers: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let value = headers.get("range")?.to_str().ok()?;
    let suffix = value.strip_prefix("bytes=")?;
    // Reject multi-range
    if suffix.contains(',') {
        return None;
    }
    let (start_str, end_str) = suffix.split_once('-')?;
    // Reject suffix ranges (empty start)
    if start_str.is_empty() {
        return None;
    }
    let start = start_str.parse::<u64>().ok()?;
    let end = if end_str.is_empty() {
        None
    } else {
        Some(end_str.parse::<u64>().ok()?)
    };
    Some((start, end))
}

/// Try to serve a NAR from the local store via the disk cache.
///
/// When `range` is `Some`, returns a `206 Partial Content` response for
/// the requested byte range. Supports open-ended `(start, None)` and
/// closed `(start, Some(end))` ranges.
async fn try_local_store_nar(
    state: &AppState,
    hash: &str,
    expected_nar_hash: &str,
    range: Option<(u64, Option<u64>)>,
) -> Response {
    let store_hash = match hash.parse::<StoreHash>() {
        Ok(h) => h,
        Err(_) => return (StatusCode::NOT_FOUND, "Not found").into_response(),
    };
    let path_info = match state.nix.find_path_info(&store_hash).await {
        Ok(Some(info)) => info,
        Ok(None) => return (StatusCode::NOT_FOUND, "Not found").into_response(),
        Err(e) => {
            tracing::warn!("Failed to query Nix database for {}: {}", hash, e);
            return (StatusCode::NOT_FOUND, "Not found").into_response();
        }
    };

    let store_path = PathBuf::from(&path_info.path);

    // Verify the local NarHash matches what the URL claims
    if path_info.nar_hash.to_hex() != expected_nar_hash {
        tracing::debug!(
            "Local NarHash mismatch for {}: expected {}, got {}",
            hash,
            expected_nar_hash,
            path_info.nar_hash.to_hex()
        );
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }

    // Get from cache or generate, opening the file under the RwLock
    let (cached, mut file) = match state.nar_cache.ensure_and_open(hash, &store_path).await {
        Ok((cached, Some(file))) => (cached, file),
        Ok((_, None)) => {
            tracing::debug!("Cached NAR for {} evicted during open", hash);
            return (StatusCode::NOT_FOUND, "Not found").into_response();
        }
        Err(e) => {
            tracing::error!("Failed to generate NAR for {}: {}", store_path.display(), e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to generate NAR").into_response();
        }
    };

    let file_size = cached.file_size;
    let file_hash = cached.file_hash.to_sri();

    match range {
        Some((start, end)) => {
            // Clamp end to file boundary
            let end = end.map(|e| e.min(file_size - 1)).unwrap_or(file_size - 1);

            if start >= file_size || (end < start) {
                let content_range = format!("bytes */{}", file_size);
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    [("content-range", content_range.as_str())],
                )
                    .into_response();
            }

            if start > 0 {
                use tokio::io::AsyncSeekExt;
                if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                    tracing::error!("Failed to seek to {} for {}: {}", start, hash, e);
                    return (StatusCode::INTERNAL_SERVER_ERROR, "Seek failed").into_response();
                }
            }

            let length = end - start + 1;
            let body = Body::from_stream(ReaderStream::new(file.take(length)));
            let content_length = length.to_string();
            let content_range = format!("bytes {}-{}/{}", start, end, file_size);
            tracing::info!(
                "Serving cached NAR for {} (range {}-{})",
                store_path.display(),
                start,
                end,
            );
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    ("content-type", "application/zstd"),
                    ("content-length", content_length.as_str()),
                    ("content-range", content_range.as_str()),
                    ("accept-ranges", "bytes"),
                    ("x-ogygia-nar-file-hash", file_hash.as_str()),
                ],
                body,
            )
                .into_response()
        }
        None => {
            tracing::info!("Serving cached NAR for {}", store_path.display());
            let body = Body::from_stream(ReaderStream::new(file));
            let content_length = file_size.to_string();
            (
                StatusCode::OK,
                [
                    ("content-type", "application/zstd"),
                    ("content-length", content_length.as_str()),
                    ("accept-ranges", "bytes"),
                    ("x-ogygia-nar-file-hash", file_hash.as_str()),
                ],
                body,
            )
                .into_response()
        }
    }
}

/// Query parameters for GET /providers/{hash}
#[derive(Debug, Deserialize)]
pub struct ProvidersQuery {
    #[serde(default)]
    pub verbose: bool,
}

/// Response body for GET /providers/{hash}
#[derive(Debug, Serialize)]
pub struct ProvidersResponse {
    pub hash: String,
    pub providers: Vec<String>,
    pub local: bool,
    /// Bloom filter candidates (only present when verbose=true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bloom_candidates: Option<Vec<String>>,
}

/// GET /providers/{hash} — find which peers have a store path
pub async fn get_providers(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ProvidersQuery>,
) -> Response {
    // Check local store
    let local = match hash.parse::<StoreHash>() {
        Ok(store_hash) => state
            .nix
            .find_store_path(&store_hash)
            .await
            .ok()
            .flatten()
            .is_some(),
        Err(_) => false,
    };

    // Check peers via bloom lookup
    let peer_urls = &state.config.peers.urls;
    let mut providers = Vec::new();
    let mut bloom_candidates = Vec::new();

    if !peer_urls.is_empty() {
        state
            .peer_blooms
            .ensure_fresh(peer_urls, &state.http_client)
            .await;

        let candidates = state.peer_blooms.lookup(&hash).await;
        bloom_candidates = candidates.clone();

        for peer_url in &candidates {
            let url = format!("{}/local/{}.narinfo", peer_url.trim_end_matches('/'), hash);
            match state.http_client.get(&url).send().await {
                Ok(response) if response.status().is_success() => {
                    providers.push(peer_url.clone());
                }
                _ => {}
            }
        }
    }

    (
        StatusCode::OK,
        Json(ProvidersResponse {
            hash,
            providers,
            local,
            bloom_candidates: if query.verbose {
                Some(bloom_candidates)
            } else {
                None
            },
        }),
    )
        .into_response()
}

/// Request body for POST /rescan
#[derive(Debug, Deserialize)]
pub struct RescanRequest {
    pub paths: Vec<PathBuf>,
}

/// Response body for POST /rescan
#[derive(Debug, Serialize)]
pub struct RescanResponse {
    pub rescanned: usize,
    pub errors: usize,
}

/// POST /rescan - rescan store paths and insert hashes into bloom
///
/// This endpoint is called by `ogygia iris push` after signing paths.
/// Only indexes paths that are serveable (signed or content-addressed).
pub async fn rescan(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RescanRequest>,
) -> Response {
    let mut rescanned = 0;
    let mut errors = 0;

    for path in &request.paths {
        let path_str = path.to_string_lossy();

        // Extract and validate the store-path hash (also validates the format).
        let hash = match StoreHash::from_store_path(&path_str) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("rescan: invalid path {}: {}", path_str, e);
                errors += 1;
                continue;
            }
        };

        // Query the Nix database and verify serveability
        let serveable = match state.nix.is_path_serveable(&path_str).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "rescan: failed to query Nix database for {}: {}",
                    path_str,
                    e
                );
                errors += 1;
                continue;
            }
        };

        if !serveable {
            tracing::warn!(
                "rescan: path is not serveable (neither signed nor content-addressed): {}",
                path_str
            );
            errors += 1;
            continue;
        }

        state.local_bloom.insert(hash.as_str());
        rescanned += 1;

        tracing::info!("rescan: indexed {}", path_str);
    }

    tracing::info!(
        "rescan complete: {} rescanned, {} errors",
        rescanned,
        errors
    );

    (StatusCode::OK, Json(RescanResponse { rescanned, errors })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_range(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("range", value.parse().unwrap());
        headers
    }

    #[test]
    fn test_parse_byte_range_open_ended() {
        assert_eq!(
            parse_byte_range(&headers_with_range("bytes=0-")),
            Some((0, None))
        );
        assert_eq!(
            parse_byte_range(&headers_with_range("bytes=100-")),
            Some((100, None))
        );
        assert_eq!(
            parse_byte_range(&headers_with_range("bytes=999999-")),
            Some((999999, None)),
        );
    }

    #[test]
    fn test_parse_byte_range_closed() {
        assert_eq!(
            parse_byte_range(&headers_with_range("bytes=0-499")),
            Some((0, Some(499))),
        );
        assert_eq!(
            parse_byte_range(&headers_with_range("bytes=100-200")),
            Some((100, Some(200))),
        );
    }

    #[test]
    fn test_parse_byte_range_missing() {
        assert_eq!(parse_byte_range(&HeaderMap::new()), None);
    }

    #[test]
    fn test_parse_byte_range_suffix_range() {
        // bytes=-500 (suffix range) is not supported
        assert_eq!(parse_byte_range(&headers_with_range("bytes=-500")), None);
    }

    #[test]
    fn test_parse_byte_range_multi_range() {
        assert_eq!(
            parse_byte_range(&headers_with_range("bytes=0-100,200-300")),
            None,
        );
    }

    #[test]
    fn test_parse_byte_range_invalid() {
        assert_eq!(parse_byte_range(&headers_with_range("invalid")), None);
        assert_eq!(parse_byte_range(&headers_with_range("bytes=abc-")), None);
        assert_eq!(parse_byte_range(&headers_with_range("bytes=0-abc")), None);
    }

    mod bloom_readiness {
        use std::time::Duration;

        use super::*;
        use crate::bloom::local::LocalBloom;
        use crate::bloom::peers::PeerBlooms;
        use crate::config::CacheConfig;
        use crate::nix::cache::NarCache;

        async fn make_state(bloom_ready: bool) -> Arc<AppState> {
            let config = crate::config::Config {
                server: crate::config::ServerConfig {
                    listen: vec!["127.0.0.1:0".to_string()],
                    priority: 30,
                },
                bloom: Default::default(),
                peers: Default::default(),
                trust: Default::default(),
                cache: CacheConfig {
                    dir: tempfile::tempdir().unwrap().path().to_path_buf(),
                    time_to_idle_secs: 0,
                    max_size_bytes: 0,
                },
            };
            let nar_cache = Arc::new(NarCache::new(&config.cache).await.unwrap());
            let local_bloom = Arc::new(LocalBloom::new(0.01, 0.1));
            if bloom_ready {
                local_bloom.finish_rebuild();
            }
            Arc::new(AppState {
                config: Arc::new(config),
                local_bloom,
                peer_blooms: Arc::new(PeerBlooms::new(
                    Duration::from_secs(300),
                    Duration::from_secs(600),
                    0,
                    0,
                )),
                http_client: reqwest::Client::new(),
                nar_cache,
                nix: ogygia_nixutils::Nix::in_memory().await.unwrap(),
            })
        }

        #[tokio::test]
        async fn test_get_bloom_503_before_ready() {
            let state = make_state(false).await;
            let response = get_bloom(State(state)).await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let retry_after = response
                .headers()
                .get("retry-after")
                .expect("retry-after header");
            assert_eq!(retry_after, "300");
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let body = String::from_utf8_lossy(&body);
            assert!(body.contains("indexing in progress"));
        }

        #[tokio::test]
        async fn test_get_bloom_200_after_ready() {
            let state = make_state(true).await;
            let response = get_bloom(State(state)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let content_type = response
                .headers()
                .get("content-type")
                .expect("content-type");
            assert_eq!(content_type, "application/octet-stream");
        }
    }
}
