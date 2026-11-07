use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use git2::Oid;
use serde::Deserialize;
use serde::Serialize;
use tracing::info;
use tracing::warn;

use crate::canary::CanaryHold;
use crate::canary::CanaryState;
use crate::canary::CanaryTarget;
use crate::canary::FinishReason;
use crate::config::Config;
use crate::control::Events;
use crate::control::Step;
use crate::repo::Repo;
use crate::system::Activation;
use crate::system::RealSystem;
use crate::system::System;

/// Result of one update cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// The running and next-boot systems already match the branch tip.
    UpToDate,
    /// The running system was not built from the tracked branch, so it
    /// was left alone.
    NotOnBranch { revision: String },
    /// The new system was activated and made the boot default.
    Switched { commit: String },
    /// The next-boot system was not built from the tracked branch, so
    /// the new system was activated without touching the boot profile.
    TestActivated { commit: String },
    /// The new system needs a new kernel; it is the boot default and a
    /// reboot has been scheduled.
    RebootScheduled { commit: String },
    /// The cache-warm closure could not be substituted, so the cycle was
    /// skipped rather than built locally. Retried on the next cycle.
    PrefetchUnavailable { commit: String },
    /// A canary was started: the commit runs, and `next_boot` is what a
    /// reboot lands on — a safe floor, or the trial itself when it persists.
    CanaryStarted {
        commit: String,
        next_boot: String,
        persist: bool,
        expires_at: Option<DateTime<Utc>>,
    },
    /// A scheduled cycle held the running trial in place, keeping the boot
    /// floor current without touching the running system.
    CanaryHeld { commit: String, next_boot: String },
}

impl Outcome {
    /// Whether the running system was replaced, so the daemon must exit
    /// for systemd to restart it under the new unit. A bare boot-default
    /// change (reboot scheduled, canary floor staged) leaves the running
    /// system untouched and does not qualify; only starting a canary
    /// re-drives the running system among the canary outcomes.
    pub fn activated(&self) -> bool {
        matches!(
            self,
            Outcome::Switched { .. }
                | Outcome::TestActivated { .. }
                | Outcome::CanaryStarted { .. }
        )
    }

    /// Whether the cycle did what the caller asked of it. An up-to-date
    /// host, a held trial, and a deliberately off-branch system are all
    /// settled states and count as served; a cycle that wanted to advance
    /// and could not does not, leaving the host behind the branch it
    /// tracks.
    pub fn served(&self) -> bool {
        !matches!(self, Outcome::PrefetchUnavailable { .. })
    }
}

/// What initiated an update cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// The daemon's own interval fired. A host deliberately running or
    /// booting an off-branch build is left alone, and an active canary is
    /// held, expired, or retired once its commit merges.
    Scheduled,
    /// An operator asked for a plain update. Any canary is cleared and the
    /// host is pulled back onto the branch tip regardless of what it runs.
    Manual,
    /// An operator asked to trial `branch`. Its tip is pinned now and the
    /// host is held there until the timeout elapses, the commit merges, or
    /// a manual update supersedes it. `persist` makes the trial the boot
    /// default so it also survives a reboot.
    StartCanary {
        branch: String,
        timeout: Option<Duration>,
        persist: bool,
    },
}

/// Run one update cycle, reporting its progress to `events`.
pub fn run_once(config: &Config, trigger: Trigger, events: &Events) -> Result<Outcome> {
    let system = RealSystem { events };
    run(config, &system, Utc::now(), &read_boot_id()?, trigger)
}

fn run(
    config: &Config,
    system: &impl System,
    now: DateTime<Utc>,
    boot_id: &str,
    trigger: Trigger,
) -> Result<Outcome> {
    system.step(Step::Fetch);
    let repo = Repo::open_or_clone(&config.repo_path(), &config.repo.url)?;
    repo.fetch()?;
    let main_tip = repo.tip(&config.repo.branch)?;
    // Unreadable state must not wedge the host: failing the cycle would
    // stall every future one too, so an unparseable record is reported and
    // treated as no trial, letting the guarded tracking path resume.
    let canary = CanaryState::load(&config.canary_state_path()).unwrap_or_else(|error| {
        warn!(%error, "unreadable canary state; treating it as no active trial");
        None
    });
    info!(%main_tip, branch = %config.repo.branch, ?trigger, "fetched");

    match trigger {
        Trigger::StartCanary {
            branch,
            timeout,
            persist,
        } => {
            let expires_at = timeout.map(|d| now + chrono::Duration::seconds(d.as_secs() as i64));
            let hold = if persist {
                CanaryHold::Persistent
            } else {
                CanaryHold::Ephemeral {
                    boot_id: boot_id.to_string(),
                }
            };
            start_canary(config, system, &repo, main_tip, &branch, expires_at, hold)
        }
        Trigger::Manual => {
            if let Some(CanaryState::Active { target, .. }) = &canary {
                finish_canary(config, now, target, FinishReason::Cleared)?;
                info!("cleared active canary for a manual update");
            }
            update_to_tip(config, system, &repo, main_tip, true)
        }
        Trigger::Scheduled => scheduled(config, system, &repo, now, boot_id, main_tip, canary),
    }
}

/// The kernel's boot id, which changes on every boot.
fn read_boot_id() -> Result<String> {
    let path = "/proc/sys/kernel/random/boot_id";
    Ok(fs::read_to_string(path)
        .with_context(|| format!("reading {path}"))?
        .trim()
        .to_string())
}

