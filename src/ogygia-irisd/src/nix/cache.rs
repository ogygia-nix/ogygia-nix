//! Disk-backed NAR cache with moka-managed metadata
//!
//! Compressed NAR files are stored on disk under a configurable directory.
//! `moka::future::Cache` manages in-memory metadata (file path, compressed
//! hash, compressed size) with time-to-idle and weighted-size eviction.
//! An async eviction listener deletes the backing file when moka evicts
//! an entry.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use async_compression::tokio::bufread::ZstdEncoder;
use moka::future::Cache;
use ogygia_nixutils::NarHash;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;

use crate::config::CacheConfig;

/// Metadata for a cached, zstd-compressed NAR file on disk.
pub struct CachedNar {
    /// Guarded path to the `.nar.zst` file on disk.
    ///
    /// Set to `None` by the eviction listener just before deleting the file.
    /// Readers acquire the read lock and open the file before releasing it,
    /// so the fd keeps the inode alive even after unlink.
    path: tokio::sync::RwLock<Option<PathBuf>>,

    /// SHA-256 of the compressed file content.
    pub file_hash: NarHash,

    /// Size of the compressed file in bytes.
    pub file_size: u64,
}

impl CachedNar {
    /// Open the cached NAR file for reading.
    ///
    /// Uses `try_read` rather than `read` because the write lock is only
    /// ever taken by the eviction listener, which sets the path to `None`
    /// and never releases it. If the write lock is held, the path is
    /// already gone and there is no point waiting.
    ///
    /// Returns `None` if the entry is being evicted or has already been evicted.
    pub async fn open(&self) -> Result<Option<tokio::fs::File>> {
        let guard = match self.path.try_read() {
            Ok(g) => g,
            Err(_) => return Ok(None),
        };
        let Some(path) = guard.as_ref() else {
            return Ok(None);
        };
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("Failed to open cached NAR: {}", path.display()))?;
        Ok(Some(file))
    }
}

/// Disk-backed NAR cache with moka-managed eviction.
pub struct NarCache {
    cache: Cache<String, Arc<CachedNar>>,
    dir: PathBuf,
}

impl NarCache {
    /// Create a new NAR cache from configuration.
    ///
    /// Creates the cache directory if it does not exist and builds the
    /// moka cache with the configured eviction policies.
    pub async fn new(config: &CacheConfig) -> Result<Self> {
        tokio::fs::create_dir_all(&config.dir)
            .await
            .with_context(|| format!("Failed to create cache dir: {}", config.dir.display()))?;

        let mut builder = Cache::builder()
            .weigher(|_key: &String, value: &Arc<CachedNar>| -> u32 {
                value.file_size.min(u32::MAX as u64) as u32
            })
            .async_eviction_listener(|_key, value: Arc<CachedNar>, cause| {
                Box::pin(async move {
                    // Take the path under the write lock so no new readers
                    // can open the file after this point.
                    let path = value.path.write().await.take();
                    if let Some(path) = path {
                        tracing::debug!(
                            "Evicting cached NAR {} (cause: {:?})",
                            path.display(),
                            cause,
                        );
                        if let Err(e) = tokio::fs::remove_file(&path).await {
                            tracing::warn!(
                                "Failed to delete evicted NAR {}: {}",
                                path.display(),
                                e,
                            );
                        }
                    }
                })
            });

        if config.max_size_bytes > 0 {
            builder = builder.max_capacity(config.max_size_bytes);
        }

        if config.time_to_idle_secs > 0 {
            builder = builder.time_to_idle(Duration::from_secs(config.time_to_idle_secs));
        }

        let cache = builder.build();

        Ok(Self {
            cache,
            dir: config.dir.clone(),
        })
    }

    /// Ensure a NAR is cached, generating it if absent. Returns metadata only.
    ///
    /// Uses moka's `try_get_with` to coalesce concurrent requests for the
    /// same uncached NAR into a single generation.
    pub async fn ensure(
        &self,
        store_hash: &str,
        store_path: &Path,
    ) -> Result<Arc<CachedNar>, Arc<anyhow::Error>> {
        let dir = self.dir.clone();
        let store_path = store_path.to_path_buf();

        self.cache
            .try_get_with(store_hash.to_string(), async move {
                generate_cached_nar(&dir, &store_path).await
            })
            .await
    }

    /// Look up an already-cached NAR without generating it.
    pub async fn get(&self, store_hash: &str) -> Option<Arc<CachedNar>> {
        self.cache.get(store_hash).await
    }

