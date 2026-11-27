//! Read-only access to the Nix SQLite database.
//!
//! The Nix daemon maintains a SQLite database at `/nix/var/nix/db/db.sqlite`
//! with an indexed `path` column in `ValidPaths`. [`NixDb`] provides fast,
//! index-backed lookups that replace shelling out to `nix path-info` and
//! scanning `/nix/store`.
//!
//! Queries run on an async connection pool ([`deadpool_sqlite`]): the handle is
//! cheaply cloneable and shares its pool, and each query is dispatched to a
//! blocking thread so it never stalls the async runtime.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use deadpool_sqlite::Config;
use deadpool_sqlite::Pool;
use deadpool_sqlite::Runtime;
use rusqlite::OptionalExtension;

use crate::path_info::PathInfo;
use crate::types::NarHash;
use crate::types::StoreHash;

/// Default path to the Nix SQLite database.
const DEFAULT_DB_PATH: &str = "/nix/var/nix/db/db.sqlite";

/// Schema for the in-memory test database, mirroring the columns these queries
/// read from a real Nix database.
const IN_MEMORY_SCHEMA: &str = "\
    CREATE TABLE ValidPaths ( \
        id INTEGER PRIMARY KEY, \
        path TEXT UNIQUE, \
        hash TEXT, \
        narSize INTEGER, \
        ultimate INTEGER, \
        sigs TEXT, \
        ca TEXT, \
        deriver TEXT \
    ); \
    CREATE TABLE Refs (referrer INTEGER, reference INTEGER);";

/// Read-only handle to the Nix store database.
///
/// The backend behind [`Nix`](crate::Nix)'s store-metadata queries. Cloning is
/// cheap and shares the underlying connection pool, so the components holding a
/// `Nix` all draw from one pool of read-only connections. The pool can later
/// grow, shrink, or gain smarter recycling without any caller changing how it
/// queries.
#[derive(Clone)]
pub struct NixDb {
    pool: Pool,
}

impl NixDb {
    /// Open the Nix database in read-only mode at the default location.
    pub async fn open() -> Result<Self> {
        Self::open_path(Path::new(DEFAULT_DB_PATH)).await
    }

    /// Open a Nix database at a specific path (useful for tests).
    pub async fn open_path(path: &Path) -> Result<Self> {
        // rusqlite's default `OpenFlags` include `SQLITE_OPEN_URI`, so a
        // `file:…?mode=ro` filename restricts an otherwise read-write open down
        // to read-only. This reads the live WAL database the nix-daemon keeps
        // open (via a private heap wal-index), without needing write access to
        // the `-wal`/`-shm` files.
        let uri = format!("file:{}?mode=ro", path.display());
        let pool = Config::new(uri)
            .create_pool(Runtime::Tokio1)
            .with_context(|| {
                format!(
                    "failed to configure Nix database pool at {}",
                    path.display()
                )
            })?;
        let db = Self { pool };
        // Open one connection eagerly so a missing or unreadable database fails
        // here rather than on the first query.
        db.pool
            .get()
            .await
            .map(drop)
            .with_context(|| format!("failed to open Nix database at {}", path.display()))?;
        Ok(db)
    }

    /// Open an in-memory database with the Nix schema, holding only `paths`.
    ///
    /// Intended for tests that need a [`NixDb`] but no real store. Each
    /// reference must itself be one of `paths`.
    pub async fn open_in_memory(paths: &[PathInfo]) -> Result<Self> {
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;

        // A uniquely-named shared-cache in-memory database, so every connection
        // the pool opens sees the same schema. The pool keeps a connection open
        // for its lifetime, which keeps the database alive.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let uri = format!("file:nixdb-mem-{id}?mode=memory&cache=shared");

        let pool = Config::new(uri)
            .create_pool(Runtime::Tokio1)
            .context("failed to configure in-memory database pool")?;
        let conn = pool
            .get()
            .await
            .context("failed to open in-memory database")?;
        conn.interact(|conn| conn.execute_batch(IN_MEMORY_SCHEMA))
            .await
            .map_err(|e| anyhow::anyhow!("failed to create Nix schema: {e}"))?
            .context("failed to create Nix schema")?;

        let paths = paths.to_vec();
        conn.interact(move |conn| {
            for info in &paths {
                let sigs = info
                    .signatures
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ");
                conn.execute(
                    "INSERT INTO ValidPaths (path, hash, narSize, sigs, ca, deriver) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        info.path,
                        format!("sha256:{}", info.nar_hash.to_hex()),
                        info.nar_size as i64,
                        sigs,
                        info.ca,
                        info.deriver,
                    ],
                )?;
            }
            for info in &paths {
                for reference in &info.references {
                    conn.execute(
                        "INSERT INTO Refs (referrer, reference) \
                         SELECT a.id, b.id FROM ValidPaths a, ValidPaths b \
                         WHERE a.path = ?1 AND b.path = ?2",
                        [&info.path, reference],
                    )?;
                }
            }
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("failed to insert store paths: {e}"))?
        .context("failed to insert store paths")?;
        Ok(Self { pool })
    }

