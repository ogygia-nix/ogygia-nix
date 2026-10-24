//! The privileged half of the Ogygia Mac app: a root launchd daemon that runs
//! Nebula for this Mac and nothing else.
//!
//! The daemon reads which fleet repository and device to follow from the
//! `defaults` domain [`settings::DOMAIN`], evaluates the device's
//! `ogygiaConfigurations` entry with the system `nix`, and runs the `nebula`
//! binary bundled beside it on the result, restarting it whenever the
//! evaluated config changes. The menu bar app talks to it over a Unix socket
//! with the line protocol in [`protocol`].
//!
//! The protocol, settings validation and config rendering are platform
//! neutral so they are tested everywhere; everything that touches the system
//! is macOS only.

pub mod config;
pub mod mesh;
pub mod protocol;
pub mod render;
pub mod report;
pub mod settings;

#[cfg(target_os = "macos")]
pub mod daemon;
#[cfg(target_os = "macos")]
mod debug;
#[cfg(target_os = "macos")]
mod peer;
#[cfg(target_os = "macos")]
mod prefs;
#[cfg(target_os = "macos")]
mod supervisor;