/// A scheduled cycle: follow the tracked branch, or end an active canary
/// on the first terminal condition in precedence order — reboot, then
/// out-of-band switch, then merge, then timeout — so a host that left the
/// trial records why, even if the commit also merged or the deadline
/// lapsed while it was gone. Every ending resumes normal tracking.
fn scheduled(
    config: &Config,
    system: &impl System,
    repo: &Repo,
    now: DateTime<Utc>,
    boot_id: &str,
    main_tip: Oid,
    canary: Option<CanaryState>,
) -> Result<Outcome> {
    let Some(CanaryState::Active {
        target,
        expires_at,
        hold,
    }) = canary
    else {
        return update_to_tip(config, system, repo, main_tip, false);
    };

    let commit = target_commit(&target)?;

    // A reboot loses an ephemeral test activation, so that trial is over.
    // This outranks merge and expiry, which may both have come true while
    // the host was down. The trial is never reapplied; we record the reason
    // and resume normal tracking this cycle, rolling forward from the floor.
    // A persistent trial is the boot default and comes back up on it, so a
    // reboot is the point rather than the end.
    if let CanaryHold::Ephemeral { boot_id: started } = &hold
        && boot_id != started
    {
        finish_canary(config, now, &target, FinishReason::Rebooted)?;
        info!("canary host rebooted; ending the trial and resuming the tracked branch");
        return update_to_tip(config, system, repo, main_tip, false);
    }

    // No reboot, but the host is no longer running the trial: it was
    // switched out of band by hand. Resume tracking, which leaves a
    // deliberately off-branch system alone.
    let current = read_revision(&config.host.current_revision_path)?;
    if current != commit {
        finish_canary(config, now, &target, FinishReason::Overwritten)?;
        info!("canary overwritten out of band; ending the trial and resuming the tracked branch");
        return update_to_tip(config, system, repo, main_tip, false);
    }

    // Still trialling. Once the commit lands on the tracked branch the
    // canary has served its purpose; resume following the tip. The trial's
    // own commit is often rewritten before it lands — reworded, or amended
    // after review — so under jj its changes reaching the branch under new
    // hashes ends the trial just as an untouched merge does.
    if on_branch(repo, commit, main_tip)? {
        finish_canary(config, now, &target, FinishReason::Merged)?;
        info!("canary merged; resuming the tracked branch");
        return update_to_tip(config, system, repo, main_tip, false);
    }

    if expires_at.is_some_and(|deadline| now >= deadline) {
        finish_canary(config, now, &target, FinishReason::Expired)?;
        info!("canary timed out; reverting to the tracked branch");
        return update_to_tip(config, system, repo, main_tip, true);
    }

    // Hold the trial open without re-driving the running system: keep an
    // ephemeral trial's floor recent, while a persistent trial already owns
    // the boot default and needs nothing staged under it.
    let next_boot = match hold {
        CanaryHold::Ephemeral { .. } => stage_boot_floor(config, system, repo, commit, main_tip)?,
        CanaryHold::Persistent => commit,
    };
    Ok(Outcome::CanaryHeld {
        commit: commit.to_string(),
        next_boot: next_boot.to_string(),
    })
}

/// Pin `branch`'s tip and hold the host on it. A `Persistent` hold trades
/// the boot floor for the trial itself as the boot default, so it survives
/// a reboot.
fn start_canary(
    config: &Config,
    system: &impl System,
    repo: &Repo,
    main_tip: Oid,
    branch: &str,
    expires_at: Option<DateTime<Utc>>,
    hold: CanaryHold,
) -> Result<Outcome> {
    let commit = repo.tip(branch)?;
    let persist = matches!(hold, CanaryHold::Persistent);
    CanaryState::Active {
        target: CanaryTarget::Pinned {
            branch: branch.to_string(),
            commit: commit.to_string(),
        },
        expires_at,
        hold,
    }
    .store(&config.canary_state_path())?;

    let current = read_revision(&config.host.current_revision_path)?;

    // A persistent trial owns the boot default, so it is applied like a
    // normal update: profile moved and switched. That deliberately leaves
    // no floor to fall back to — the operator reboots when ready to
    // exercise the trial's kernel and recovers from the bootloader menu.
    if persist {
        let next_boot = read_revision(&config.next_boot_revision_path())?;
        if current != commit || next_boot != commit {
            let store_path = checkout_and_build(config, system, repo, commit)?;
            system.set_profile(&config.activate.profile, &store_path)?;
            system.switch_to_configuration(&store_path, Activation::Switch)?;
        }
        return Ok(Outcome::CanaryStarted {
            commit: commit.to_string(),
            next_boot: commit.to_string(),
            persist,
            expires_at,
        });
    }

    let floor = stage_boot_floor(config, system, repo, commit, main_tip)?;

    // Apply the trial once, as an ephemeral test activation. Scheduled
    // cycles never reapply it, so a reboot ends it for good.
    if current != commit {
        let store_path = checkout_and_build(config, system, repo, commit)?;
        system.switch_to_configuration(&store_path, Activation::Test)?;
    }

    Ok(Outcome::CanaryStarted {
        commit: commit.to_string(),
        next_boot: floor.to_string(),
        persist,
        expires_at,
    })
}