    /// Run a read-only query on a pooled connection.
    ///
    /// Acquires a connection from the pool and runs `f` on a blocking thread,
    /// flattening the pool/interact/query error layers into a single result.
    async fn query<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self
            .pool
            .get()
            .await
            .context("failed to get Nix database connection")?;
        let value = conn
            .interact(f)
            .await
            .map_err(|e| anyhow::anyhow!("Nix database query failed: {e}"))??;
        Ok(value)
    }

    /// Find a store path by its store-path hash.
    ///
    /// Uses the unique index on `ValidPaths.path` for an O(log n) prefix
    /// lookup instead of scanning the `/nix/store` directory.
    pub async fn find_store_path(&self, hash: &StoreHash) -> Result<Option<PathBuf>> {
        let pattern = format!("/nix/store/{hash}-*");
        let path = self
            .query(move |conn| {
                let mut stmt =
                    conn.prepare_cached("SELECT path FROM ValidPaths WHERE path GLOB ?1 LIMIT 1")?;
                stmt.query_row([&pattern], |row| row.get::<_, String>(0))
                    .optional()
            })
            .await?;
        Ok(path.map(PathBuf::from))
    }

    /// Look up full path information by store-path hash.
    ///
    /// Returns the same [`PathInfo`] that `nix path-info --json` would, but via
    /// a direct, index-backed SQLite query and join. The `hash` column in the
    /// database is `<algo>:<hex>`; it is converted to the SRI form expected by
    /// narinfo consumers. Empty `deriver`/`ca` columns are normalised to `None`
    /// to match the command's `null` output.
    pub async fn find_path_info(&self, hash: &StoreHash) -> Result<Option<PathInfo>> {
        let pattern = format!("/nix/store/{hash}-*");

        let raw = self
            .query(move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT id, path, hash, narSize, sigs, ca, deriver \
                     FROM ValidPaths WHERE path GLOB ?1 LIMIT 1",
                )?;
                let row = stmt
                    .query_row([&pattern], |row| {
                        Ok(RawPathRow {
                            id: row.get(0)?,
                            path: row.get(1)?,
                            hash: row.get(2)?,
                            nar_size: row.get(3)?,
                            sigs: row.get::<_, Option<String>>(4)?,
                            ca: row.get(5)?,
                            deriver: row.get(6)?,
                        })
                    })
                    .optional()?;

                let Some(row) = row else {
                    return Ok(None);
                };

                // References, sorted by store path to match `nix path-info`.
                let mut refs_stmt = conn.prepare_cached(
                    "SELECT vp2.path FROM Refs \
                     JOIN ValidPaths vp2 ON vp2.id = Refs.reference \
                     WHERE Refs.referrer = ?1 \
                     ORDER BY vp2.path",
                )?;
                let references = refs_stmt
                    .query_map([row.id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                Ok(Some((row, references)))
            })
            .await?;

        let Some((row, references)) = raw else {
            return Ok(None);
        };

        let nar_hash = NarHash::from_db_str(&row.hash)
            .with_context(|| format!("invalid NAR hash for {}", row.path))?;
        let signatures = row
            .sigs
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(' ').map(str::parse).collect())
            .unwrap_or_else(|| Ok(Vec::new()))
            .with_context(|| format!("invalid signature for {}", row.path))?;

        Ok(Some(PathInfo {
            path: row.path,
            nar_hash,
            nar_size: row.nar_size as u64,
            references,
            deriver: row.deriver.filter(|s| !s.is_empty()),
            signatures,
            ca: row.ca.filter(|s| !s.is_empty()),
        }))
    }

    /// Return the 32-character store hashes of all serveable paths.
    ///
    /// A path is serveable if it has signatures or is content-addressed. Used
    /// by the store scanner to populate the bloom filter in a single query.
    pub async fn serveable_hashes(&self) -> Result<Vec<StoreHash>> {
        let raw: Vec<String> = self
            .query(|conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT SUBSTR(path, 12, 32) FROM ValidPaths \
                     WHERE (sigs IS NOT NULL AND sigs != '') \
                        OR (ca IS NOT NULL AND ca != '')",
                )?;
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        raw.iter().map(|h| h.parse()).collect()
    }

    /// Whether a store path was built locally rather than substituted from a
    /// binary cache — Nix's `ultimate` flag, read from the `ValidPaths.ultimate`
    /// column (`null` implies false). Errors if the path is not in the store,
    /// matching what `nix path-info` reports for an unknown path.
    pub async fn is_ultimate(&self, store_path: &str) -> Result<bool> {
        let owned = store_path.to_owned();
        let ultimate = self
            .query(move |conn| {
                let mut stmt =
                    conn.prepare_cached("SELECT ultimate FROM ValidPaths WHERE path = ?1")?;
                stmt.query_row([&owned], |row| row.get::<_, Option<i64>>(0))
                    .optional()
            })
            .await?;
        match ultimate {
            Some(flag) => Ok(flag.unwrap_or(0) != 0),
            None => anyhow::bail!("{store_path} is not a valid store path"),
        }
    }

    /// Check whether a store path is serveable (signed or content-addressed).
    pub async fn is_path_serveable(&self, store_path: &str) -> Result<bool> {
        let store_path = store_path.to_owned();
        self.query(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT 1 FROM ValidPaths \
                 WHERE path = ?1 \
                   AND ((sigs IS NOT NULL AND sigs != '') \
                     OR (ca IS NOT NULL AND ca != '')) \
                 LIMIT 1",
            )?;
            Ok(stmt
                .query_row([&store_path], |_| Ok(()))
                .optional()?
                .is_some())
        })
        .await
    }
}

