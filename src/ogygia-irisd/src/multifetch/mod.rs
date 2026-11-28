//! Chunked download of one file from many interchangeable sources.
//!
//! Every source must serve byte-identical content. Sources are admitted in
//! the order the source stream yields them and fetch chunks in file order,
//! so the in-order prefix grows as early as possible. A source that faults
//! leaves the download and its chunk goes to another source.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::ops::Range;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use bytes::BytesMut;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use futures::future::AbortHandle;
use futures::future::Aborted;
use futures::future::BoxFuture;
use futures::future::abortable;
use futures::stream::BoxStream;
use futures::stream::FuturesUnordered;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::MissedTickBehavior;
use tracing::Instrument;

#[cfg(test)]
mod tests;

/// Tuning for one download.
pub struct Config {
    /// Bytes per range request; the last chunk may be shorter.
    pub chunk_size: u64,
    /// At most one more source is admitted per interval.
    pub admission_interval: Duration,
    /// A request that delivers nothing for this long is a fault.
    pub stall_timeout: Duration,
    /// Most sources active at once; `None` means one per chunk plus one.
    pub peer_cap: Option<usize>,
    /// A chunk whose request has delivered no byte after this long may be
    /// duplicated on another source.
    pub ttfb_limit: Duration,
    /// A chunk whose request runs slower than this fraction of the median
    /// rate of the other sources may be duplicated.
    pub slow_ratio: f64,
    /// How long a duplicate runs before it is kept or cancelled.
    pub trial: Duration,
    /// Admission stops once a trial slows the original by at least this
    /// fraction of the duplicate's rate.
    pub bottleneck_ratio: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chunk_size: 2 * 1024 * 1024,
            admission_interval: Duration::from_millis(10),
            stall_timeout: Duration::from_secs(30),
            peer_cap: None,
            ttfb_limit: Duration::from_millis(500),
            slow_ratio: 0.1,
            trial: Duration::from_secs(2),
            bottleneck_ratio: 0.5,
        }
    }
}

/// The body of a range response.
pub type ByteStream = BoxStream<'static, anyhow::Result<Bytes>>;

/// One place the file can be fetched from.
pub trait RangeSource: fmt::Display + Send + Sync + 'static {
    /// Fetch `range` of the file. An error, here or in the stream, is a
    /// fault that removes this source from the download.
    fn fetch(&self, range: Range<u64>) -> impl Future<Output = anyhow::Result<ByteStream>> + Send;
}

/// Fixed-length storage the file is assembled in.
pub trait ChunkStore: Send + Sync + 'static {
    fn write_at(&self, offset: u64, data: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
    fn read_at(&self, offset: u64, len: usize) -> impl Future<Output = io::Result<Bytes>> + Send;
}

/// Checks the assembled file.
pub trait PrefixVerifier: Send + 'static {
    /// Called with the whole file in order, each byte once. An error fails
    /// the download.
    fn update(&mut self, data: &[u8]) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Called once the whole file has been passed to `update`.
    fn finish(self) -> impl Future<Output = anyhow::Result<()>> + Send;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no source left to fetch from")]
    NoSources,
    #[error("chunk store failed: {0}")]
    Store(#[from] io::Error),
    #[error("verification failed: {0:#}")]
    Verify(anyhow::Error),
}

/// How a download ended.
pub struct Completion {
    pub result: Result<(), Error>,
    /// Every admitted source, in admission order.
    pub sources: Vec<SourceStats>,
}

#[derive(Debug, Clone)]
pub struct SourceStats {
    pub source: String,
    /// When the source stream yielded it, from the start of the download.
    pub arrived_after: Duration,
    /// Time to first byte of its first request that delivered one.
    pub first_byte: Option<Duration>,
    pub chunks_won: usize,
    pub bytes_won: u64,
    /// Why it left the download, if it did.
    pub fault: Option<String>,
}

/// A running download. Dropping it cancels the download.
pub struct Download {
    watermark: watch::Receiver<u64>,
    task: AbortOnDrop,
}

