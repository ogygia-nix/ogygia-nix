use std::collections::VecDeque;

use rand::RngExt;
use rand::SeedableRng;
use rand::rngs::StdRng;
use sha2::Digest;
use sha2::Sha256;

use super::*;

const CHUNK: u64 = 1024;

fn config() -> Config {
    Config {
        chunk_size: CHUNK,
        stall_timeout: Duration::from_secs(1),
        ..Default::default()
    }
}

fn payload(len: usize) -> Arc<Vec<u8>> {
    Arc::new((0..len).map(|i| (i * 7 % 251) as u8).collect())
}

#[derive(Clone, Copy)]
enum Failure {
    /// The request itself fails.
    Connect,
    /// The body errors on reaching this file offset.
    ErrorAt(u64),
    /// The body stops sending on reaching this file offset.
    StallAt(u64),
    /// The body ends on reaching this file offset.
    ShortAt(u64),
}

/// A scripted source serving `data` in `piece`-byte pieces, one every
/// `piece_delay` after a first byte at `ttfb`.
#[derive(Clone)]
struct Fake {
    name: &'static str,
    data: Arc<Vec<u8>>,
    ttfb: Duration,
    piece: u64,
    piece_delay: Duration,
    failure: Option<Failure>,
    requests: Arc<Mutex<Vec<Range<u64>>>>,
}

impl Fake {
    fn new(name: &'static str, data: &Arc<Vec<u8>>) -> Self {
        Self {
            name,
            data: Arc::clone(data),
            ttfb: Duration::from_millis(1),
            piece: 256,
            piece_delay: Duration::from_millis(1),
            failure: None,
            requests: Arc::default(),
        }
    }

    fn ttfb(mut self, ttfb: Duration) -> Self {
        self.ttfb = ttfb;
        self
    }

    fn pace(mut self, piece: u64, piece_delay: Duration) -> Self {
        self.piece = piece;
        self.piece_delay = piece_delay;
        self
    }

    fn failing(mut self, failure: Failure) -> Self {
        self.failure = Some(failure);
        self
    }

    fn requests(&self) -> Vec<Range<u64>> {
        self.requests.lock().unwrap().clone()
    }
}

impl fmt::Display for Fake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

impl RangeSource for Fake {
    async fn fetch(&self, range: Range<u64>) -> anyhow::Result<ByteStream> {
        self.requests.lock().unwrap().push(range.clone());
        if let Some(Failure::Connect) = self.failure {
            anyhow::bail!("connection refused");
        }
        let fake = self.clone();
        let body =
            futures::stream::unfold((range.start, true), move |(pos, first)| {
                let fake = fake.clone();
                let end = range.end;
                async move {
                    if pos >= end {
                        return None;
                    }
                    tokio::time::sleep(if first { fake.ttfb } else { fake.piece_delay }).await;
                    let mut stop = (pos + fake.piece).min(end);
                    match fake.failure {
                        Some(Failure::ErrorAt(at)) if at == pos => {
                            return Some((Err(anyhow::anyhow!("connection reset")), (end, false)));
                        }
                        Some(Failure::StallAt(at)) if at == pos => {
                            futures::future::pending::<()>().await;
                        }
                        Some(Failure::ShortAt(at)) if at == pos => return None,
                        Some(
                            Failure::ErrorAt(at) | Failure::StallAt(at) | Failure::ShortAt(at),
                        ) if (pos..stop).contains(&at) => stop = at,
                        _ => {}
                    }
                    let piece = Bytes::copy_from_slice(&fake.data[pos as usize..stop as usize]);
                    Some((Ok(piece), (stop, false)))
                }
            });
        Ok(body.boxed())
    }
}

/// Yield each source at its time after the call, then stay open until
/// `open_until` before ending.
fn arrivals(
    sources: Vec<(Duration, Fake)>,
    open_until: Duration,
) -> impl Stream<Item = Fake> + Send + 'static {
    let start = Instant::now();
    futures::stream::unfold(VecDeque::from(sources), move |mut sources| async move {
        match sources.pop_front() {
            Some((at, source)) => {
                tokio::time::sleep_until(start + at).await;
                Some((source, sources))
            }
            None => {
                tokio::time::sleep_until(start + open_until).await;
                None
            }
        }
    })
}

struct MemStore(Mutex<Vec<u8>>);