/// Intermediate row from the `ValidPaths` table.
struct RawPathRow {
    id: i64,
    path: String,
    hash: String,
    nar_size: i64,
    sigs: Option<String>,
    ca: Option<String>,
    deriver: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::HashSet;

    use rstest::rstest;
    use rusqlite::Connection;
    use testcontainers::GenericImage;
    use testcontainers::ImageExt;
    use testcontainers::core::CmdWaitFor;
    use testcontainers::core::ExecCommand;
    use testcontainers::core::Mount;
    use testcontainers::core::WaitFor;
    use testcontainers::runners::AsyncRunner;

    use super::*;
    use crate::path_info::parse_path_info_json;

    /// Shell script run inside the container to produce a variety of store
    /// paths covering the cases the SQLite queries must get right:
    ///   - a content-addressed path (no refs, no deriver, no sigs)
    ///   - built derivations with a deriver (Nix marks these `ultimate`)
    ///   - a path with multiple references (insertion order != lexical order)
    ///     plus a self-reference, signed with two keys
    ///   - a substituted path (the base image's `bash`), which is not `ultimate`
    ///
    /// It writes the list of paths, the golden `nix path-info --json`, and a
    /// copy of the database into the bind-mounted `/work` directory.
    const SETUP_SCRIPT: &str = r#"
set -eu
export NIX_CONFIG='substituters =
experimental-features = nix-command'

# A builder shell that definitely exists in this image's store.
SH=$(command -v bash)
# The store path containing the builder shell: substituted into the image's
# store, so Nix leaves its `ultimate` flag unset.
REAL=$(readlink -f "$SH")
PB=/nix/store/$(printf '%s' "${REAL#/nix/store/}" | cut -d/ -f1)

cat > /tmp/leaf.nix <<'NIXEOF'
{ sh, name }:
derivation {
  inherit name;
  system = builtins.currentSystem;
  builder = sh;
  args = [ "-c" "echo $name > $out" ];
}
NIXEOF

cat > /tmp/refs.nix <<'NIXEOF'
{ sh, l1, l2, l3 }:
derivation {
  name = "refs";
  system = builtins.currentSystem;
  builder = sh;
  # Interpolating the leaf paths via builtins.storePath attaches string
  # context, so Nix registers them as inputs; writing them (and $out) into
  # the output then records references to the leaves and a self-reference.
  args = [ "-c" "echo ${builtins.storePath l1} ${builtins.storePath l2} ${builtins.storePath l3} $out > $out" ];
}
NIXEOF

# A: content-addressed fixed path (no refs/deriver/sigs).
echo hello-ca > /tmp/a.txt
PA=$(nix-store --add-fixed sha256 /tmp/a.txt)

# Built leaves. Build "zeta" before "alpha"/"mid" so database insertion order
# (id) differs from lexical path order — this is what catches a missing
# ORDER BY on the references query.
L1=$(nix-build --no-out-link --argstr sh "$SH" --argstr name zeta /tmp/leaf.nix)
L2=$(nix-build --no-out-link --argstr sh "$SH" --argstr name alpha /tmp/leaf.nix)
L3=$(nix-build --no-out-link --argstr sh "$SH" --argstr name mid /tmp/leaf.nix)

# C: references all three leaves and itself; then sign with two keys.
PC=$(nix-build --no-out-link --argstr sh "$SH" \
      --argstr l1 "$L1" --argstr l2 "$L2" --argstr l3 "$L3" /tmp/refs.nix)
nix-store --generate-binary-cache-key test-key-1 /tmp/k1.sec /tmp/k1.pub
nix-store --generate-binary-cache-key test-key-2 /tmp/k2.sec /tmp/k2.pub
nix store sign --key-file /tmp/k1.sec "$PC"
nix store sign --key-file /tmp/k2.sec "$PC"

printf '%s\n' "$PA" "$L1" "$L2" "$L3" "$PC" "$PB" > /work/paths.txt
nix path-info --json $(cat /work/paths.txt) > /work/golden.json
cp /nix/var/nix/db/db.sqlite* /work/
chmod -R a+rw /work
"#;

