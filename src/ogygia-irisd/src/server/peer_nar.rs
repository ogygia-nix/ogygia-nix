//! The NAR stage for paths only peers hold.
//!
//! Peers are probed for the NAR in two tiers: a cache-only `HEAD` to every
//! bloom candidate, then plain `HEAD`s, which may make a peer compress the
//! NAR, one at a time as suppliers are needed. Peers serving the same
//! compressed file form a group that [`multifetch`] downloads from.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::io::SeekFrom;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

use anyhow::Context as _;
use async_compression::tokio::write::ZstdDecoder;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use ogygia_nixutils::NarHash;
use reqwest::header::CACHE_CONTROL;
use reqwest::header::CONTENT_LENGTH;
use reqwest::header::CONTENT_RANGE;
use reqwest::header::RANGE;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::AppState;
use crate::multifetch;
use crate::multifetch::ByteStream;
use crate::multifetch::ChunkStore;
use crate::multifetch::Download;
use crate::multifetch::PrefixVerifier;
use crate::multifetch::RangeSource;

/// Plain-tier suppliers kept probed ahead of the downloader.
const LOOKAHEAD: usize = 1;

const FILE_HASH_HEADER: &str = "x-ogygia-nar-file-hash";

/// Serve the NAR at `nar_path` (`{nar_hash}/{file name}`) from peers.
///
/// Responds once the first chunk is in hand, so a group that fails before
/// then is replaced by the next; `404` once no group is left.
pub(super) async fn serve(
    state: &AppState,
    hash: &str,
    nar_hash: &str,
    nar_path: &str,
) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "Not found").into_response();
    let peer_urls = &state.config.peers.urls;
    if peer_urls.is_empty() {
        return not_found();
    }
    let Ok(nar_hash) = NarHash::from_hex(nar_hash) else {
        return not_found();
    };
    let file_name = nar_path.rsplit('/').next().unwrap_or(nar_path);

    let candidates = state
        .peer_blooms
        .lookup_stream(peer_urls, hash, &state.http_client)
        .await;
    let suppliers = Suppliers::new(state.http_client.clone(), nar_path, candidates);
    let mut tried = HashSet::new();

    while let Some(first) = suppliers.next_group(&tried).await {
        let file_hash = first.file_hash;
        let file_size = first.file_size;
        tried.insert(file_hash);

        let store = match TempNar::create(&state.config.cache.dir, file_name, file_size).await {
            Ok(store) => Arc::new(store),
            Err(e) => {
                tracing::error!("Failed to create a temporary file for NAR {}: {}", hash, e);
                return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to download NAR")
                    .into_response();
            }
        };
        let config = multifetch::Config::default();
        let read_size = config.chunk_size;
        let download = tracing::info_span!("nar", %hash, %file_hash).in_scope(|| {
            multifetch::start(
                config,
                file_size,
                suppliers.group(first),
                Arc::clone(&store),
                NarVerifier::new(file_hash, nar_hash),
            )
        });

        let mut watermark = download.watermark();
        if watermark.wait_for(|&w| w > 0).await.is_ok() {
            tracing::info!("Proxying NAR {} from peers serving {}", hash, file_hash);
            let body = BodyState {
                download,
                store,
                watermark,
                pos: 0,
                file_size,
                read_size,
            };
            return axum::http::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/zstd")
                .header("content-length", file_size)
                .body(Body::from_stream(body.into_stream()))
                .unwrap()
                .into_response();
        }

        let completion = download.finish().await;
        if let Err(e) = completion.result {
            tracing::warn!(
                "NAR {} from the {} group failed before its first byte after {} sources: {}",
                hash,
                file_hash,
                completion.sources.len(),
                e
            );
        }
    }

    not_found()
}

/// What streaming a download to Nix needs; the download is cancelled when
/// the response is dropped.
struct BodyState {
    download: Download,
    store: Arc<TempNar>,
    watermark: watch::Receiver<u64>,
    pos: u64,
    file_size: u64,
    read_size: u64,
}

