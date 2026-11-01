//! Pull-based NixOS auto-updater.
//!
//! Tracks a branch of the NixOS configuration repository and updates the
//! local system to its tip: the running and next-boot systems' build
//! revisions (embedded by `ogygia.versions`) gate the update so hosts
//! deliberately running unmerged builds are left alone, the new system is
//! built from a private clone, and activation mirrors nixos-rebuild
//! (`test`/`switch`/`boot` plus a delayed reboot on kernel changes).
//!
//! The `ogygia-updated` daemon owns all update state and runs cycles on
//! an interval; `ogygia update` asks it to run a cycle now over its
//! control socket.

pub mod branches;
pub mod control;

mod canary;
mod config;
mod engine;
mod repo;
mod system;

#[cfg(test)]
mod jjtest;

pub use canary::CanaryHold;
pub use canary::CanaryState;
pub use canary::CanaryTarget;
pub use canary::FinishReason;
pub use config::Config;
pub use config::DEFAULT_CONFIG_PATH;
pub use engine::Outcome;
pub use engine::Trigger;
pub use engine::run_once;
pub use system::Activation;
