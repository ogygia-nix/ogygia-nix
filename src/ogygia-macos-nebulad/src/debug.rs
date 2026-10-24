//! Reading the mesh from Nebula's debug sshd, which only the daemon may
//! enable.
//!
//! Nebula listens on loopback with a host key the daemon generates and
//! accepts only the daemon's own client key; both live in the root-only
//! state directory. The daemon connects with the system's OpenSSH client and
//! pins the host key, so another process holding the port cannot feed it a
//! made-up mesh, and runs only Nebula's read-only listing commands.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use tokio::process::Command;

use crate::render::SSHD_USER;
use crate::render::Sshd;

/// Loopback only. Nebula refuses port 22, and this one sits beside its usual
/// 4242.
const LISTEN_HOST: &str = "127.0.0.1";
const LISTEN_PORT: u16 = 4243;
/// A command that takes longer than this has hung.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Debug {
    host_key: PathBuf,
    client_key: PathBuf,
    known_hosts: PathBuf,
    sandbox_dir: PathBuf,
}

impl Debug {
    pub fn new(state: &Path) -> Self {
        Self {
            host_key: state.join("sshd_host_key"),
            client_key: state.join("sshd_client_key"),
            known_hosts: state.join("sshd_known_hosts"),
            sandbox_dir: state.join("sshd"),
        }
    }

    /// Generate the keys on first start and return the sshd Nebula should
    /// run.
    pub async fn ensure(&self) -> Result<Sshd> {
        let host = ensure_key(&self.host_key).await?;
        let client = ensure_key(&self.client_key).await?;
        std::fs::write(
            &self.known_hosts,
            format!("[{LISTEN_HOST}]:{LISTEN_PORT} {host}\n"),
        )
        .with_context(|| format!("could not write {}", self.known_hosts.display()))?;
        std::fs::create_dir_all(&self.sandbox_dir)
            .with_context(|| format!("could not create {}", self.sandbox_dir.display()))?;
        std::fs::set_permissions(&self.sandbox_dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(Sshd {
            listen: format!("{LISTEN_HOST}:{LISTEN_PORT}"),
            host_key: self.host_key.clone(),
            authorized_key: client,
            sandbox_dir: self.sandbox_dir.clone(),
        })
    }

    /// Run one of Nebula's debug commands and return what it printed.
    pub async fn run(&self, command: &str) -> Result<Vec<u8>> {
        let output = Command::new("/usr/bin/ssh")
            // Nothing from any user's or the system's SSH config applies.
            .args(["-F", "/dev/null", "-T", "-i"])
            .arg(&self.client_key)
            .arg("-o")
            // ssh splits this option on spaces, as it takes a list, and the
            // state directory is under "Application Support".
            .arg(format!(
                "UserKnownHostsFile=\"{}\"",
                self.known_hosts.display()
            ))
            .args([
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "LogLevel=ERROR",
                "-p",
            ])
            .arg(LISTEN_PORT.to_string())
            .arg(format!("{SSHD_USER}@{LISTEN_HOST}"))
            .arg(command)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(COMMAND_TIMEOUT, output)
            .await
            .with_context(|| format!("Nebula did not answer {command}"))?
            .context("could not run ssh")?;
        if !output.status.success() {
            bail!(
                "could not ask Nebula for {command}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }
}

/// An Ed25519 key at `path`, generated if missing, returning its public half
/// in OpenSSH form.
async fn ensure_key(path: &Path) -> Result<String> {
    let public = path.with_extension("pub");
    if !path.exists() || !public.exists() {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(&public);
        let output = Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", SSHD_USER, "-f"])
            .arg(path)
            .output()
            .await
            .context("could not run ssh-keygen")?;
        if !output.status.success() {
            bail!(
                "ssh-keygen failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let public = std::fs::read_to_string(&public)
        .with_context(|| format!("could not read {}", public.display()))?;
    Ok(public.trim().to_owned())
}
