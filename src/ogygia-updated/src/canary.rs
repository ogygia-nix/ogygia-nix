//! Persisted state for a canary: a trial that holds the host on a
//! chosen build instead of the tracked branch tip.
//!
//! The daemon exits and is restarted by systemd after every activation,
//! so this outlives the process and is the sole record of an in-flight
//! trial. The file is absent until the first canary runs; afterwards it
//! always describes the current or most-recent one, so `canary status`
//! can explain how the last trial ended.

use std::fs;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

/// Which build a canary holds the host on. One variant today; a
/// branch-following mode slots in later without touching the lifecycle
/// or the engine's floor, expiry, and finish handling, which are all
/// target-agnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CanaryTarget {
    /// A commit resolved when the canary was issued. `branch` is kept for
    /// provenance and display only — the host does not track it.
    Pinned { branch: String, commit: String },
    // Following { branch: String },  // future: re-resolve the tip each cycle
}

/// How a trial is held on the host, and so what a reboot does to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CanaryHold {
    /// An ephemeral test activation, with a safe floor staged for boot. A
    /// reboot loses the activation and ends the trial, which is why the
    /// boot id it started in is recorded.
    Ephemeral { boot_id: String },
    /// The trial is the boot default, so it survives a reboot — the only
    /// way to exercise a new kernel or new boot flags. Nothing is staged
    /// to fall back to, so there is no boot id worth comparing.
    Persistent,
}

/// Lifecycle of the current or most-recent canary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CanaryState {
    /// A trial in progress.
    Active {
        target: CanaryTarget,
        /// Absolute deadline; `None` for a canary with no timeout.
        expires_at: Option<DateTime<Utc>>,
        hold: CanaryHold,
    },
    /// A trial that ended, retained so `canary status` can explain how.
    Finished {
        target: CanaryTarget,
        at: DateTime<Utc>,
        reason: FinishReason,
    },
}

/// Why a canary stopped holding the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The pinned commit became an ancestor of the tracked tip.
    Merged,
    /// The deadline passed.
    Expired,
    /// A bare `ogygia update` superseded it.
    Cleared,
    /// The host rebooted, losing the trial's ephemeral test activation.
    Rebooted,
    /// The running system was replaced out of band — a manual switch, with
    /// no reboot.
    Overwritten,
}

impl CanaryState {
    /// Load the record, or `None` if no canary has ever run.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading canary state {}", path.display()));
            }
        };
        let state = serde_json::from_str(&raw)
            .with_context(|| format!("parsing canary state {}", path.display()))?;
        Ok(Some(state))
    }

    /// Persist the record, replacing any previous one atomically.
    pub fn store(&self, path: &Path) -> Result<()> {
        let payload = serde_json::to_vec_pretty(self).context("encoding canary state")?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &payload)
            .with_context(|| format!("writing canary state {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("replacing canary state {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_no_canary() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("canary.json");
        assert!(CanaryState::load(&path).unwrap().is_none());
    }

    /// State written before `hold` existed carries a bare `boot_id`, which
    /// no longer parses. The engine turns that into "no active trial" so a
    /// host is not wedged by it; this pins the loud half of that contract.
    #[test]
    fn state_written_before_hold_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("canary.json");
        fs::write(
            &path,
            r#"{"state":"active","target":{"mode":"pinned","branch":"feature","commit":"abc"},"expires_at":null,"boot_id":"boot-1"}"#,
        )
        .unwrap();

        assert!(CanaryState::load(&path).is_err());
    }

    #[test]
    fn active_and_finished_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("canary.json");
        let target = CanaryTarget::Pinned {
            branch: "feature".to_string(),
            commit: "a".repeat(40),
        };

        let active = CanaryState::Active {
            target: target.clone(),
            expires_at: Some("2026-07-18T12:00:00Z".parse().unwrap()),
            hold: CanaryHold::Ephemeral {
                boot_id: "boot-1".to_string(),
            },
        };
        active.store(&path).unwrap();
        assert!(matches!(
            CanaryState::load(&path).unwrap(),
            Some(CanaryState::Active {
                expires_at: Some(_),
                ..
            })
        ));

        // Storing again replaces the record rather than appending.
        let finished = CanaryState::Finished {
            target,
            at: "2026-07-18T12:00:00Z".parse().unwrap(),
            reason: FinishReason::Merged,
        };
        finished.store(&path).unwrap();
        assert!(matches!(
            CanaryState::load(&path).unwrap(),
            Some(CanaryState::Finished {
                reason: FinishReason::Merged,
                ..
            })
        ));
    }
}