impl ChunkStore for MemStore {
    async fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let offset = offset as usize;
        self.0.lock().unwrap()[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    async fn read_at(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let offset = offset as usize;
        Ok(Bytes::copy_from_slice(
            &self.0.lock().unwrap()[offset..offset + len],
        ))
    }
}

struct Sha256Verifier {
    hasher: Sha256,
    expected: [u8; 32],
}

impl PrefixVerifier for Sha256Verifier {
    async fn update(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.hasher.update(data);
        Ok(())
    }

    async fn finish(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            <[u8; 32]>::from(self.hasher.finalize()) == self.expected,
            "hash mismatch"
        );
        Ok(())
    }
}

struct Outcome {
    completion: Completion,
    elapsed: Duration,
    stored: Vec<u8>,
    watermarks: Vec<u64>,
}

async fn download(
    config: Config,
    data: &[u8],
    sources: impl Stream<Item = Fake> + Send + 'static,
) -> Outcome {
    let started = Instant::now();
    let store = Arc::new(MemStore(Mutex::new(vec![0; data.len()])));
    let verifier = Sha256Verifier {
        hasher: Sha256::new(),
        expected: Sha256::digest(data).into(),
    };
    let download = start(
        config,
        data.len() as u64,
        sources,
        Arc::clone(&store),
        verifier,
    );
    let mut watermark = download.watermark();
    let watcher = tokio::spawn(async move {
        let mut seen = vec![*watermark.borrow_and_update()];
        while watermark.changed().await.is_ok() {
            seen.push(*watermark.borrow_and_update());
        }
        seen
    });
    let completion = download.finish().await;
    let elapsed = started.elapsed();
    let stored = store.0.lock().unwrap().clone();
    Outcome {
        completion,
        elapsed,
        stored,
        watermarks: watcher.await.unwrap(),
    }
}

fn stats<'a>(outcome: &'a Outcome, name: &str) -> &'a SourceStats {
    outcome
        .completion
        .sources
        .iter()
        .find(|s| s.source == name)
        .unwrap_or_else(|| panic!("{name} was not admitted"))
}