impl BodyState {
    fn into_stream(self) -> impl Stream<Item = io::Result<Bytes>> + Send {
        futures::stream::try_unfold(self, |mut s| async move {
            loop {
                let available = *s.watermark.borrow_and_update();
                if available > s.pos {
                    let len = (available - s.pos).min(s.read_size);
                    let data = s.store.read_at(s.pos, len as usize).await?;
                    s.pos += len;
                    return Ok(Some((data, s)));
                }
                if s.pos < s.file_size && s.watermark.changed().await.is_ok() {
                    continue;
                }
                s.download.finish().await.result.map_err(io::Error::other)?;
                return Ok(None);
            }
        })
    }
}

/// A peer known to serve the NAR as a particular compressed file.
struct Supplier {
    url: String,
    file_hash: NarHash,
    file_size: u64,
}

enum Probe {
    Cached {
        url: String,
        supplier: Option<Supplier>,
    },
    Plain(Option<Supplier>),
    CachedTierDone,
}

/// The suppliers found so far, of every group, and the probes still running.
struct Pool {
    client: reqwest::Client,
    events: mpsc::UnboundedReceiver<Probe>,
    events_tx: mpsc::UnboundedSender<Probe>,
    tasks: JoinSet<()>,
    cached_tier_done: bool,
    cached: VecDeque<Supplier>,
    plain: VecDeque<Supplier>,
    unprobed: VecDeque<String>,
    plain_probes: usize,
}

impl Pool {
    /// Take the earliest supplier `wanted` accepts, cached tier first,
    /// keeping `LOOKAHEAD` plain probes ahead of demand. `None` once no
    /// probe that could yield one is left.
    fn poll_take(
        &mut self,
        cx: &mut Context<'_>,
        wanted: impl Fn(&NarHash) -> bool,
    ) -> Poll<Option<Supplier>> {
        while let Poll::Ready(Some(probe)) = self.events.poll_recv(cx) {
            match probe {
                Probe::Cached {
                    supplier: Some(supplier),
                    ..
                } => self.cached.push_back(supplier),
                Probe::Cached {
                    url,
                    supplier: None,
                } => self.unprobed.push_back(url),
                Probe::Plain(supplier) => {
                    self.plain_probes -= 1;
                    self.plain.extend(supplier);
                }
                Probe::CachedTierDone => self.cached_tier_done = true,
            }
        }

        let take = |queue: &mut VecDeque<Supplier>| {
            let i = queue.iter().position(|s| wanted(&s.file_hash))?;
            queue.remove(i)
        };
        let taken = take(&mut self.cached).or_else(|| take(&mut self.plain));

        let mut ahead = self.plain_probes
            + self
                .cached
                .iter()
                .chain(&self.plain)
                .filter(|s| wanted(&s.file_hash))
                .count();
        while ahead < LOOKAHEAD
            && let Some(url) = self.unprobed.pop_front()
        {
            let client = self.client.clone();
            let tx = self.events_tx.clone();
            self.tasks.spawn(async move {
                drop(tx.send(Probe::Plain(probe(&client, &url, false).await)));
            });
            self.plain_probes += 1;
            ahead += 1;
        }

        match taken {
            Some(supplier) => Poll::Ready(Some(supplier)),
            None if self.cached_tier_done && self.unprobed.is_empty() && self.plain_probes == 0 => {
                Poll::Ready(None)
            }
            None => Poll::Pending,
        }
    }
}

/// Suppliers for one NAR, grouped by the compressed file they serve.
struct Suppliers {
    pool: Arc<Mutex<Pool>>,
    client: reqwest::Client,
}