    /// Spin up a real Nix store, run the equivalence-test setup, and assert the
    /// read-only [`NixDb`] queries agree with `nix path-info --json`:
    ///   - `find_path_info` matches the golden metadata field-for-field
    ///   - `is_path_serveable` matches `PathInfo::is_serveable` per path
    ///   - `is_ultimate` matches the golden `ultimate` flag per path
    ///   - `find_store_path` resolves each hash back to its full store path
    ///   - `serveable_hashes` returns exactly the set of serveable paths
    ///
    /// Parameterised over the supported Nix daemon versions (the `nixos/nix`
    /// image tag). The matrix spans from the version nixos-26.05 defaults to up
    /// to the latest release; add a `#[case]` line as new releases land.
    ///
    /// Requires a Docker daemon.
    #[rstest]
    #[tokio::test]
    #[case::nix_2_34("2.34.7")]
    async fn nix_db_matches_nix_path_info(#[case] image_tag: &str) {
        let work = tempfile::tempdir().unwrap();

        let container = GenericImage::new("nixos/nix", image_tag)
            .with_wait_for(WaitFor::message_on_stdout("container-ready"))
            .with_cmd(["sh", "-c", "echo container-ready && sleep infinity"])
            .with_mount(Mount::bind_mount(
                work.path().to_str().unwrap().to_string(),
                "/work",
            ))
            .start()
            .await
            .expect("start nix container");

        let mut setup = container
            .exec(
                ExecCommand::new(["sh", "-c", SETUP_SCRIPT])
                    .with_cmd_ready_condition(CmdWaitFor::exit()),
            )
            .await
            .expect("exec setup script");
        // Fully drain the exec's output streams before touching the bind
        // mount: closing the process's fds is what makes its file writes
        // reliably visible on the host.
        let exit = setup.exit_code().await.expect("setup exit code");
        let _ = setup.stdout_to_vec().await;
        let setup_stderr =
            String::from_utf8_lossy(&setup.stderr_to_vec().await.unwrap()).to_string();
        if exit != Some(0) {
            panic!("setup script failed (exit {exit:?}):\n{setup_stderr}");
        }

        // Read the artifacts the container wrote into the bind mount.
        let golden_json =
            std::fs::read_to_string(work.path().join("golden.json")).expect("read golden.json");
        let paths: Vec<String> = std::fs::read_to_string(work.path().join("paths.txt"))
            .expect("read paths.txt")
            .lines()
            .map(str::to_string)
            .collect();

        // Fold any WAL into the copied database so we can open it read-only.
        let db_copy = work.path().join("db.sqlite");
        {
            let conn = Connection::open(&db_copy).expect("open db copy");
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
                .expect("checkpoint db copy");
        }

        let golden: HashMap<String, PathInfo> = parse_path_info_json(&golden_json)
            .expect("parse golden")
            .into_iter()
            .map(|info| (info.path.clone(), info))
            .collect();

        // `ultimate` isn't part of `PathInfo` (nothing that consumes a
        // `PathInfo` needs it), so read the flag straight from the golden JSON
        // to check `is_ultimate` against.
        #[derive(serde::Deserialize)]
        struct GoldenUltimate {
            #[serde(default)]
            ultimate: bool,
        }
        let golden_ultimate: HashMap<String, GoldenUltimate> =
            serde_json::from_str(&golden_json).expect("parse golden ultimate flags");

        let db = NixDb::open_path(&db_copy).await.expect("open NixDb");

        assert!(!paths.is_empty(), "no paths produced by setup");

        // The `is_ultimate` check below is only meaningful if the fixture
        // exercises both states: the built paths are ultimate, the substituted
        // bash path is not.
        assert!(
            paths.iter().any(|p| golden_ultimate[p].ultimate),
            "fixture produced no ultimate path",
        );
        assert!(
            paths.iter().any(|p| !golden_ultimate[p].ultimate),
            "fixture produced no non-ultimate path",
        );

        for store_path in &paths {
            // "/nix/store/" is 11 chars; the hash is the next 32.
            let hash: StoreHash = store_path[11..43].parse().expect("valid store hash");
            let expected = golden
                .get(store_path)
                .unwrap_or_else(|| panic!("path missing from golden: {store_path}"));

            let actual = db
                .find_path_info(&hash)
                .await
                .expect("find_path_info query")
                .unwrap_or_else(|| panic!("path missing from NixDb: {store_path}"));
            assert_eq!(&actual, expected, "PathInfo mismatch for {store_path}");

            // Serveability is a separate hand-written predicate; it must agree
            // with the golden `is_serveable`.
            assert_eq!(
                db.is_path_serveable(store_path)
                    .await
                    .expect("is_path_serveable query"),
                expected.is_serveable(),
                "is_path_serveable mismatch for {store_path}",
            );

            // The dedicated `is_ultimate` query must agree with the golden
            // `ultimate` flag.
            assert_eq!(
                db.is_ultimate(store_path).await.expect("is_ultimate query"),
                golden_ultimate[store_path].ultimate,
                "is_ultimate mismatch for {store_path}",
            );

            // The hash prefix must resolve back to the full store path.
            assert_eq!(
                db.find_store_path(&hash)
                    .await
                    .expect("find_store_path query")
                    .as_deref(),
                Some(Path::new(store_path)),
                "find_store_path mismatch for {store_path}",
            );
        }

        // `serveable_hashes` powers the bloom filter. The image's store holds
        // many paths beyond the fixture's, so we can't assert an exact set;
        // instead check that each fixture hash is present iff it is serveable.
        let serveable: HashSet<String> = db
            .serveable_hashes()
            .await
            .expect("serveable_hashes query")
            .into_iter()
            .map(|h| h.as_str().to_owned())
            .collect();
        for store_path in &paths {
            let hash = &store_path[11..43];
            assert_eq!(
                serveable.contains(hash),
                golden[store_path].is_serveable(),
                "serveable_hashes membership mismatch for {store_path}",
            );
        }
    }