/// Stage `merge-base(commit, main_tip)` as the boot default so a reboot
/// lands on a recent tracked commit rather than the trial. Boot only: it
/// never touches the running system and never schedules a reboot. Returns
/// the commit a reboot now lands on.
///
/// A newly-added host has no entry on the tracked branch yet, so the
/// branch point predates it and cannot build its system. There is no
/// tracked floor to fall back to, so the existing boot default — the
/// system the host is already running — is left in place and reported
/// instead, letting a canary start without `--persist` on a fresh host.
fn stage_boot_floor(
    config: &Config,
    system: &impl System,
    repo: &Repo,
    commit: Oid,
    main_tip: Oid,
) -> Result<Oid> {
    let floor = repo.merge_base(commit, main_tip)?;
    let next_boot = read_revision(&config.next_boot_revision_path())?;

    repo.checkout(floor)?;
    if !system.attr_defined(&flake_ref(config))? {
        info!(%floor, attr = %config.flake_attr(), "build attribute undefined at boot floor; keeping the current boot default");
        return Ok(next_boot);
    }

    if next_boot != floor {
        let floor_path = checkout_and_build(config, system, repo, floor)?;
        system.set_profile(&config.activate.profile, &floor_path)?;
        system.switch_to_configuration(&floor_path, Activation::Boot)?;
    }

    Ok(floor)
}

/// Record a canary as finished so `canary status` can explain how it ended.
fn finish_canary(
    config: &Config,
    now: DateTime<Utc>,
    target: &CanaryTarget,
    reason: FinishReason,
) -> Result<()> {
    CanaryState::Finished {
        target: target.clone(),
        at: now,
        reason,
    }
    .store(&config.canary_state_path())
}

/// Update the host to `tip`. `force` overrides the scheduler's guards
/// that otherwise leave a deliberately off-branch running or next-boot
/// system alone.
fn update_to_tip(
    config: &Config,
    system: &impl System,
    repo: &Repo,
    tip: Oid,
    force: bool,
) -> Result<Outcome> {
    let current = read_revision(&config.host.current_revision_path)?;
    let next_boot = read_revision(&config.next_boot_revision_path())?;

    if !force && !on_branch(repo, current, tip)? {
        return Ok(Outcome::NotOnBranch {
            revision: current.to_string(),
        });
    }

    if current == tip && next_boot == tip {
        return Ok(Outcome::UpToDate);
    }

    repo.checkout(tip)?;

    let flake = format!("git+file://{}", config.repo_path().display());
    // The prefetch substitutes a cache-resident edition of the closure so
    // the real build only makes the commit-specific remainder. If it
    // cannot be substituted the cache is not yet ready, and a host that
    // relies on it must not fall back to a full local build; skip the
    // cycle and retry once the cache catches up.
    if let Some(prefetch_attr) = &config.build.prefetch_attr
        && let Err(error) = system.prefetch(&format!("{flake}#{prefetch_attr}"))
    {
        warn!(%error, "cache-warm closure unavailable; skipping the update");
        return Ok(Outcome::PrefetchUnavailable {
            commit: tip.to_string(),
        });
    }
    let store_path = system.build(&format!("{flake}#{}", config.flake_attr()))?;
    info!(store_path = %store_path.display(), "built new system");

    // A hand-staged off-branch next-boot system is preserved by the
    // scheduler, but a manual trigger resets the boot default to the tip.
    let next_boot_on_branch = force || on_branch(repo, next_boot, tip)?;
    activate(config, system, &store_path, tip, next_boot_on_branch)
}

/// The flake reference this host's system is built from.
fn flake_ref(config: &Config) -> String {
    format!(
        "git+file://{}#{}",
        config.repo_path().display(),
        config.flake_attr()
    )
}

/// Check out `commit`, leaving a clean tree, and build this host's system.
fn checkout_and_build(
    config: &Config,
    system: &impl System,
    repo: &Repo,
    commit: Oid,
) -> Result<PathBuf> {
    repo.checkout(commit)?;
    let store_path = system.build(&flake_ref(config))?;
    info!(store_path = %store_path.display(), %commit, "built system");
    Ok(store_path)
}

/// Parse the run commit from a canary target.
fn target_commit(target: &CanaryTarget) -> Result<Oid> {
    match target {
        CanaryTarget::Pinned { commit, .. } => {
            Oid::from_str(commit).with_context(|| format!("parsing pinned canary commit {commit}"))
        }
    }
}

/// Whether `revision` has reached the branch leading to `tip`, either as a
/// plain ancestor or as a deployed jj revision whose every change has since
/// landed there (rewritten, keeping its change-id). Such a fully-landed
/// revision counts as merged; a stack with any change still outstanding
/// does not.
fn on_branch(repo: &Repo, revision: Oid, tip: Oid) -> Result<bool> {
    if repo.is_ancestor(revision, tip)? {
        return Ok(true);
    }
    if repo.changes_landed(revision, tip)? {
        info!(%revision, "deployed changes have all landed on the tracked branch");
        return Ok(true);
    }
    Ok(false)
}

fn activate(
    config: &Config,
    system: &impl System,
    store_path: &Path,
    tip: Oid,
    next_boot_on_branch: bool,
) -> Result<Outcome> {
    let commit = tip.to_string();

    // A next-boot system off the tracked branch was deliberately staged
    // by hand; activate the update without clobbering it.
    if !next_boot_on_branch {
        system.switch_to_configuration(store_path, Activation::Test)?;
        return Ok(Outcome::TestActivated { commit });
    }

    if !config.activate.allow_reboot {
        system.set_profile(&config.activate.profile, store_path)?;
        system.switch_to_configuration(store_path, Activation::Switch)?;
        return Ok(Outcome::Switched { commit });
    }

    system.set_profile(&config.activate.profile, store_path)?;
    system.switch_to_configuration(store_path, Activation::Boot)?;
    if system.kernel_links(&config.activate.booted_system) == system.kernel_links(store_path) {
        system.switch_to_configuration(store_path, Activation::Test)?;
        Ok(Outcome::Switched { commit })
    } else {
        system.schedule_reboot(config.activate.reboot_delay_minutes)?;
        Ok(Outcome::RebootScheduled { commit })
    }
}