impl Suppliers {
    fn new(
        client: reqwest::Client,
        nar_path: &str,
        candidates: mpsc::UnboundedReceiver<String>,
    ) -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        let mut tasks = JoinSet::new();
        tasks.spawn(probe_cached_tier(
            client.clone(),
            nar_path.to_string(),
            candidates,
            events_tx.clone(),
        ));
        let pool = Pool {
            client: client.clone(),
            events,
            events_tx,
            tasks,
            cached_tier_done: false,
            cached: VecDeque::new(),
            plain: VecDeque::new(),
            unprobed: VecDeque::new(),
            plain_probes: 0,
        };
        Self {
            pool: Arc::new(Mutex::new(pool)),
            client,
        }
    }

    /// The first supplier of a group not in `tried`.
    async fn next_group(&self, tried: &HashSet<NarHash>) -> Option<Supplier> {
        poll_fn(|cx| {
            self.pool
                .lock()
                .unwrap()
                .poll_take(cx, |h| !tried.contains(h))
        })
        .await
    }

    /// The suppliers of `first`'s group, starting with `first`.
    fn group(&self, first: Supplier) -> GroupStream {
        GroupStream {
            pool: Arc::clone(&self.pool),
            client: self.client.clone(),
            file_hash: first.file_hash,
            file_size: first.file_size,
            first: Some(first),
        }
    }
}

struct GroupStream {
    pool: Arc<Mutex<Pool>>,
    client: reqwest::Client,
    file_hash: NarHash,
    file_size: u64,
    first: Option<Supplier>,
}

impl Stream for GroupStream {
    type Item = PeerSource;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<PeerSource>> {
        let this = self.get_mut();
        let supplier = match this.first.take() {
            Some(supplier) => supplier,
            None => {
                let file_hash = this.file_hash;
                let taken = this.pool.lock().unwrap().poll_take(cx, |h| *h == file_hash);
                match ready!(taken) {
                    Some(supplier) => supplier,
                    None => return Poll::Ready(None),
                }
            }
        };
        Poll::Ready(Some(PeerSource {
            client: this.client.clone(),
            url: supplier.url,
            file_hash: this.file_hash,
            file_size: this.file_size,
        }))
    }
}

fn nar_url(peer_url: &str, nar_path: &str) -> String {
    format!("{}/local/nar/{}", peer_url.trim_end_matches('/'), nar_path)
}

/// Send the cache-only probe to every candidate as it arrives.
async fn probe_cached_tier(
    client: reqwest::Client,
    nar_path: String,
    mut candidates: mpsc::UnboundedReceiver<String>,
    tx: mpsc::UnboundedSender<Probe>,
) {
    let mut probes = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;

            Some(probe) = probes.next() => drop(tx.send(probe)),

            Some(peer_url) = candidates.recv() => {
                let url = nar_url(&peer_url, &nar_path);
                let client = client.clone();
                probes.push(async move {
                    let supplier = probe(&client, &url, true).await;
                    Probe::Cached { url, supplier }
                });
            }

            else => break,
        }
    }
    drop(tx.send(Probe::CachedTierDone));
}

/// `HEAD` a peer's NAR URL, returning the compressed file it serves.
async fn probe(client: &reqwest::Client, url: &str, only_if_cached: bool) -> Option<Supplier> {
    let mut request = client.head(url);
    if only_if_cached {
        request = request.header(CACHE_CONTROL, "only-if-cached");
    }
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("Probe of {} failed: {}", url, e);
            return None;
        }
    };
    tracing::debug!(
        "{} probe of {} returned {}",
        if only_if_cached {
            "Cache-only"
        } else {
            "Plain"
        },
        url,
        response.status()
    );
    if !response.status().is_success() {
        return None;
    }
    let header = |name| response.headers().get(name)?.to_str().ok();
    Some(Supplier {
        url: url.to_string(),
        file_hash: NarHash::from_sri(header(FILE_HASH_HEADER)?).ok()?,
        file_size: header(CONTENT_LENGTH.as_str())?.parse().ok()?,
    })
}

/// A peer's `/local/nar` URL for a file of a known hash and size.
struct PeerSource {
    client: reqwest::Client,
    url: String,
    file_hash: NarHash,
    file_size: u64,
}

impl std::fmt::Display for PeerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.url)
    }
}