    /// `is_ultimate` reads the `ultimate` column, treats a `null` (the common
    /// case for substituted paths) as false, and reports an unknown path as an
    /// error the way `nix path-info` does. Runs against an in-memory database,
    /// so it needs no Docker daemon.
    #[tokio::test]
    async fn is_ultimate_reads_column_treating_null_as_false() {
        let db = NixDb::open_in_memory(&[]).await.expect("open in-memory db");
        db.pool
            .get()
            .await
            .expect("connection")
            .interact(|conn| {
                conn.execute_batch(
                    "INSERT INTO ValidPaths (path, hash, narSize, ultimate) VALUES \
                     ('/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-built', 'sha256:00', 1, 1), \
                     ('/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-subst', 'sha256:00', 1, NULL)",
                )
            })
            .await
            .expect("interact")
            .expect("insert rows");

        assert!(
            db.is_ultimate("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-built")
                .await
                .expect("ultimate query"),
            "path with ultimate=1 should be ultimate",
        );
        assert!(
            !db.is_ultimate("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-subst")
                .await
                .expect("non-ultimate query"),
            "path with ultimate=NULL should not be ultimate",
        );
        assert!(
            db.is_ultimate("/nix/store/cccccccccccccccccccccccccccccccc-missing")
                .await
                .is_err(),
            "unknown path should error",
        );
    }
}
