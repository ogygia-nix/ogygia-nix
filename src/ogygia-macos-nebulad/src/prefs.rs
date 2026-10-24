//! Reading and writing [`Settings`] through CFPreferences, the API behind
//! `defaults`.

use core_foundation::base::CFType;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::preferences::CFPreferencesAppSynchronize;
use core_foundation_sys::preferences::CFPreferencesAppValueIsForced;
use core_foundation_sys::preferences::CFPreferencesCopyAppValue;
use core_foundation_sys::preferences::CFPreferencesSetValue;
use core_foundation_sys::preferences::CFPreferencesSynchronize;
use core_foundation_sys::preferences::kCFPreferencesAnyHost;
use core_foundation_sys::preferences::kCFPreferencesAnyUser;

use crate::config::ConfigMode;
use crate::config::LocalConfig;
use crate::settings::DOMAIN;
use crate::settings::KEY_BRANCH;
use crate::settings::KEY_CONFIG_MODE;
use crate::settings::KEY_DEVICE;
use crate::settings::KEY_LOCAL_CONFIG;
use crate::settings::KEY_REPOSITORY_URL;
use crate::settings::Settings;

/// The settings as the daemon sees them: forced values from a configuration
/// profile first, then `/Library/Preferences`.
pub fn load() -> Settings {
    let domain = CFString::new(DOMAIN);
    // Drop cfprefsd's cached copy so a `defaults write` made since the last
    // read is seen.
    unsafe { CFPreferencesAppSynchronize(domain.as_concrete_TypeRef()) };
    Settings {
        repository_url: string(&domain, KEY_REPOSITORY_URL),
        branch: string(&domain, KEY_BRANCH),
        device: string(&domain, KEY_DEVICE),
    }
}

/// Whether a configuration profile forces any of the settings.
pub fn managed() -> bool {
    [KEY_REPOSITORY_URL, KEY_BRANCH, KEY_DEVICE]
        .iter()
        .any(|key| forced(key))
}

/// Whether to follow the fleet's Nebula config, and the local one. Either
/// left unset is the default, but a value that does not parse is an error,
/// so that a typo cannot change what Nebula runs.
pub fn load_config() -> anyhow::Result<(ConfigMode, LocalConfig)> {
    let domain = CFString::new(DOMAIN);
    unsafe { CFPreferencesAppSynchronize(domain.as_concrete_TypeRef()) };
    parse_config(
        string(&domain, KEY_CONFIG_MODE).as_deref(),
        string(&domain, KEY_LOCAL_CONFIG).as_deref(),
    )
}

fn parse_config(
    mode: Option<&str>,
    local: Option<&str>,
) -> anyhow::Result<(ConfigMode, LocalConfig)> {
    let mode = match mode {
        None | Some("fleet") => ConfigMode::Fleet,
        Some("local") => ConfigMode::Local,
        Some(other) => anyhow::bail!("{KEY_CONFIG_MODE} is {other:?}, not fleet or local"),
    };
    let local = match local {
        None => LocalConfig::default(),
        Some(json) => serde_json::from_str(json)
            .map_err(|e| anyhow::anyhow!("{KEY_LOCAL_CONFIG} does not parse: {e}"))?,
    };
    Ok((mode, local))
}

/// Whether a configuration profile forces the config mode or the local
/// config.
pub fn config_mode_managed() -> bool {
    forced(KEY_CONFIG_MODE) || forced(KEY_LOCAL_CONFIG)
}

fn forced(key: &str) -> bool {
    let domain = CFString::new(DOMAIN);
    let key = CFString::new(key);
    unsafe {
        CFPreferencesAppValueIsForced(key.as_concrete_TypeRef(), domain.as_concrete_TypeRef()) != 0
    }
}

/// Store the settings for every user of this Mac, in
/// `/Library/Preferences/dev.ogygia.macos.plist`, removing any that are
/// unset. Only root may write there.
pub fn store(settings: &Settings) -> anyhow::Result<()> {
    write([
        (KEY_REPOSITORY_URL, settings.repository_url.clone()),
        (KEY_BRANCH, settings.branch.clone()),
        (KEY_DEVICE, settings.device.clone()),
    ])
}

/// Store the config mode and the local config beside the settings.
pub fn store_config(mode: ConfigMode, local: &LocalConfig) -> anyhow::Result<()> {
    let mode = match mode {
        ConfigMode::Fleet => "fleet",
        ConfigMode::Local => "local",
    };
    write([
        (KEY_CONFIG_MODE, Some(mode.to_owned())),
        (KEY_LOCAL_CONFIG, Some(serde_json::to_string(local)?)),
    ])
}

fn write<const N: usize>(values: [(&str, Option<String>); N]) -> anyhow::Result<()> {
    let domain = CFString::new(DOMAIN);
    for (key, value) in values {
        let key = CFString::new(key);
        let value = value.as_deref().map(CFString::new);
        unsafe {
            CFPreferencesSetValue(
                key.as_concrete_TypeRef(),
                // A null value removes the key.
                value
                    .as_ref()
                    .map_or(std::ptr::null(), |v| v.as_CFTypeRef()),
                domain.as_concrete_TypeRef(),
                kCFPreferencesAnyUser,
                kCFPreferencesAnyHost,
            );
        }
    }
    let synced = unsafe {
        CFPreferencesSynchronize(
            domain.as_concrete_TypeRef(),
            kCFPreferencesAnyUser,
            kCFPreferencesAnyHost,
        )
    };
    if synced == 0 {
        anyhow::bail!("could not write the settings to /Library/Preferences/{DOMAIN}.plist");
    }
    Ok(())
}

fn string(domain: &CFString, key: &str) -> Option<String> {
    let key = CFString::new(key);
    let value = unsafe {
        CFPreferencesCopyAppValue(key.as_concrete_TypeRef(), domain.as_concrete_TypeRef())
    };
    if value.is_null() {
        return None;
    }
    // Copy rule: the returned reference is ours to release.
    let value = unsafe { CFType::wrap_under_create_rule(value) };
    let value = value.downcast_into::<CFString>()?.to_string();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_values_are_the_defaults() {
        assert_eq!(
            parse_config(None, None).unwrap(),
            (ConfigMode::Fleet, LocalConfig::default())
        );
    }

    #[test]
    fn stored_values_are_read() {
        let (mode, local) = parse_config(Some("local"), Some(r#"{"listen_port":4242}"#)).unwrap();
        assert_eq!(mode, ConfigMode::Local);
        assert_eq!(local.listen_port, 4242);
    }

    #[test]
    fn values_that_do_not_parse_are_errors_not_defaults() {
        assert!(parse_config(Some("Local"), None).is_err());
        assert!(parse_config(Some("local"), Some("{")).is_err());
        assert!(parse_config(Some("local"), Some(r#"{"listen_port":"4242"}"#)).is_err());
    }
}