impl RangeSource for PeerSource {
    async fn fetch(&self, range: Range<u64>) -> anyhow::Result<ByteStream> {
        let last = range.end - 1;
        let response = self
            .client
            .get(&self.url)
            .header(RANGE, format!("bytes={}-{}", range.start, last))
            .send()
            .await?;
        anyhow::ensure!(
            response.status() == reqwest::StatusCode::PARTIAL_CONTENT,
            "returned {}",
            response.status()
        );
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
        };
        let content_range = header(CONTENT_RANGE.as_str());
        anyhow::ensure!(
            content_range == format!("bytes {}-{}/{}", range.start, last, self.file_size),
            "returned Content-Range {content_range:?} for bytes {}-{last}",
            range.start
        );
        let file_hash = header(FILE_HASH_HEADER);
        anyhow::ensure!(
            NarHash::from_sri(file_hash).is_ok_and(|h| h == self.file_hash),
            "serves file hash {file_hash:?}, not {}",
            self.file_hash
        );
        Ok(response
            .bytes_stream()
            .map(|piece| piece.map_err(anyhow::Error::from))
            .boxed())
    }
}

/// A sparse `.nar.zst.tmp` file in the NAR cache directory, removed on drop.
struct TempNar {
    path: PathBuf,
    file: tokio::sync::Mutex<tokio::fs::File>,
}

impl TempNar {
    async fn create(dir: &Path, file_name: &str, len: u64) -> io::Result<Self> {
        let store_name = file_name.strip_suffix(".nar.zst").unwrap_or(file_name);
        // Unique per request, as concurrent requests for one NAR each download it.
        let path = dir.join(format!(
            "{store_name}.{:016x}.nar.zst.tmp",
            rand::random::<u64>()
        ));
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        let mut nar = Self {
            path,
            file: tokio::sync::Mutex::new(file),
        };
        nar.file.get_mut().set_len(len).await?;
        Ok(nar)
    }
}

impl Drop for TempNar {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::warn!("Failed to delete {}: {}", self.path.display(), e);
        }
    }
}

impl ChunkStore for TempNar {
    async fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(offset)).await?;
        file.write_all(data).await?;
        file.flush().await
    }

    async fn read_at(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let mut file = self.file.lock().await;
        file.seek(SeekFrom::Start(offset)).await?;
        let mut data = vec![0; len];
        file.read_exact(&mut data).await?;
        Ok(data.into())
    }
}

/// Checks the compressed file against its FileHash and the decompressed
/// NAR against the NarHash.
struct NarVerifier {
    file: Sha256,
    file_hash: NarHash,
    nar: ZstdDecoder<HashWriter>,
    nar_hash: NarHash,
}

impl NarVerifier {
    fn new(file_hash: NarHash, nar_hash: NarHash) -> Self {
        Self {
            file: Sha256::new(),
            file_hash,
            nar: ZstdDecoder::new(HashWriter(Sha256::new())),
            nar_hash,
        }
    }
}

impl PrefixVerifier for NarVerifier {
    async fn update(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.file.update(data);
        self.nar
            .write_all(data)
            .await
            .context("corrupt zstd stream")
    }

    async fn finish(self) -> anyhow::Result<()> {
        let Self {
            file,
            file_hash,
            mut nar,
            nar_hash,
        } = self;
        let got = NarHash::from_bytes(file.finalize().into());
        anyhow::ensure!(got == file_hash, "file hash is {got}, expected {file_hash}");
        nar.shutdown().await.context("corrupt zstd stream")?;
        let got = NarHash::from_bytes(nar.into_inner().0.finalize().into());
        anyhow::ensure!(got == nar_hash, "NAR hash is {got}, expected {nar_hash}");
        Ok(())
    }
}

struct HashWriter(Sha256);