struct AbortOnDrop(JoinHandle<Completion>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Download {
    /// Bytes below the watermark are final in the store. It never
    /// decreases, and the channel closes when the download ends.
    pub fn watermark(&self) -> watch::Receiver<u64> {
        self.watermark.clone()
    }

    pub async fn finish(mut self) -> Completion {
        match (&mut self.task.0).await {
            Ok(completion) => completion,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

/// Start downloading a `file_size`-byte file into `store`, which must
/// already be that long, from the sources `sources` yields.
///
/// The source stream is polled only when another source is wanted, and a
/// download that runs out of sources waits until the stream ends before it
/// fails.
pub fn start<S, St, C, V>(
    config: Config,
    file_size: u64,
    sources: St,
    store: Arc<C>,
    verifier: V,
) -> Download
where
    S: RangeSource,
    St: Stream<Item = S> + Send + 'static,
    C: ChunkStore,
    V: PrefixVerifier,
{
    let (watermark_tx, watermark) = watch::channel(0);
    let num_chunks = file_size.div_ceil(config.chunk_size) as usize;
    let run = Run {
        peer_cap: config.peer_cap.unwrap_or(num_chunks + 1),
        config,
        file_size,
        store,
        watermark: watermark_tx,
        started: Instant::now(),
        chunks: (0..num_chunks).map(|_| Chunk::Free).collect(),
        peers: Vec::new(),
        attempts: HashMap::new(),
        next_attempt: 0,
        running: FuturesUnordered::new(),
        admitting: true,
    };
    let task = tokio::spawn(
        run.run(sources, verifier)
            .instrument(tracing::Span::current()),
    );
    Download {
        watermark,
        task: AbortOnDrop(task),
    }
}

enum Chunk {
    Free,
    Fetching {
        primary: u64,
        duplicate: Option<u64>,
        // Each primary request is trialled against at most one duplicate.
        duplicated: bool,
    },
    Done,
}

#[derive(PartialEq)]
enum PeerState {
    Idle,
    Busy(u64),
    Gone,
}

struct Peer<S> {
    source: Arc<S>,
    state: PeerState,
    stats: SourceStats,
    // Totals over its finished requests, for its rate.
    bytes: u64,
    busy_for: Duration,
}

struct Attempt {
    peer: usize,
    chunk: usize,
    started: Instant,
    progress: Arc<Mutex<Progress>>,
    abort: AbortHandle,
    trial: Option<Trial>,
}

struct Trial {
    ends: Instant,
    original_received: u64,
    original_rate: f64,
}

#[derive(Default, Clone, Copy)]
struct Progress {
    received: u64,
    first_byte: Option<Instant>,
}

impl Attempt {
    fn progress(&self) -> Progress {
        *self.progress.lock().unwrap()
    }

    /// Bytes received so far and the rate since the request started.
    fn rate(&self, now: Instant) -> (u64, f64) {
        let received = self.progress().received;
        let elapsed = (now - self.started).as_secs_f64();
        let rate = if elapsed > 0.0 {
            received as f64 / elapsed
        } else {
            0.0
        };
        (received, rate)
    }
}

fn eta(remaining: u64, rate: f64) -> f64 {
    if rate > 0.0 {
        remaining as f64 / rate
    } else {
        f64::INFINITY
    }
}

type Running = BoxFuture<'static, (u64, Result<anyhow::Result<Bytes>, Aborted>)>;

struct Run<S, C> {
    config: Config,
    file_size: u64,
    peer_cap: usize,
    store: Arc<C>,
    watermark: watch::Sender<u64>,
    started: Instant,
    chunks: Vec<Chunk>,
    peers: Vec<Peer<S>>,
    attempts: HashMap<u64, Attempt>,
    next_attempt: u64,
    running: FuturesUnordered<Running>,
    admitting: bool,
}

impl<S: RangeSource, C: ChunkStore> Run<S, C> {
    async fn run<St, V>(mut self, sources: St, verifier: V) -> Completion
    where
        St: Stream<Item = S> + Send + 'static,
        V: PrefixVerifier,
    {
        let result = self.drive(sources, verifier).await;
        match &result {
            Ok(()) => tracing::info!(
                "Downloaded {} bytes in {:?}",
                self.file_size,
                self.started.elapsed()
            ),
            Err(e) => tracing::warn!("Download failed after {:?}: {}", self.started.elapsed(), e),
        }
        for peer in &self.peers {
            let stats = &peer.stats;
            tracing::info!(
                "Source {}: arrived after {:?}, first byte after {:?}, won {} chunks ({} bytes), fault: {}",
                stats.source,
                stats.arrived_after,
                stats.first_byte,
                stats.chunks_won,
                stats.bytes_won,
                stats.fault.as_deref().unwrap_or("none"),
            );
        }
        Completion {
            result,
            sources: self.peers.into_iter().map(|p| p.stats).collect(),
        }
    }

    async fn drive<St, V>(&mut self, sources: St, mut verifier: V) -> Result<(), Error>
    where
        St: Stream<Item = S> + Send + 'static,
        V: PrefixVerifier,
    {
        let mut sources = pin!(sources);
        let mut stream_open = true;
        let mut admit_due = true;
        let interval = self.config.admission_interval;
        let mut tick = tokio::time::interval_at(self.started + interval, interval);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut fed = 0;

        loop {
            while fed < self.chunks.len() && matches!(self.chunks[fed], Chunk::Done) {
                let range = self.range(fed);
                let data = self
                    .store
                    .read_at(range.start, (range.end - range.start) as usize)
                    .await?;
                verifier.update(&data).await.map_err(Error::Verify)?;
                fed += 1;
                self.watermark.send_modify(|w| *w = range.end);
            }
            if fed == self.chunks.len() {
                return verifier.finish().await.map_err(Error::Verify);
            }

            let live = self
                .peers
                .iter()
                .filter(|p| p.state != PeerState::Gone)
                .count();
            if !stream_open && live == 0 {
                return Err(Error::NoSources);
            }
            let want_source = stream_open
                && (live == 0
                    || (admit_due
                        && self.admitting
                        && live < self.peer_cap
                        && self.has_work(Instant::now())));

            tokio::select! {
                biased;

                Some((id, outcome)) = self.running.next() => {
                    if let Ok(result) = outcome {
                        self.attempt_done(id, result).await?;
                    }
                }

                source = sources.next(), if want_source => match source {
                    Some(source) => {
                        self.admit(source);
                        admit_due = false;
                    }
                    None => stream_open = false,
                },

                _ = tick.tick() => {
                    admit_due = true;
                    self.judge_trials();
                }
            }

            self.assign_idle();
        }
    }

    fn range(&self, chunk: usize) -> Range<u64> {
        let start = chunk as u64 * self.config.chunk_size;
        start..(start + self.config.chunk_size).min(self.file_size)
    }

    fn admit(&mut self, source: S) {
        let arrived_after = self.started.elapsed();
        tracing::info!("Source {} arrived after {:?}", source, arrived_after);
        self.peers.push(Peer {
            stats: SourceStats {
                source: source.to_string(),
                arrived_after,
                first_byte: None,
                chunks_won: 0,
                bytes_won: 0,
                fault: None,
            },
            source: Arc::new(source),
            state: PeerState::Idle,
            bytes: 0,
            busy_for: Duration::ZERO,
        });
    }

    /// Whether a newly admitted source would have something to fetch.
    fn has_work(&self, now: Instant) -> bool {
        self.chunks.iter().any(|c| matches!(c, Chunk::Free))
            || self.duplicate_candidate(now).is_some()
    }

    /// Give every idle source the earliest free chunk, or else a chunk to
    /// duplicate.
    fn assign_idle(&mut self) {
        let now = Instant::now();
        for peer in 0..self.peers.len() {
            if self.peers[peer].state != PeerState::Idle {
                continue;
            }
            if let Some(chunk) = self.chunks.iter().position(|c| matches!(c, Chunk::Free)) {
                let primary = self.launch(peer, chunk, None);
                self.chunks[chunk] = Chunk::Fetching {
                    primary,
                    duplicate: None,
                    duplicated: false,
                };
            } else if let Some(chunk) = self.duplicate_candidate(now) {
                let Chunk::Fetching { primary, .. } = self.chunks[chunk] else {
                    unreachable!("duplicate candidates are being fetched");
                };
                let original = &self.attempts[&primary];
                let (original_received, original_rate) = original.rate(now);
                tracing::info!(
                    "Chunk {}: duplicating {} on {}",
                    chunk,
                    self.peers[original.peer].stats.source,
                    self.peers[peer].stats.source,
                );
                let trial = Trial {
                    ends: now + self.config.trial,
                    original_received,
                    original_rate,
                };
                let id = self.launch(peer, chunk, Some(trial));
                if let Chunk::Fetching {
                    duplicate,
                    duplicated,
                    ..
                } = &mut self.chunks[chunk]
                {
                    *duplicate = Some(id);
                    *duplicated = true;
                }
            } else {
                break;
            }
        }
    }

    /// The chunk an idle source should duplicate: of the chunks whose
    /// request looks broken, the one that will finish last.
    fn duplicate_candidate(&self, now: Instant) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (chunk, state) in self.chunks.iter().enumerate() {
            let Chunk::Fetching {
                primary,
                duplicate: None,
                duplicated: false,
            } = state
            else {
                continue;
            };
            let attempt = &self.attempts[primary];
            if !self.looks_broken(attempt, now) {
                continue;
            }
            let range = self.range(chunk);
            let (received, rate) = attempt.rate(now);
            let eta = eta(range.end - range.start - received, rate);
            if best.is_none_or(|(_, best_eta)| eta > best_eta) {
                best = Some((chunk, eta));
            }
        }
        best.map(|(chunk, _)| chunk)
    }

    fn looks_broken(&self, attempt: &Attempt, now: Instant) -> bool {
        if now - attempt.started < self.config.ttfb_limit {
            return false;
        }
        if attempt.progress().first_byte.is_none() {
            return true;
        }
        let mut others: Vec<f64> = (0..self.peers.len())
            .filter(|&p| p != attempt.peer && self.peers[p].state != PeerState::Gone)
            .filter_map(|p| self.peer_rate(p, now))
            .collect();
        if others.is_empty() {
            return false;
        }
        others.sort_by(f64::total_cmp);
        let median = others[others.len() / 2];
        attempt.rate(now).1 < self.config.slow_ratio * median
    }

    fn peer_rate(&self, peer: usize, now: Instant) -> Option<f64> {
        let p = &self.peers[peer];
        let (mut bytes, mut time) = (p.bytes, p.busy_for);
        if let PeerState::Busy(id) = p.state {
            let attempt = &self.attempts[&id];
            bytes += attempt.progress().received;
            time += now - attempt.started;
        }
        (bytes > 0 && !time.is_zero()).then(|| bytes as f64 / time.as_secs_f64())
    }

    fn launch(&mut self, peer: usize, chunk: usize, trial: Option<Trial>) -> u64 {
        let id = self.next_attempt;
        self.next_attempt += 1;
        let progress = Arc::new(Mutex::new(Progress::default()));
        let (fetch, abort) = abortable(fetch_chunk(
            Arc::clone(&self.peers[peer].source),
            self.range(chunk),
            self.config.stall_timeout,
            Arc::clone(&progress),
        ));
        self.running.push(fetch.map(move |r| (id, r)).boxed());
        self.attempts.insert(
            id,
            Attempt {
                peer,
                chunk,
                started: Instant::now(),
                progress,
                abort,
                trial,
            },
        );
        self.peers[peer].state = PeerState::Busy(id);
        id
    }

    /// Stop a request that is not at fault; its source becomes idle.
    fn cancel(&mut self, id: u64) -> String {
        let attempt = self
            .attempts
            .remove(&id)
            .expect("cancelled request is running");
        attempt.abort.abort();
        self.retire(&attempt, Instant::now());
        let peer = &mut self.peers[attempt.peer];
        peer.state = PeerState::Idle;
        peer.stats.source.clone()
    }

    /// Fold a finished request into its source's totals.
    fn retire(&mut self, attempt: &Attempt, now: Instant) {
        let progress = attempt.progress();
        let peer = &mut self.peers[attempt.peer];
        peer.bytes += progress.received;
        peer.busy_for += now - attempt.started;
        if peer.stats.first_byte.is_none() {
            peer.stats.first_byte = progress.first_byte.map(|t| t - attempt.started);
        }
    }

    async fn attempt_done(&mut self, id: u64, result: anyhow::Result<Bytes>) -> Result<(), Error> {
        let Some(attempt) = self.attempts.remove(&id) else {
            return Ok(());
        };
        let now = Instant::now();
        self.retire(&attempt, now);
        let chunk = attempt.chunk;
        let source = self.peers[attempt.peer].stats.source.clone();

        let data = match result {
            Ok(data) => data,
            Err(fault) => {
                tracing::warn!(
                    "Source {} left after a fault on chunk {}: {:#}",
                    source,
                    chunk,
                    fault
                );
                let peer = &mut self.peers[attempt.peer];
                peer.state = PeerState::Gone;
                peer.stats.fault = Some(format!("{fault:#}"));
                let Chunk::Fetching {
                    primary, duplicate, ..
                } = self.chunks[chunk]
                else {
                    unreachable!("a running request's chunk is being fetched");
                };
                self.chunks[chunk] = match (primary == id, duplicate) {
                    (false, _) => Chunk::Fetching {
                        primary,
                        duplicate: None,
                        duplicated: true,
                    },
                    (true, Some(duplicate)) => {
                        if let Some(promoted) = self.attempts.get_mut(&duplicate) {
                            promoted.trial = None;
                        }
                        Chunk::Fetching {
                            primary: duplicate,
                            duplicate: None,
                            duplicated: false,
                        }
                    }
                    (true, None) => Chunk::Free,
                };
                return Ok(());
            }
        };

        let elapsed = now - attempt.started;
        tracing::debug!(
            "Chunk {} from {}: {} bytes in {:?} ({:.0} B/s), first byte after {:?}",
            chunk,
            source,
            data.len(),
            elapsed,
            data.len() as f64 / elapsed.as_secs_f64(),
            attempt.progress().first_byte.map(|t| t - attempt.started),
        );
        let peer = &mut self.peers[attempt.peer];
        peer.state = PeerState::Idle;
        peer.stats.chunks_won += 1;
        peer.stats.bytes_won += data.len() as u64;

        let Chunk::Fetching {
            primary, duplicate, ..
        } = self.chunks[chunk]
        else {
            unreachable!("a running request's chunk is being fetched");
        };
        let loser = if primary == id {
            duplicate
        } else {
            Some(primary)
        };
        if let Some(loser) = loser {
            let loser = self.cancel(loser);
            tracing::info!("Chunk {}: {} beat {}", chunk, source, loser);
        }

        self.store.write_at(self.range(chunk).start, &data).await?;
        self.chunks[chunk] = Chunk::Done;
        Ok(())
    }

    /// Keep each duplicate whose trial is over if it would finish first;
    /// otherwise cancel it.
    fn judge_trials(&mut self) {
        let now = Instant::now();
        let due: Vec<u64> = self
            .attempts
            .iter()
            .filter(|(_, a)| a.trial.as_ref().is_some_and(|t| now >= t.ends))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            let duplicate = &self.attempts[&id];
            let chunk = duplicate.chunk;
            let len = {
                let range = self.range(chunk);
                range.end - range.start
            };
            let Chunk::Fetching { primary, .. } = self.chunks[chunk] else {
                unreachable!("a running request's chunk is being fetched");
            };
            let trial = duplicate.trial.as_ref().expect("filtered on trial");
            let (duplicate_received, duplicate_rate) = duplicate.rate(now);
            let original = &self.attempts[&primary];
            let original_received = original.progress().received;
            let original_rate = (original_received - trial.original_received) as f64
                / (now - duplicate.started).as_secs_f64();
            let duplicate_eta = eta(len - duplicate_received, duplicate_rate);
            let original_eta = eta(len - original_received, original_rate);

            if trial.original_rate > 0.0
                && duplicate_rate > 0.0
                && trial.original_rate - original_rate
                    >= self.config.bottleneck_ratio * duplicate_rate
            {
                tracing::info!(
                    "Chunk {}: the duplicate slowed the original from {:.0} to {:.0} B/s; admitting no more sources",
                    chunk,
                    trial.original_rate,
                    original_rate,
                );
                self.admitting = false;
            }

            let duplicate_source = &self.peers[duplicate.peer].stats.source;
            if duplicate_eta < original_eta {
                tracing::info!(
                    "Chunk {}: keeping duplicate on {} (done in {:.1}s, original in {:.1}s)",
                    chunk,
                    duplicate_source,
                    duplicate_eta,
                    original_eta,
                );
                self.attempts
                    .get_mut(&id)
                    .expect("judged request is running")
                    .trial = None;
            } else {
                tracing::info!(
                    "Chunk {}: cancelling duplicate on {} (done in {:.1}s, original in {:.1}s)",
                    chunk,
                    duplicate_source,
                    duplicate_eta,
                    original_eta,
                );
                self.cancel(id);
                if let Chunk::Fetching { duplicate, .. } = &mut self.chunks[chunk] {
                    *duplicate = None;
                }
            }
        }
    }
}

/// Fetch one chunk, failing on a stall or a body of the wrong length.
async fn fetch_chunk<S: RangeSource>(
    source: Arc<S>,
    range: Range<u64>,
    stall_timeout: Duration,
    progress: Arc<Mutex<Progress>>,
) -> anyhow::Result<Bytes> {
    let len = (range.end - range.start) as usize;
    let mut body = tokio::time::timeout(stall_timeout, source.fetch(range))
        .await
        .map_err(|_| anyhow::anyhow!("no response within {stall_timeout:?}"))??;
    let mut data = BytesMut::with_capacity(len);
    loop {
        let piece = match tokio::time::timeout(stall_timeout, body.next()).await {
            Err(_) => anyhow::bail!("stalled after {} of {} bytes", data.len(), len),
            Ok(None) => break,
            Ok(Some(piece)) => piece?,
        };
        if piece.is_empty() {
            continue;
        }
        data.extend_from_slice(&piece);
        anyhow::ensure!(
            data.len() <= len,
            "body longer than the {len} bytes requested"
        );
        let mut progress = progress.lock().unwrap();
        progress.received = data.len() as u64;
        progress.first_byte.get_or_insert_with(Instant::now);
    }
    anyhow::ensure!(
        data.len() == len,
        "short body: {} of {} bytes",
        data.len(),
        len
    );
    Ok(data.freeze())
}