fn read_revision(path: &Path) -> Result<Oid> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("reading build revision {}", path.display()))?;
    Oid::from_str(raw.trim()).with_context(|| {
        format!(
            "parsing build revision {:?} from {}",
            raw.trim(),
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::canary::FinishReason;
    use crate::config::ActivateConfig;
    use crate::config::BuildConfig;
    use crate::config::DaemonConfig;
    use crate::config::HostConfig;
    use crate::config::RepoConfig;
    use crate::jjtest::Jj;
    use crate::repo::testutil::commit;
    use crate::repo::testutil::commit_with_change_id;
    use crate::repo::testutil::init_origin;

    /// A fixed clock for deterministic expiry tests.
    fn now() -> DateTime<Utc> {
        "2026-07-17T00:00:00Z".parse().unwrap()
    }

    /// The boot id a canary is started in; a cycle passing a different one
    /// looks like a reboot.
    const BOOT: &str = "boot-session-a";

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Prefetch(String),
        Build(String),
        SetProfile(PathBuf),
        SwitchToConfiguration(Activation),
        ScheduleReboot(u32),
    }

    #[derive(Default)]
    struct MockSystem {
        calls: RefCell<Vec<Call>>,
        booted_kernel: Option<&'static str>,
        built_kernel: Option<&'static str>,
        fail_prefetch: bool,
        /// Whether `attr_defined` reports the build attribute as present;
        /// `None` defaults to present, standing in for an attribute on the
        /// tracked branch. Set to `Some(false)` to model a freshly-added
        /// host.
        attr_defined: Option<bool>,
        /// Phases the engine announced, in order.
        steps: RefCell<Vec<Step>>,
    }

    impl System for MockSystem {
        fn step(&self, step: Step) {
            self.steps.borrow_mut().push(step);
        }

        fn prefetch(&self, flake_ref: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(Call::Prefetch(flake_ref.to_string()));
            if self.fail_prefetch {
                anyhow::bail!("prefetch unavailable");
            }
            Ok(())
        }

        fn attr_defined(&self, _flake_ref: &str) -> Result<bool> {
            Ok(self.attr_defined.unwrap_or(true))
        }

        fn build(&self, flake_ref: &str) -> Result<PathBuf> {
            self.calls
                .borrow_mut()
                .push(Call::Build(flake_ref.to_string()));
            Ok(PathBuf::from("/nix/store/new-system"))
        }

        fn set_profile(&self, profile: &Path, _store_path: &Path) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(Call::SetProfile(profile.to_path_buf()));
            Ok(())
        }

        fn switch_to_configuration(&self, _store_path: &Path, action: Activation) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(Call::SwitchToConfiguration(action));
            Ok(())
        }

        fn kernel_links(&self, system: &Path) -> [Option<PathBuf>; 3] {
            let kernel = if system == Path::new("/run/booted-system") {
                self.booted_kernel
            } else {
                self.built_kernel
            };
            [kernel.map(PathBuf::from), None, None]
        }

        fn schedule_reboot(&self, delay_minutes: u32) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(Call::ScheduleReboot(delay_minutes));
            Ok(())
        }
    }

    /// The host-side state a cycle reads and writes: the daemon's config
    /// plus the revision files standing in for the running and next-boot
    /// systems. The origin it tracks is built separately, by git2 or by jj.
    struct Fixture {
        // Held so the temporary directory outlives the test.
        _dir: TempDir,
        config: Config,
    }

    impl Fixture {
        fn new() -> (Self, git2::Repository, Oid) {
            let dir = TempDir::new().unwrap();
            let origin_path = dir.path().join("origin");
            let (origin, initial) = init_origin(&origin_path);
            let fixture = Self::tracking(dir, &origin_path);
            (fixture, origin, initial)
        }

        /// A fixture tracking an origin built by the real `jj` binary, so
        /// the commits the daemon sees carry the change-ids jj writes rather
        /// than a hand-rolled imitation of them.
        fn jj() -> (Self, Jj) {
            let dir = TempDir::new().unwrap();
            let jj = Jj::init(dir.path());
            let fixture = Self::tracking(dir, jj.origin());
            (fixture, jj)
        }

        fn tracking(dir: TempDir, origin_path: &Path) -> Self {
            let profile = dir.path().join("profile");
            fs::create_dir_all(profile.join("sw/share/ogygia")).unwrap();

            let config = Config {
                state_dir: dir.path().to_path_buf(),
                repo: RepoConfig {
                    url: origin_path.to_str().unwrap().to_string(),
                    branch: "main".to_string(),
                },
                host: HostConfig {
                    name: "host.example.com".to_string(),
                    current_revision_path: dir.path().join("current-revision"),
                },
                build: BuildConfig::default(),
                activate: ActivateConfig {
                    profile,
                    ..ActivateConfig::default()
                },
                daemon: DaemonConfig::default(),
            };

            Self { _dir: dir, config }
        }

        fn set_current(&self, revision: Oid) {
            fs::write(
                &self.config.host.current_revision_path,
                format!("{revision}\n"),
            )
            .unwrap();
        }

        fn set_next_boot(&self, revision: Oid) {
            fs::write(
                self.config.next_boot_revision_path(),
                format!("{revision}\n"),
            )
            .unwrap();
        }
    }

    #[test]
    fn up_to_date_makes_no_changes() {
        let (fixture, _origin, initial) = Fixture::new();
        fixture.set_current(initial);
        fixture.set_next_boot(initial);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(outcome, Outcome::UpToDate);
        assert!(system.calls.borrow().is_empty());
    }

    #[test]
    fn the_cycle_announces_fetching_first() {
        let (fixture, _origin, initial) = Fixture::new();
        fixture.set_current(initial);
        fixture.set_next_boot(initial);

        let system = MockSystem::default();
        run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(*system.steps.borrow(), vec![Step::Fetch]);
    }

    #[test]
    fn update_switches_to_the_new_commit() {
        let (fixture, origin, initial) = Fixture::new();
        fixture.set_current(initial);
        fixture.set_next_boot(initial);
        let second = commit(&origin, "main", &[initial], "second");

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
        let flake_ref = format!(
            "git+file://{}#nixosConfigurations.\"host.example.com\".config.system.build.toplevel",
            fixture.config.repo_path().display()
        );
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(flake_ref),
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Switch),
            ]
        );

        // The working tree was left checked out at the new commit.
        assert_eq!(
            fs::read_to_string(fixture.config.repo_path().join("marker")).unwrap(),
            "second"
        );
    }

    #[test]
    fn off_branch_current_revision_blocks_the_update() {
        let (fixture, origin, initial) = Fixture::new();
        let side = commit(&origin, "side", &[initial], "side");
        commit(&origin, "main", &[initial], "second");
        fixture.set_current(side);
        fixture.set_next_boot(initial);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::NotOnBranch {
                revision: side.to_string()
            }
        );
        assert!(system.calls.borrow().is_empty());
    }

    #[test]
    fn off_branch_next_boot_gets_test_activation_only() {
        let (fixture, origin, initial) = Fixture::new();
        let side = commit(&origin, "side", &[initial], "side");
        let second = commit(&origin, "main", &[initial], "second");
        fixture.set_current(initial);
        fixture.set_next_boot(side);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::TestActivated {
                commit: second.to_string()
            }
        );
        let calls = system.calls.borrow();
        assert_eq!(
            *calls,
            vec![
                Call::Build(format!(
                    "git+file://{}#nixosConfigurations.\"host.example.com\".config.system.build.toplevel",
                    fixture.config.repo_path().display()
                )),
                Call::SwitchToConfiguration(Activation::Test),
            ]
        );
    }

    #[test]
    fn landed_revision_updates_like_a_merge() {
        let (fixture, origin, initial) = Fixture::new();
        // The host runs a jj commit deployed from a branch; it has since
        // landed on main rewritten, keeping its change-id.
        let deployed =
            commit_with_change_id(&origin, "deploy", &[initial], "deployed", "kxyzkxyzkxyz");
        let landed = commit_with_change_id(&origin, "main", &[initial], "landed", "kxyzkxyzkxyz");
        fixture.set_current(deployed);
        fixture.set_next_boot(deployed);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: landed.to_string()
            }
        );
        // The landed next-boot revision counts as on-branch, so the boot
        // profile moves too rather than getting a test-only activation.
        assert_eq!(
            system.calls.borrow()[1..],
            [
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Switch),
            ]
        );
    }

    #[test]
    fn manual_recovers_an_off_branch_running_system() {
        let (fixture, origin, initial) = Fixture::new();
        let side = commit(&origin, "side", &[initial], "side");
        let second = commit(&origin, "main", &[initial], "second");
        fixture.set_current(side);
        fixture.set_next_boot(initial);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Manual).unwrap();

        // The off-branch guard is overridden and the host is switched onto
        // the branch tip.
        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(format!(
                    "git+file://{}#nixosConfigurations.\"host.example.com\".config.system.build.toplevel",
                    fixture.config.repo_path().display()
                )),
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Switch),
            ]
        );
    }

    #[test]
    fn manual_resets_an_off_branch_next_boot() {
        let (fixture, origin, initial) = Fixture::new();
        let side = commit(&origin, "side", &[initial], "side");
        let second = commit(&origin, "main", &[initial], "second");
        fixture.set_current(initial);
        fixture.set_next_boot(side);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Manual).unwrap();

        // Rather than test-activating around the hand-staged boot entry,
        // the manual trigger resets the boot default to the tip.
        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(format!(
                    "git+file://{}#nixosConfigurations.\"host.example.com\".config.system.build.toplevel",
                    fixture.config.repo_path().display()
                )),
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Switch),
            ]
        );
    }

    #[test]
    fn prefetch_precedes_the_build() {
        let (fixture, origin, initial) = Fixture::new();
        fixture.set_current(initial);
        fixture.set_next_boot(initial);
        let second = commit(&origin, "main", &[initial], "second");
        let mut config = fixture.config;
        config.build.prefetch_attr = Some("checks.x86_64-linux.prefetch".to_string());

        let system = MockSystem::default();
        let outcome = run(&config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
        let flake = format!("git+file://{}", config.repo_path().display());
        assert_eq!(
            system.calls.borrow()[..2],
            [
                Call::Prefetch(format!("{flake}#checks.x86_64-linux.prefetch")),
                Call::Build(format!(
                    "{flake}#nixosConfigurations.\"host.example.com\".config.system.build.toplevel"
                )),
            ]
        );
    }

    #[test]
    fn failed_prefetch_skips_the_build() {
        let (fixture, origin, initial) = Fixture::new();
        fixture.set_current(initial);
        fixture.set_next_boot(initial);
        let second = commit(&origin, "main", &[initial], "second");
        let mut config = fixture.config;
        config.build.prefetch_attr = Some("checks.x86_64-linux.prefetch".to_string());

        let system = MockSystem {
            fail_prefetch: true,
            ..MockSystem::default()
        };
        let outcome = run(&config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::PrefetchUnavailable {
                commit: second.to_string()
            }
        );
        // The host is left behind the branch, so an explicit request for a
        // cycle is answered as a failure rather than a skip.
        assert!(!outcome.served());
        // A cache-dependent host never falls back to a local build.
        assert_eq!(
            *system.calls.borrow(),
            [Call::Prefetch(format!(
                "git+file://{}#checks.x86_64-linux.prefetch",
                config.repo_path().display()
            ))]
        );
    }

    #[test]
    fn unchanged_kernel_switches_without_reboot() {
        let (fixture, origin, initial) = Fixture::new();
        fixture.set_current(initial);
        fixture.set_next_boot(initial);
        let second = commit(&origin, "main", &[initial], "second");
        let mut config = fixture.config;
        config.activate.allow_reboot = true;
        config.activate.booted_system = PathBuf::from("/run/booted-system");

        let system = MockSystem {
            booted_kernel: Some("kernel-1"),
            built_kernel: Some("kernel-1"),
            ..MockSystem::default()
        };
        let outcome = run(&config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
        assert_eq!(
            system.calls.borrow()[1..],
            [
                Call::SetProfile(config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Boot),
                Call::SwitchToConfiguration(Activation::Test),
            ]
        );
    }

    #[test]
    fn changed_kernel_schedules_a_reboot() {
        let (fixture, origin, initial) = Fixture::new();
        fixture.set_current(initial);
        fixture.set_next_boot(initial);
        let second = commit(&origin, "main", &[initial], "second");
        let mut config = fixture.config;
        config.activate.allow_reboot = true;
        config.activate.booted_system = PathBuf::from("/run/booted-system");

        let system = MockSystem {
            booted_kernel: Some("kernel-1"),
            built_kernel: Some("kernel-2"),
            ..MockSystem::default()
        };
        let outcome = run(&config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::RebootScheduled {
                commit: second.to_string()
            }
        );
        assert_eq!(
            system.calls.borrow()[1..],
            [
                Call::SetProfile(config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Boot),
                Call::ScheduleReboot(15),
            ]
        );
    }

    fn store_active(config: &Config, commit: Oid, expires_at: Option<DateTime<Utc>>) {
        store_hold(
            config,
            commit,
            expires_at,
            CanaryHold::Ephemeral {
                boot_id: BOOT.to_string(),
            },
        );
    }

    fn store_hold(
        config: &Config,
        commit: Oid,
        expires_at: Option<DateTime<Utc>>,
        hold: CanaryHold,
    ) {
        CanaryState::Active {
            target: CanaryTarget::Pinned {
                branch: "canary".to_string(),
                commit: commit.to_string(),
            },
            expires_at,
            hold,
        }
        .store(&config.canary_state_path())
        .unwrap();
    }

    fn finish_reason(config: &Config) -> FinishReason {
        match CanaryState::load(&config.canary_state_path())
            .unwrap()
            .unwrap()
        {
            CanaryState::Finished { reason, .. } => reason,
            other => panic!("expected a finished canary, got {other:?}"),
        }
    }

    #[test]
    fn start_canary_runs_commit_and_stages_boot_floor() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        fixture.set_current(initial);
        fixture.set_next_boot(initial);

        let system = MockSystem::default();
        let outcome = run(
            &fixture.config,
            &system,
            now(),
            BOOT,
            Trigger::StartCanary {
                branch: "canary".to_string(),
                timeout: Some(Duration::from_secs(86400)),
                persist: false,
            },
        )
        .unwrap();

        // The trial runs the branch tip; the boot floor is the branch point
        // with the tracked tip (here `second`).
        assert_eq!(
            outcome,
            Outcome::CanaryStarted {
                commit: side.to_string(),
                next_boot: second.to_string(),
                persist: false,
                expires_at: Some(now() + chrono::Duration::seconds(86400)),
            }
        );
        assert!(outcome.activated());

        let flake = flake_ref(&fixture.config);
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(flake.clone()),
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Boot),
                Call::Build(flake),
                Call::SwitchToConfiguration(Activation::Test),
            ]
        );

        // The canary was pinned to the resolved commit, not the branch name.
        let state = CanaryState::load(&fixture.config.canary_state_path())
            .unwrap()
            .unwrap();
        assert!(matches!(
            state,
            CanaryState::Active {
                target: CanaryTarget::Pinned { commit, .. },
                expires_at: Some(_),
                hold: CanaryHold::Ephemeral { boot_id },
            } if commit == side.to_string() && boot_id == BOOT
        ));
    }

    #[test]
    fn new_host_canary_skips_a_missing_boot_floor() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let tip = commit(&origin, "main", &[second], "third");
        let side = commit(&origin, "canary", &[second], "side");
        // The host tracks the tip and has just been added to the flake on
        // the canary branch, so it is undefined at the branch point
        // (`second`) the floor would otherwise build.
        fixture.set_current(tip);
        fixture.set_next_boot(tip);

        let system = MockSystem {
            attr_defined: Some(false),
            ..MockSystem::default()
        };
        let outcome = run(
            &fixture.config,
            &system,
            now(),
            BOOT,
            Trigger::StartCanary {
                branch: "canary".to_string(),
                timeout: Some(Duration::from_secs(86400)),
                persist: false,
            },
        )
        .unwrap();

        // No floor is staged: the running tracked system stays the boot
        // default, so a reboot ends the trial as it would for any ephemeral
        // canary. The trial is only test-activated.
        assert_eq!(
            outcome,
            Outcome::CanaryStarted {
                commit: side.to_string(),
                next_boot: tip.to_string(),
                persist: false,
                expires_at: Some(now() + chrono::Duration::seconds(86400)),
            }
        );
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(flake_ref(&fixture.config)),
                Call::SwitchToConfiguration(Activation::Test),
            ]
        );
    }

    #[test]
    fn persistent_canary_takes_the_boot_default_with_no_floor() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        fixture.set_current(initial);
        fixture.set_next_boot(initial);

        let system = MockSystem::default();
        let outcome = run(
            &fixture.config,
            &system,
            now(),
            BOOT,
            Trigger::StartCanary {
                branch: "canary".to_string(),
                timeout: None,
                persist: true,
            },
        )
        .unwrap();

        // The trial itself is the next-boot system; no floor is staged.
        assert_eq!(
            outcome,
            Outcome::CanaryStarted {
                commit: side.to_string(),
                next_boot: side.to_string(),
                persist: true,
                expires_at: None,
            }
        );
        assert!(outcome.activated());

        // One build, and the profile moves — unlike the ephemeral path,
        // which builds the floor as well and only test-activates.
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(flake_ref(&fixture.config)),
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Switch),
            ]
        );

        let state = CanaryState::load(&fixture.config.canary_state_path())
            .unwrap()
            .unwrap();
        assert!(matches!(
            state,
            CanaryState::Active {
                hold: CanaryHold::Persistent,
                ..
            }
        ));
    }

    #[test]
    fn persistent_canary_survives_a_reboot() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        // Rebooted straight back onto the trial, which owns the boot default.
        fixture.set_current(side);
        fixture.set_next_boot(side);
        store_hold(&fixture.config, side, None, CanaryHold::Persistent);

        let system = MockSystem::default();
        let outcome = run(
            &fixture.config,
            &system,
            now(),
            "boot-session-b",
            Trigger::Scheduled,
        )
        .unwrap();

        // A changed boot id does not end the trial, and nothing is staged
        // under it.
        assert_eq!(
            outcome,
            Outcome::CanaryHeld {
                commit: side.to_string(),
                next_boot: side.to_string(),
            }
        );
        assert!(system.calls.borrow().is_empty());
        assert!(matches!(
            CanaryState::load(&fixture.config.canary_state_path())
                .unwrap()
                .unwrap(),
            CanaryState::Active { .. }
        ));
    }

    #[test]
    fn expired_persistent_canary_reverts_to_the_tracked_tip() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        fixture.set_current(side);
        fixture.set_next_boot(side);
        store_hold(
            &fixture.config,
            side,
            Some(now() - chrono::Duration::hours(1)),
            CanaryHold::Persistent,
        );

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        // Expiry forces the host back onto the tip, reclaiming the boot
        // default the trial held.
        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Expired);
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(flake_ref(&fixture.config)),
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Switch),
            ]
        );
    }

    #[test]
    fn unreadable_canary_state_does_not_wedge_the_cycle() {
        let (fixture, origin, initial) = Fixture::new();
        fixture.set_current(initial);
        fixture.set_next_boot(initial);
        let second = commit(&origin, "main", &[initial], "second");
        // A record from a build that predates `hold`, or plain corruption.
        fs::write(fixture.config.canary_state_path(), "{\"state\":\"active\"}").unwrap();

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        // Treated as no trial, so guarded tracking resumes rather than the
        // cycle failing and stalling every one after it.
        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
    }

    #[test]
    fn scheduled_holds_an_active_canary_in_place() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        fixture.set_current(side);
        fixture.set_next_boot(second);
        store_active(&fixture.config, side, None);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::CanaryHeld {
                commit: side.to_string(),
                next_boot: second.to_string(),
            }
        );
        assert!(!outcome.activated());
        // Holding a trial is a settled state, not a missed update.
        assert!(outcome.served());
        assert!(system.calls.borrow().is_empty());
    }

    #[test]
    fn rebooted_canary_ends_and_resumes_tracking() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let third = commit(&origin, "main", &[second], "third");
        let side = commit(&origin, "canary", &[second], "side");
        // Rebooted onto the boot floor (merge-base(side, third) = second);
        // the timeout also lapsed while the host was down.
        fixture.set_current(second);
        fixture.set_next_boot(second);
        store_active(
            &fixture.config,
            side,
            Some(now() - chrono::Duration::hours(1)),
        );

        // A changed boot id ends the trial as Rebooted (outranking the
        // lapsed timeout) and resumes tracking in the same cycle.
        let system = MockSystem::default();
        let outcome = run(
            &fixture.config,
            &system,
            now(),
            "boot-session-b",
            Trigger::Scheduled,
        )
        .unwrap();

        // Rolled forward to the tracked tip, not reapplied onto the trial.
        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: third.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Rebooted);
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(flake_ref(&fixture.config)),
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Switch),
            ]
        );
    }

    #[test]
    fn overwritten_canary_ends_and_respects_off_branch_switch() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        let hand = commit(&origin, "hand", &[second], "hand");
        // Same boot session, but the host was hand-switched to an off-branch
        // commit.
        fixture.set_current(hand);
        fixture.set_next_boot(second);
        store_active(&fixture.config, side, None);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        // Ended as Overwritten; normal tracking leaves the deliberate
        // off-branch switch alone.
        assert_eq!(
            outcome,
            Outcome::NotOnBranch {
                revision: hand.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Overwritten);
        assert!(system.calls.borrow().is_empty());
    }

    #[test]
    fn expired_canary_reverts_to_the_tracked_tip() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        fixture.set_current(side);
        fixture.set_next_boot(second);
        store_active(
            &fixture.config,
            side,
            Some(now() - chrono::Duration::hours(1)),
        );

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        // The off-branch running system is forced back onto the tip.
        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Expired);
        assert_eq!(
            *system.calls.borrow(),
            vec![
                Call::Build(flake_ref(&fixture.config)),
                Call::SetProfile(fixture.config.activate.profile.clone()),
                Call::SwitchToConfiguration(Activation::Switch),
            ]
        );
    }

    #[test]
    fn merged_canary_resumes_the_tracked_tip() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        // The branch merges back into main via a merge commit.
        let merge = commit(&origin, "main", &[second, side], "merge");
        fixture.set_current(side);
        fixture.set_next_boot(second);
        store_active(&fixture.config, side, None);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: merge.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Merged);
    }

    #[test]
    fn manual_update_clears_an_active_canary() {
        let (fixture, origin, initial) = Fixture::new();
        let second = commit(&origin, "main", &[initial], "second");
        let side = commit(&origin, "canary", &[second], "side");
        fixture.set_current(side);
        fixture.set_next_boot(second);
        store_active(&fixture.config, side, None);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Manual).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: second.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Cleared);
    }

    /// Trial `branch`'s tip on a host currently running main, and leave the
    /// fixture as the next scheduled cycle finds it: the trial running, the
    /// branch point staged for boot. Returns the commit the daemon pinned.
    fn apply_canary(fixture: &Fixture, jj: &Jj, branch: &str) -> Oid {
        let base = jj.tip("main");
        fixture.set_current(base);
        fixture.set_next_boot(base);

        let outcome = run(
            &fixture.config,
            &MockSystem::default(),
            now(),
            BOOT,
            Trigger::StartCanary {
                branch: branch.to_string(),
                timeout: None,
                persist: false,
            },
        )
        .unwrap();

        let Outcome::CanaryStarted {
            commit, next_boot, ..
        } = outcome
        else {
            panic!("expected a canary to start, got {outcome:?}")
        };
        assert_eq!(next_boot, base.to_string());
        let pinned = Oid::from_str(&commit).unwrap();
        fixture.set_current(pinned);
        pinned
    }

    #[test]
    fn a_reworded_jj_canary_merges_and_resumes_the_tracked_tip() {
        let (fixture, jj) = Fixture::jj();
        jj.commit("main", "canary change", "canary");
        jj.set_bookmark("jj/blah", "@");
        let pushed = jj.push("jj/blah");
        let pinned = apply_canary(&fixture, &jj, "jj/blah");
        assert_eq!(pinned, pushed);

        // Reworded during review, then merged. jj rewrote the commit, so the
        // hash the daemon pinned is on no branch at all any more — only its
        // change-id ties the running system to what landed.
        jj.describe("jj/blah", "canary change, reworded");
        let rewritten = jj.push("jj/blah");
        assert_ne!(rewritten, pinned);
        jj.merge_into_main("jj/blah", "merge jj/blah");
        let main_tip = jj.push("main");

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: main_tip.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Merged);
    }

    #[test]
    fn a_squashed_jj_canary_merges_when_its_change_lands() {
        let (fixture, jj) = Fixture::jj();
        jj.commit("main", "canary change", "canary");
        jj.set_bookmark("jj/blah", "@");
        jj.push("jj/blah");
        let pinned = apply_canary(&fixture, &jj, "jj/blah");

        // Amended rather than reworded, and landed by fast-forward instead of
        // a merge commit: main's tip is the rewrite itself.
        jj.squash_into("jj/blah", "amended");
        let rewritten = jj.push("jj/blah");
        assert_ne!(rewritten, pinned);
        jj.set_bookmark("main", "jj/blah");
        let main_tip = jj.push("main");
        assert_eq!(main_tip, rewritten);

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: main_tip.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Merged);
    }

    #[test]
    fn a_rewritten_jj_canary_is_held_until_its_change_lands() {
        let (fixture, jj) = Fixture::jj();
        let base = jj.tip("main");
        jj.commit("main", "canary change", "canary");
        jj.set_bookmark("jj/blah", "@");
        jj.push("jj/blah");
        let pinned = apply_canary(&fixture, &jj, "jj/blah");

        // Rewritten but not merged, while main moved on without it.
        jj.describe("jj/blah", "canary change, reworded");
        jj.push("jj/blah");
        jj.commit("main", "unrelated", "unrelated");
        jj.set_bookmark("main", "@");
        jj.push("main");

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        // A rewrite alone is not a merge; the trial holds at its floor.
        assert_eq!(
            outcome,
            Outcome::CanaryHeld {
                commit: pinned.to_string(),
                next_boot: base.to_string(),
            }
        );
        assert!(system.calls.borrow().is_empty());
    }

    #[test]
    fn a_jj_canary_stack_merges_only_once_all_of_it_lands() {
        let (fixture, jj) = Fixture::jj();
        let base = jj.tip("main");
        jj.commit("main", "lower", "lower");
        jj.set_bookmark("lower", "@");
        jj.commit("lower", "upper", "upper");
        jj.set_bookmark("jj/stack", "@");
        jj.push("lower");
        jj.push("jj/stack");
        let pinned = apply_canary(&fixture, &jj, "jj/stack");

        // The upper change is pulled out of the stack and landed on its own.
        // The host runs both changes, so it must not roll forward yet: the
        // tracked tip is missing the lower one.
        jj.rebase("jj/stack", "main");
        jj.push("jj/stack");
        jj.merge_into_main("jj/stack", "merge the upper change");
        jj.push("main");

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::CanaryHeld {
                commit: pinned.to_string(),
                next_boot: base.to_string(),
            }
        );

        // The lower change lands too, so every change the host runs is now
        // on the branch and the trial is done.
        jj.merge_into_main("lower", "merge the lower change");
        let main_tip = jj.push("main");

        let system = MockSystem::default();
        let outcome = run(&fixture.config, &system, now(), BOOT, Trigger::Scheduled).unwrap();

        assert_eq!(
            outcome,
            Outcome::Switched {
                commit: main_tip.to_string()
            }
        );
        assert_eq!(finish_reason(&fixture.config), FinishReason::Merged);
    }
}
