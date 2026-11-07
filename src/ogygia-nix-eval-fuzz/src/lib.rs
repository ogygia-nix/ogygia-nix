#![doc = include_str!("../README.md")]

mod bundle;
mod cgroup;
mod check;
mod features;
mod findings;
mod flake;
mod minimise;
mod mutate;
mod skip;
mod stats;

pub use cgroup::limit_nix_memory;
pub use check::Budget;
pub use check::Outcome;
pub use check::block_network;
pub use check::check;
pub use features::take as features;
pub use findings::Recheck;
pub use findings::recheck;
pub use findings::record;
pub use minimise::Minimised;
pub use minimise::minimise;
pub use mutate::crossover;
pub use mutate::mutate;
pub use skip::Skip;
pub use stats::Summary;
pub use stats::count;