    /// Scan the cache directory and re-populate the moka cache from existing files.
    ///
    /// This allows the cache to survive daemon restarts. For each `.nar.zst`
    /// file found, we stat it, compute its SHA-256, and insert the metadata
    /// into the moka cache.
    pub async fn recover(&self) -> Result<usize> {
        let mut count = 0;
        let mut read_dir = tokio::fs::read_dir(&self.dir).await?;

        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let name = match entry.file_name().to_str().map(String::from) {
                Some(n) => n,
                None => continue,
            };

            // Delete leftover temp files from interrupted generations
            if name.ends_with(".nar.zst.tmp") {
                let path = entry.path();
                tracing::info!("Deleting leftover temp file: {}", path.display());
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    tracing::warn!("Failed to delete temp file {}: {}", path.display(), e);
                }
                continue;
            }

            if !name.ends_with(".nar.zst") {
                continue;
            }

            // Extract store hash: first 32 chars of the filename
            let store_hash = match name.get(..32) {
                Some(h) if h.len() == 32 && h.chars().all(|c| c.is_ascii_alphanumeric()) => h,
                _ => continue,
            };

            let path = entry.path();
            let meta = match tokio::fs::metadata(&path).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("Failed to stat cached NAR {}: {}", path.display(), e);
                    continue;
                }
            };

            let file_size = meta.len();
            let file_hash = match hash_file(&path).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("Failed to hash cached NAR {}: {}", path.display(), e);
                    continue;
                }
            };

            let cached = Arc::new(CachedNar {
                path: tokio::sync::RwLock::new(Some(path)),
                file_hash,
                file_size,
            });

            self.cache.insert(store_hash.to_string(), cached).await;
            count += 1;
        }

        tracing::info!("Recovered {} cached NARs from disk", count);
        Ok(count)
    }
}

/// Generate a zstd-compressed NAR file, write it to disk, and return metadata.
///
/// 1. Spawns `nix-store --dump <store_path>` piped through `ZstdEncoder`.
/// 2. Writes compressed output to `{store_name}.nar.zst.tmp`.
/// 3. Computes SHA-256 of the compressed content while writing.
/// 4. Renames to `{store_name}.nar.zst`.
/// 5. Returns `CachedNar` with the file path, hash, and size.
async fn generate_cached_nar(cache_dir: &Path, store_path: &Path) -> Result<Arc<CachedNar>> {
    let store_name = store_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid store path: {}", store_path.display()))?;

    let final_path = cache_dir.join(format!("{store_name}.nar.zst"));
    let tmp_path = cache_dir.join(format!("{store_name}.nar.zst.tmp"));

    // If the final file already exists on disk (e.g. recovery raced with
    // generation), use it directly.
    if let Ok(meta) = tokio::fs::metadata(&final_path).await {
        let file_size = meta.len();
        let file_hash = hash_file(&final_path).await?;
        return Ok(Arc::new(CachedNar {
            path: tokio::sync::RwLock::new(Some(final_path)),
            file_hash,
            file_size,
        }));
    }

    let mut child = Command::new("nix-store")
        .arg("--dump")
        .arg(store_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "Failed to spawn nix-store --dump for {}",
                store_path.display()
            )
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout"))?;

    let buf_reader = BufReader::new(stdout);
    let mut encoder = ZstdEncoder::new(buf_reader);

    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .with_context(|| format!("Failed to create temp NAR file: {}", tmp_path.display()))?;

    let mut hasher = Sha256::new();
    let mut file_size: u64 = 0;
    let mut buf = vec![0u8; 8192];

    loop {
        let n = encoder.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).await?;
        file_size += n as u64;
    }
    file.flush().await?;
    drop(file);

    let status = child.wait().await?;
    if !status.success() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(anyhow::anyhow!(
            "nix-store --dump failed for {}",
            store_path.display()
        ));
    }

    let file_hash = NarHash::from_bytes(hasher.finalize().into());

    tokio::fs::rename(&tmp_path, &final_path)
        .await
        .with_context(|| {
            format!(
                "Failed to rename {} -> {}",
                tmp_path.display(),
                final_path.display()
            )
        })?;

    tracing::info!(
        "Cached NAR {} ({} bytes compressed)",
        final_path.display(),
        file_size,
    );

    Ok(Arc::new(CachedNar {
        path: tokio::sync::RwLock::new(Some(final_path)),
        file_hash,
        file_size,
    }))
}

/// Compute the SHA-256 hash of a file.
async fn hash_file(path: &Path) -> Result<NarHash> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(NarHash::from_bytes(hasher.finalize().into()))
}