#[tokio::test(start_paused = true)]
async fn test_slow_first_source_is_out_raced() {
    let data = payload(1000);
    let slow = Fake::new("slow", &data).ttfb(Duration::from_secs(60));
    let fast = Fake::new("fast", &data);
    let config = Config {
        ttfb_limit: Duration::from_millis(5),
        stall_timeout: Duration::from_secs(120),
        ..config()
    };
    let sources = arrivals(
        vec![
            (Duration::ZERO, slow.clone()),
            (Duration::ZERO, fast.clone()),
        ],
        Duration::ZERO,
    );

    let outcome = download(config, &data, sources).await;
    outcome.completion.result.as_ref().unwrap();
    assert_eq!(outcome.stored, *data);
    assert_eq!(
        stats(&outcome, "fast").arrived_after,
        Duration::from_millis(10)
    );
    assert_eq!(stats(&outcome, "fast").chunks_won, 1);
    assert_eq!(stats(&outcome, "slow").chunks_won, 0);
    assert_eq!(stats(&outcome, "slow").fault, None);
    assert_eq!(slow.requests(), vec![0..1000]);
    assert_eq!(fast.requests(), vec![0..1000]);
    assert!(outcome.elapsed < Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn test_stalled_chunk_is_refetched_elsewhere() {
    let data = payload(4 * CHUNK as usize);
    let stalls = Fake::new("stalls", &data).failing(Failure::StallAt(CHUNK + CHUNK / 2));
    let steady = Fake::new("steady", &data);
    let config = Config {
        slow_ratio: 0.0,
        ..config()
    };
    let sources = arrivals(
        vec![
            (Duration::ZERO, stalls.clone()),
            (Duration::ZERO, steady.clone()),
        ],
        Duration::ZERO,
    );

    let outcome = download(config, &data, sources).await;
    outcome.completion.result.as_ref().unwrap();
    assert_eq!(outcome.stored, *data);
    assert!(
        stats(&outcome, "stalls")
            .fault
            .as_ref()
            .is_some_and(|f| f.contains("stalled"))
    );
    assert_eq!(stalls.requests(), vec![0..CHUNK, CHUNK..2 * CHUNK]);
    assert!(steady.requests().contains(&(CHUNK..2 * CHUNK)));
}

#[tokio::test(start_paused = true)]
async fn test_wrong_bytes_fail_the_download() {
    let data = payload(2 * CHUNK as usize);
    let mut corrupt = (*data).clone();
    corrupt[CHUNK as usize + 1] ^= 0xff;
    let honest = Fake::new("honest", &data).pace(64, Duration::from_millis(1));
    let liar = Fake::new("liar", &Arc::new(corrupt));
    let sources = arrivals(
        vec![(Duration::ZERO, honest), (Duration::ZERO, liar.clone())],
        Duration::ZERO,
    );

    let outcome = download(config(), &data, sources).await;
    assert!(matches!(outcome.completion.result, Err(Error::Verify(_))));
    assert_eq!(liar.requests(), vec![CHUNK..2 * CHUNK]);
}

#[tokio::test(start_paused = true)]
async fn test_waits_for_a_late_source_while_the_stream_is_open() {
    let data = payload(2 * CHUNK as usize);
    let refused = Fake::new("refused", &data).failing(Failure::Connect);
    let resets = Fake::new("resets", &data).failing(Failure::ErrorAt(100));
    let late = Fake::new("late", &data);
    let sources = arrivals(
        vec![
            (Duration::ZERO, refused),
            (Duration::ZERO, resets),
            (Duration::from_secs(60), late),
        ],
        Duration::from_secs(60),
    );

    let outcome = download(config(), &data, sources).await;
    outcome.completion.result.as_ref().unwrap();
    assert_eq!(outcome.stored, *data);
    assert!(stats(&outcome, "refused").fault.is_some());
    assert!(stats(&outcome, "resets").fault.is_some());
    assert_eq!(stats(&outcome, "late").chunks_won, 2);
    assert!(outcome.elapsed >= Duration::from_secs(60));
}

#[tokio::test(start_paused = true)]
async fn test_fails_only_once_the_stream_ends() {
    let data = payload(2 * CHUNK as usize);
    let refused = Fake::new("refused", &data).failing(Failure::Connect);
    let short = Fake::new("short", &data).failing(Failure::ShortAt(100));
    let sources = arrivals(
        vec![(Duration::ZERO, refused), (Duration::ZERO, short)],
        Duration::from_secs(30),
    );

    let outcome = download(config(), &data, sources).await;
    assert!(matches!(outcome.completion.result, Err(Error::NoSources)));
    assert_eq!(outcome.elapsed, Duration::from_secs(30));
    assert_eq!(outcome.watermarks.last(), Some(&0));
}

/// Any mix of faulty sources plus one honest source, arriving at any time,
/// yields the exact file with a watermark that never goes backwards.
#[tokio::test(start_paused = true)]
async fn test_random_scripts_with_an_honest_source_succeed() {
    for seed in 0..200 {
        let mut rng = StdRng::seed_from_u64(seed);
        let data = payload(rng.random_range(1..5000));
        let len = data.len() as u64;
        let ms = |rng: &mut StdRng, max: u64| Duration::from_millis(rng.random_range(0..max));
        let mut sources = Vec::new();
        for _ in 0..rng.random_range(0..5) {
            let at = rng.random_range(0..len);
            let failure = match rng.random_range(0..4) {
                0 => Failure::Connect,
                1 => Failure::ErrorAt(at),
                2 => Failure::StallAt(at),
                _ => Failure::ShortAt(at),
            };
            let source = Fake::new("faulty", &data)
                .ttfb(ms(&mut rng, 3000))
                .pace(rng.random_range(16..512), ms(&mut rng, 50))
                .failing(failure);
            sources.push((ms(&mut rng, 3000), source));
        }
        // Within the stall timeout, so it never faults.
        let honest = Fake::new("honest", &data)
            .ttfb(ms(&mut rng, 900))
            .pace(rng.random_range(16..512), ms(&mut rng, 50));
        sources.push((ms(&mut rng, 3000), honest));
        sources.sort_by_key(|(at, _)| *at);
        let config = Config {
            chunk_size: rng.random_range(256..1024),
            ttfb_limit: ms(&mut rng, 1000),
            trial: ms(&mut rng, 1000),
            ..config()
        };

        let outcome = download(config, &data, arrivals(sources, Duration::from_secs(5))).await;
        assert!(
            outcome.completion.result.is_ok(),
            "seed {seed}: {:?}",
            outcome.completion.result
        );
        assert_eq!(outcome.stored, *data, "seed {seed}");
        assert!(outcome.watermarks.is_sorted(), "seed {seed}");
        assert_eq!(outcome.watermarks.last(), Some(&len), "seed {seed}");
    }
}
