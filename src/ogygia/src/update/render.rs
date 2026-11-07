//! Presenting what the ogygia-updated daemon reports.
//!
//! The daemon sends data (see `ogygia_updated::control`); everything a
//! person reads is made here.

use chrono::DateTime;
use chrono::Utc;
use ogygia_updated::CanaryHold;
use ogygia_updated::CanaryState;
use ogygia_updated::CanaryTarget;
use ogygia_updated::FinishReason;
use ogygia_updated::Outcome;
use ogygia_updated::control::Reply;

/// What a reply says, for stdout.
pub fn reply(reply: &Reply, now: DateTime<Utc>) -> String {
    match reply {
        Reply::Cycle { outcome } => self::outcome(outcome),
        Reply::CanaryStatus { state: None, .. } => "no canary".to_string(),
        Reply::CanaryStatus {
            state: Some(state),
            running,
            next_boot,
        } => canary(state, now, running, next_boot),
    }
}

/// How an update cycle ended.
pub fn outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::UpToDate => "system is up to date".to_string(),
        Outcome::NotOnBranch { revision } => {
            format!("running revision {revision} is not on the tracked branch; not updating")
        }
        Outcome::Switched { commit } => format!("switched to {commit}"),
        Outcome::TestActivated { commit } => {
            format!("activated {commit} without changing the boot default")
        }
        Outcome::RebootScheduled { commit } => {
            format!("staged {commit} for boot and scheduled a reboot")
        }
        Outcome::PrefetchUnavailable { commit } => format!(
            "cache-warm closure for {commit} is unavailable; skipping until the cache catches up"
        ),
        Outcome::CanaryStarted {
            commit,
            next_boot,
            persist,
            expires_at,
        } => {
            let staged = if *persist {
                format!("trialling {commit}, staged for boot")
            } else {
                format!("trialling {commit}; boot floor {next_boot}")
            };
            match expires_at {
                Some(at) => format!("{staged}; expires {}", timestamp(*at)),
                None => format!("{staged}; no timeout"),
            }
        }
        Outcome::CanaryHeld { commit, next_boot } => {
            format!("holding canary {commit}; boot floor {next_boot}")
        }
    }
}

/// A status line for a canary. `running` and `next_boot` are the commits
/// the host currently runs and will next boot; they are only shown for an
/// in-progress trial.
fn canary(state: &CanaryState, now: DateTime<Utc>, running: &str, next_boot: &str) -> String {
    match state {
        CanaryState::Active {
            target: CanaryTarget::Pinned { branch, commit },
            expires_at,
            hold,
        } => {
            let expiry = match expires_at {
                Some(at) => format!(
                    "expires {} (in {}h)",
                    timestamp(*at),
                    (*at - now).num_hours()
                ),
                None => "no timeout".to_string(),
            };
            // An ephemeral trial's next-boot commit is the floor it falls
            // back to; a persistent one's is the trial itself.
            let boot = match hold {
                CanaryHold::Ephemeral { .. } => format!("boot floor {next_boot}"),
                CanaryHold::Persistent => {
                    format!("boot default {next_boot}, kept across reboot")
                }
            };
            format!("trialling {branch} @{commit}; running {running}, {boot}; {expiry}")
        }
        CanaryState::Finished {
            target: CanaryTarget::Pinned { branch, commit },
            at,
            reason,
        } => {
            let reason = match reason {
                FinishReason::Merged => "merged",
                FinishReason::Expired => "expired",
                FinishReason::Cleared => "cleared",
                FinishReason::Rebooted => "rebooted",
                FinishReason::Overwritten => "overwritten",
            };
            format!(
                "no active canary; last: {branch} @{commit} finished {} ({reason})",
                timestamp(*at),
            )
        }
    }
}

fn timestamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%MZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canary_status_covers_active_and_finished() {
        let now: DateTime<Utc> = "2026-07-17T00:00:00Z".parse().unwrap();
        let target = CanaryTarget::Pinned {
            branch: "feature".to_string(),
            commit: "abc".to_string(),
        };
        let ephemeral = CanaryHold::Ephemeral {
            boot_id: "boot-1".to_string(),
        };

        let active = CanaryState::Active {
            target: target.clone(),
            expires_at: Some("2026-07-18T00:00:00Z".parse().unwrap()),
            hold: ephemeral.clone(),
        };
        assert_eq!(
            canary(&active, now, "def", "ghi"),
            "trialling feature @abc; running def, boot floor ghi; expires 2026-07-18 00:00Z (in 24h)"
        );

        let forever = CanaryState::Active {
            target: target.clone(),
            expires_at: None,
            hold: ephemeral,
        };
        assert_eq!(
            canary(&forever, now, "def", "ghi"),
            "trialling feature @abc; running def, boot floor ghi; no timeout"
        );

        // A persistent trial is the next-boot system, not a floor under it.
        let persistent = CanaryState::Active {
            target: target.clone(),
            expires_at: None,
            hold: CanaryHold::Persistent,
        };
        assert_eq!(
            canary(&persistent, now, "abc", "abc"),
            "trialling feature @abc; running abc, boot default abc, kept across reboot; no timeout"
        );

        let finished = CanaryState::Finished {
            target,
            at: now,
            reason: FinishReason::Merged,
        };
        assert_eq!(
            canary(&finished, now, "def", "ghi"),
            "no active canary; last: feature @abc finished 2026-07-17 00:00Z (merged)"
        );
    }

    #[test]
    fn outcomes_read_as_sentences() {
        let at: DateTime<Utc> = "2026-07-18T04:00:00Z".parse().unwrap();
        assert_eq!(
            outcome(&Outcome::CanaryStarted {
                commit: "abc".to_string(),
                next_boot: "def".to_string(),
                persist: false,
                expires_at: Some(at),
            }),
            "trialling abc; boot floor def; expires 2026-07-18 04:00Z"
        );
        assert_eq!(
            outcome(&Outcome::CanaryStarted {
                commit: "abc".to_string(),
                next_boot: "abc".to_string(),
                persist: true,
                expires_at: None,
            }),
            "trialling abc, staged for boot; no timeout"
        );
        assert_eq!(
            outcome(&Outcome::Switched {
                commit: "abc".to_string()
            }),
            "switched to abc"
        );
    }
}
