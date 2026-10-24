//! The daemon's local API: one JSON object per line over a Unix socket.
//!
//! A client writes a [`Request`] and reads back one [`Response`] per request,
//! like Tailscale's GUI talking to `tailscaled`. Every successful response
//! but the mesh carries the current [`Status`], so the app never needs a
//! second round trip to show the effect of a change. `nc -U` is enough to poke it by hand:
//!
//! ```text
//! $ echo '{"command":"status"}' | nc -U /var/run/ogygia/macos-nebulad.sock
//! ```

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use crate::config::ConfigMode;
use crate::config::LocalConfig;
use crate::mesh::Mesh;

/// Where the daemon listens.
pub const SOCKET_PATH: &str = "/var/run/ogygia/macos-nebulad.sock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    /// Report the current status.
    Status,
    /// Store new settings and re-evaluate at once. Only administrators may
    /// change them, as they decide what this Mac's tunnel trusts.
    SetSettings {
        repository_url: String,
        /// Empty or absent follows the repository's default branch.
        #[serde(default)]
        branch: Option<String>,
        device: String,
    },
    /// Re-evaluate now rather than at the next interval.
    Refresh,
    /// Follow the fleet's Nebula config, or manage it locally with `local`,
    /// which is stored either way. Only administrators may change it.
    SetConfig {
        mode: ConfigMode,
        local: LocalConfig,
    },
    /// Report the mesh as Nebula sees it. Only administrators may see it, as
    /// it names every peer's address outside the mesh.
    Mesh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Status(Box<Status>),
    Mesh(Mesh),
    Error(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The app build (`CFBundleVersion`) this daemon was started from. The
    /// app compares it with its own to spot a daemon an update has not
    /// restarted yet.
    pub build: Option<String>,
    /// The fleet repository being followed, if set.
    pub repository_url: Option<String>,
    /// The branch of it being followed, if not the default one.
    pub branch: Option<String>,
    /// This Mac's `ogygiaConfigurations` attribute name, if set.
    pub device: Option<String>,
    /// Whether the settings are forced by a configuration profile, in which
    /// case `set_settings` cannot change them.
    pub settings_managed: bool,
    /// This Mac's Nebula public key in PEM, for the fleet's `pubKey`.
    pub public_key: Option<String>,
    /// This Mac's mesh address, from the certificate in the current config.
    pub address: Option<String>,
    /// When that certificate expires.
    pub cert_expires: Option<DateTime<Utc>>,
    /// Whether an evaluation is running now.
    pub updating: bool,
    /// When the device's config last evaluated successfully.
    pub last_update: Option<DateTime<Utc>>,
    /// Why the last evaluation failed, if it did. Nebula keeps running on the
    /// previous config meanwhile.
    pub update_error: Option<String>,
    pub nebula: Nebula,
    /// Whether Nebula runs the fleet's config or the local one.
    pub config_mode: ConfigMode,
    /// Whether a configuration profile forces the mode.
    pub config_mode_managed: bool,
    /// The fleet's values for what a local config owns, once evaluated.
    pub fleet_config: Option<LocalConfig>,
    /// The local config, used while managing locally.
    pub local_config: LocalConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Nebula {
    /// No config to run yet.
    #[default]
    Stopped,
    Running {
        since: DateTime<Utc>,
    },
    /// Nebula exited and is waiting to be restarted.
    Exited {
        /// The last line Nebula logged, which usually says why.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged_by_command() {
        let req: Request = serde_json::from_str(
            r#"{"command":"set_settings","repository_url":"https://example.com/fleet.git","device":"mac"}"#,
        )
        .unwrap();
        assert_eq!(
            req,
            Request::SetSettings {
                repository_url: "https://example.com/fleet.git".into(),
                branch: None,
                device: "mac".into(),
            }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"command":"status"}"#).unwrap(),
            Request::Status
        );
    }

    #[test]
    fn nebula_state_is_tagged() {
        let json = serde_json::to_value(Nebula::Exited {
            reason: "bad cert".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "state": "exited", "reason": "bad cert" })
        );
    }
}
