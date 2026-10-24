//! Turning the evaluated `settings` into the file Nebula reads.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;

use crate::config::LocalConfig;

/// The debug sshd the daemon runs inside Nebula to read the mesh from it.
pub struct Sshd {
    /// A loopback `address:port`.
    pub listen: String,
    pub host_key: PathBuf,
    /// The OpenSSH public key the daemon signs in with.
    pub authorized_key: String,
    /// Where the profiling commands may write. The daemon never runs them.
    pub sandbox_dir: PathBuf,
}

/// The user the daemon signs in to the sshd as.
pub const SSHD_USER: &str = "ogygia-macos-nebulad";

/// Nebula's config for this Mac: the evaluated settings, with whatever the
/// Mac manages locally in place of the fleet's, the path of the key, which
/// never leaves the Mac and so is never in the fleet repository, and the
/// daemon's own sshd.
///
/// The result is JSON, which Nebula reads as the YAML subset it is.
///
/// Any administrator can point the daemon at a repository, as with
/// `tailscaled`, so the settings must not reach beyond the mesh. Nebula's
/// debug `sshd` does: its profiling commands create files as root wherever
/// the config's `sshd.sandbox_dir` allows, so only the daemon may configure
/// it, on loopback, with a sandbox only root can reach.
pub fn render(
    mut settings: Value,
    local: Option<&LocalConfig>,
    key: &Path,
    sshd: &Sshd,
) -> Result<Vec<u8>> {
    if settings.get("sshd").is_some() {
        bail!("the evaluated settings enable Nebula's sshd, which a device does not run");
    }
    if let Some(local) = local {
        local.apply(&mut settings)?;
    }
    let Some(root) = settings.as_object_mut() else {
        bail!("the evaluated settings are not an object");
    };
    let Some(pki) = root.get_mut("pki").and_then(Value::as_object_mut) else {
        bail!("the evaluated settings have no `pki` section");
    };
    if pki.contains_key("key") {
        bail!("the evaluated settings carry a key; a device's key must stay on the device");
    }
    let key = key.to_str().context("the key path is not UTF-8")?;
    pki.insert("key".into(), Value::String(key.into()));

    let path = |p: &Path| {
        p.to_str()
            .context("an sshd path is not UTF-8")
            .map(str::to_owned)
    };
    root.insert(
        "sshd".into(),
        json!({
            "enabled": true,
            "listen": sshd.listen,
            "host_key": path(&sshd.host_key)?,
            "sandbox_dir": path(&sshd.sandbox_dir)?,
            "authorized_users": [{ "user": SSHD_USER, "keys": [sshd.authorized_key] }],
        }),
    );

    let mut out = serde_json::to_vec_pretty(&settings)?;
    out.push(b'\n');
    Ok(out)
}

/// The PEM certificate in a rendered config, for reporting this Mac's
/// address.
pub fn certificate(config: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(config).ok()?;
    value.get("pki")?.get("cert")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::Lighthouse;

    fn sshd() -> Sshd {
        Sshd {
            listen: "127.0.0.1:4243".into(),
            host_key: "/state/sshd_host_key".into(),
            authorized_key: "ssh-ed25519 AAAA daemon".into(),
            sandbox_dir: "/state/sshd".into(),
        }
    }

    fn render(settings: Value, key: &Path) -> Result<Vec<u8>> {
        super::render(settings, None, key, &sshd())
    }

    #[test]
    fn the_key_path_and_the_daemons_sshd_are_added() {
        let settings = json!({ "pki": { "ca": "CA", "cert": "CERT" }, "listen": { "port": 0 } });
        let out = render(settings, Path::new("/var/lib/host.key")).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            out,
            json!({
                "pki": { "ca": "CA", "cert": "CERT", "key": "/var/lib/host.key" },
                "listen": { "port": 0 },
                "sshd": {
                    "enabled": true,
                    "listen": "127.0.0.1:4243",
                    "host_key": "/state/sshd_host_key",
                    "sandbox_dir": "/state/sshd",
                    "authorized_users": [{ "user": SSHD_USER, "keys": ["ssh-ed25519 AAAA daemon"] }],
                },
            })
        );
    }

    #[test]
    fn a_local_config_replaces_the_fleets() {
        let settings = json!({
            "pki": { "ca": "CA", "cert": "CERT" },
            "lighthouse": { "am_lighthouse": false, "hosts": ["10.0.0.1"] },
        });
        let local = LocalConfig {
            lighthouses: vec![Lighthouse {
                address: "10.0.0.9".into(),
                endpoints: vec![],
            }],
            ..LocalConfig::default()
        };
        let out = super::render(settings, Some(&local), Path::new("/k"), &sshd()).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            out["lighthouse"],
            json!({ "am_lighthouse": false, "hosts": ["10.0.0.9"] })
        );
        assert_eq!(out["pki"]["cert"], json!("CERT"));
    }

    #[test]
    fn a_key_from_the_repository_is_refused() {
        let settings = json!({ "pki": { "ca": "CA", "cert": "CERT", "key": "SECRET" } });
        assert!(render(settings, Path::new("/k")).is_err());
    }

    #[test]
    fn the_debug_sshd_is_refused() {
        let settings = json!({
            "pki": { "ca": "CA", "cert": "CERT" },
            "sshd": { "enabled": true, "sandbox_dir": "/" },
        });
        assert!(render(settings, Path::new("/k")).is_err());
    }

    #[test]
    fn settings_without_pki_are_refused() {
        assert!(render(json!({ "listen": {} }), Path::new("/k")).is_err());
        assert!(render(json!("nonsense"), Path::new("/k")).is_err());
    }

    #[test]
    fn the_certificate_is_read_back() {
        let out = render(
            json!({ "pki": { "ca": "CA", "cert": "CERT" } }),
            Path::new("/k"),
        )
        .unwrap();
        assert_eq!(certificate(&out).as_deref(), Some("CERT"));
    }
}
