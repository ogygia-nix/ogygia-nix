//! What the daemon follows: a fleet repository, optionally a branch of it,
//! and this Mac's name in it.
//!
//! All three live in the `defaults` domain [`DOMAIN`] at the any-user, any-host
//! level, which is `/Library/Preferences/dev.ogygia.macos.plist`. That is
//! where `defaults write /Library/Preferences/dev.ogygia.macos` and
//! nix-darwin's `system.defaults.CustomSystemPreferences` put them, and a
//! configuration profile can force them over the top.

use anyhow::Result;
use anyhow::bail;

/// The app's bundle identifier, which is also its `defaults` domain.
pub const DOMAIN: &str = "dev.ogygia.macos";
pub const KEY_REPOSITORY_URL: &str = "RepositoryURL";
pub const KEY_BRANCH: &str = "Branch";
pub const KEY_DEVICE: &str = "Device";
/// `fleet` or `local`; see [`crate::config`].
pub const KEY_CONFIG_MODE: &str = "ConfigMode";
/// The local config, as JSON.
pub const KEY_LOCAL_CONFIG: &str = "LocalConfig";

/// What [`Settings::installable`] is evaluated with: the device's Nebula
/// config, and the fleet's hosts, so the app can name the mesh. The result
/// deserializes as a [`crate::config::Fleet`].
pub const APPLY: &str = "nebula: { inherit (nebula) settings; topology = { \
    hosts = builtins.mapAttrs (_: host: host.ipv4) nebula.topology.hosts; \
    inherit (nebula.topology) lighthouses relays; }; }";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub repository_url: Option<String>,
    /// The branch to follow; the repository's default branch when unset.
    pub branch: Option<String>,
    pub device: Option<String>,
}

impl Settings {
    /// The installable for this device's Nebula module, to evaluate with
    /// [`APPLY`], or `None` until the repository and device are both set.
    pub fn installable(&self) -> Result<Option<String>> {
        let (Some(url), Some(device)) = (&self.repository_url, &self.device) else {
            return Ok(None);
        };
        self.validate()?;
        let reference = match &self.branch {
            Some(branch) => format!("git+{url}?ref={branch}"),
            None => format!("git+{url}"),
        };
        Ok(Some(format!(
            "{reference}#ogygiaConfigurations.\"{device}\".config.ogygia.nebula"
        )))
    }

    /// Check every setting that is present.
    pub fn validate(&self) -> Result<()> {
        if let Some(url) = &self.repository_url {
            validate_repository_url(url)?;
        }
        if let Some(branch) = &self.branch {
            validate_branch(branch)?;
        }
        if let Some(device) = &self.device {
            validate_device(device)?;
        }
        Ok(())
    }
}

/// The repository is fetched anonymously, as Nix's `git+` fetcher does it.
/// `https` is what devices use; `file` lets an operator point a Mac at a
/// local checkout.
pub fn validate_repository_url(url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("file:///")) {
        bail!("the repository URL must start with https:// or file:///");
    }
    // A fragment would end the flake reference early and a query would be
    // read as fetcher arguments; neither belongs in a repository URL.
    if url.contains(['#', '?']) || url.chars().any(char::is_whitespace) {
        bail!("the repository URL must not contain '#', '?' or whitespace");
    }
    Ok(())
}

/// The branch becomes the fetcher's `ref`, so it must be a plain branch name
/// that cannot add further arguments or leave the flake reference.
pub fn validate_branch(branch: &str) -> Result<()> {
    if branch.is_empty() {
        bail!("the branch must not be empty");
    }
    if !branch
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        bail!("the branch may only contain letters, digits, '-', '_', '.' and '/'");
    }
    if branch.starts_with(['-', '/', '.']) || branch.ends_with('/') || branch.contains("..") {
        bail!("{branch} is not a valid branch name");
    }
    Ok(())
}

/// The device is quoted as one attribute name, so it must not be able to
/// close the quotes or interpolate.
pub fn validate_device(device: &str) -> Result<()> {
    if device.is_empty() {
        bail!("the device name must not be empty");
    }
    if !device
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("the device name may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(url: &str, device: &str) -> Settings {
        Settings {
            repository_url: Some(url.into()),
            branch: None,
            device: Some(device.into()),
        }
    }

    #[test]
    fn nothing_to_evaluate_until_both_are_set() {
        let half = Settings {
            repository_url: Some("https://example.com/fleet.git".into()),
            branch: Some("main".into()),
            device: None,
        };
        assert_eq!(half.installable().unwrap(), None);
        assert_eq!(Settings::default().installable().unwrap(), None);
    }

    #[test]
    fn the_device_is_quoted_as_one_attribute() {
        assert_eq!(
            settings("https://example.com/fleet.git", "mac.lt.example.com")
                .installable()
                .unwrap()
                .unwrap(),
            "git+https://example.com/fleet.git#ogygiaConfigurations.\"mac.lt.example.com\".config.ogygia.nebula"
        );
    }

    #[test]
    fn a_branch_becomes_the_ref() {
        let branched = Settings {
            branch: Some("jj/abcdefgh".into()),
            ..settings("https://example.com/fleet.git", "mac")
        };
        assert_eq!(
            branched.installable().unwrap().unwrap(),
            "git+https://example.com/fleet.git?ref=jj/abcdefgh#ogygiaConfigurations.\"mac\".config.ogygia.nebula"
        );
    }

    #[test]
    fn branches_cannot_add_fetcher_arguments() {
        for branch in [
            "",
            "a&rev=0000",
            "a#b",
            "-x",
            "/main",
            "main/",
            "a..b",
            "a b",
        ] {
            assert!(validate_branch(branch).is_err(), "{branch:?}");
        }
        assert!(validate_branch("release/2026.10").is_ok());
    }

    #[test]
    fn local_checkouts_are_allowed() {
        assert!(
            settings("file:///Users/op/fleet", "mac")
                .installable()
                .is_ok()
        );
    }

    #[test]
    fn other_schemes_are_refused() {
        for url in [
            "http://example.com/fleet.git",
            "ssh://git@example.com/fleet",
            "fleet",
        ] {
            assert!(validate_repository_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn urls_cannot_smuggle_flake_syntax() {
        for url in [
            "https://example.com/fleet.git#nixosConfigurations",
            "https://example.com/fleet.git?ref=evil",
            "https://example.com/fleet .git",
        ] {
            assert!(validate_repository_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn device_names_cannot_escape_their_quotes() {
        for device in ["", "mac\"", "mac${x}", "a\\b", "two words"] {
            assert!(validate_device(device).is_err(), "{device:?}");
        }
    }
}