impl AsyncWrite for HashWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.update(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use async_compression::tokio::bufread::ZstdEncoder;
    use axum::http::HeaderMap;
    use axum::http::Method;
    use axum::routing::get;
    use ogygia_nixutils::Nix;

    use super::*;
    use crate::bloom::local::LocalBloom;
    use crate::bloom::peers::PeerBlooms;
    use crate::config::CacheConfig;
    use crate::config::Config;
    use crate::nix::cache::NarCache;

    const HASH: &str = "abc123def456ghi789jkl012mno345pq";

    fn local_bloom() -> LocalBloom {
        let bloom = LocalBloom::new(0.01, 0.1);
        bloom.insert(HASH);
        bloom.finish_rebuild();
        bloom
    }

    fn sha256(data: &[u8]) -> NarHash {
        NarHash::from_bytes(Sha256::digest(data).into())
    }

    /// A peer advertising `HASH` that serves `file` from `/local/nar`.
    /// Its cache-only probe answers after `cache_probe_delay`, with `504`
    /// unless `cached`, and its range requests fail unless `serves_ranges`.
    /// Returns its URL and a count of its range requests.
    async fn mock_peer(
        file: Bytes,
        cached: bool,
        cache_probe_delay: Duration,
        serves_ranges: bool,
    ) -> (String, Arc<AtomicU32>) {
        let range_gets = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&range_gets);
        let bloom = local_bloom().serialize().unwrap().unwrap();
        let file_hash = sha256(&file).to_sri();
        let router = axum::Router::new()
            .route("/bloom", get(move || async move { bloom }))
            .route(
                "/local/nar/{*path}",
                get(move |method: Method, headers: HeaderMap| async move {
                    if headers.contains_key("cache-control") {
                        tokio::time::sleep(cache_probe_delay).await;
                        if !cached {
                            return StatusCode::GATEWAY_TIMEOUT.into_response();
                        }
                    }
                    let range = headers.get("range").and_then(|v| v.to_str().ok());
                    match (method, range) {
                        (Method::GET, Some(range)) => {
                            counter.fetch_add(1, Ordering::SeqCst);
                            if !serves_ranges {
                                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                            }
                            let (start, end) = range
                                .strip_prefix("bytes=")
                                .and_then(|r| r.split_once('-'))
                                .unwrap();
                            let (start, end): (usize, usize) =
                                (start.parse().unwrap(), end.parse().unwrap());
                            (
                                StatusCode::PARTIAL_CONTENT,
                                [
                                    (
                                        "content-range",
                                        format!("bytes {start}-{end}/{}", file.len()),
                                    ),
                                    ("x-ogygia-nar-file-hash", file_hash),
                                ],
                                file.slice(start..=end),
                            )
                                .into_response()
                        }
                        _ => (
                            StatusCode::OK,
                            [
                                ("content-length", file.len().to_string()),
                                ("x-ogygia-nar-file-hash", file_hash),
                            ],
                            file,
                        )
                            .into_response(),
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        (url, range_gets)
    }

    /// The first group to answer fails before its first byte, so the NAR
    /// comes from the group found by the lazy plain probe instead.
    #[tokio::test]
    async fn test_switches_to_a_reserve_group() {
        let nar: Vec<u8> = (0..20_000u32).map(|i| (i % 97) as u8).collect();
        let mut compressed = Vec::new();
        ZstdEncoder::new(nar.as_slice())
            .read_to_end(&mut compressed)
            .await
            .unwrap();
        let compressed = Bytes::from(compressed);
        let nar_hash = sha256(&nar).to_hex();

        let (broken, broken_gets) = mock_peer(
            Bytes::from_static(b"another compression"),
            true,
            Duration::ZERO,
            false,
        )
        .await;
        let (good, good_gets) =
            mock_peer(compressed.clone(), false, Duration::from_millis(100), true).await;

        let cache_dir = tempfile::tempdir().unwrap();
        let bloom = local_bloom();
        let config = Config {
            server: crate::config::ServerConfig {
                listen: vec!["127.0.0.1:0".to_string()],
                priority: 30,
            },
            bloom: Default::default(),
            peers: crate::config::PeersConfig {
                urls: vec![broken, good],
            },
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

        let nar_path = format!("{nar_hash}/{HASH}-test.nar.zst");
        let response = serve(&state, HASH, &nar_hash, &nar_path).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-length"],
            compressed.len().to_string().as_str()
        );
        let body = axum::body::to_bytes(response.into_body(), compressed.len())
            .await
            .unwrap();
        assert_eq!(body, compressed);
        assert_eq!(broken_gets.load(Ordering::SeqCst), 1);
        assert_eq!(good_gets.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read_dir(cache_dir.path()).unwrap().count(), 0);
    }
}
