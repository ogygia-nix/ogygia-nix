//! The daemon itself: key, update loop, Nebula and the socket.

use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use chrono::Utc;
use core_foundation::bundle::CFBundle;
use core_foundation::string::CFString;
use ogygia_nixutils::Nix;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use tokio::sync::Notify;
use tokio::sync::watch;

use crate::config::ConfigMode;
use crate::config::Fleet;
use crate::config::LocalConfig;
use crate::debug::Debug;
use crate::mesh;
use crate::mesh::Mesh;
use crate::peer;
use crate::prefs;
use crate::protocol::Nebula;
use crate::protocol::Request;
use crate::protocol::Response;
use crate::protocol::SOCKET_PATH;
use crate::protocol::Status;
use crate::render;
use crate::render::Sshd;
use crate::report;
use crate::settings;
use crate::supervisor::Supervisor;

/// Root-only state: the keys, and the last config that evaluated.
const STATE_DIR: &str = "/Library/Application Support/Ogygia/Nebula";
/// How often to look for a new config, as `ogygia-updated` polls on NixOS.
const UPDATE_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// A first evaluation fetches nixpkgs, so allow it a while.
const EVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How often to check whether an app update has replaced the daemon.
const REPLACED_CHECK: Duration = Duration::from_secs(60);

struct Paths {
    exe: PathBuf,
    key: PathBuf,
    public_key: PathBuf,
    config: PathBuf,
    cert: PathBuf,
    fleet: PathBuf,
    nebula: PathBuf,
    nebula_cert: PathBuf,
}

impl Paths {
    /// Nebula ships in the app bundle beside the daemon, so the app's version
    /// of Nebula is always the one that runs.
    fn new() -> Result<Self> {
        let exe = std::env::current_exe().context("could not find the daemon's own path")?;
        let bin = exe.parent().context("the daemon has no parent directory")?;
        let state = Path::new(STATE_DIR);
        Ok(Self {
            exe: exe.clone(),
            key: state.join("host.key"),
            public_key: state.join("host.pub"),
            config: state.join("config.yml"),
            cert: state.join("host.crt"),
            fleet: state.join("fleet.json"),
            nebula: bin.join("nebula"),
            nebula_cert: bin.join("nebula-cert"),
        })
    }
}

struct Daemon {
    paths: Paths,
    nix: Nix,
    debug: Debug,
    sshd: Sshd,
    /// The last fleet config that evaluated, which a local config is applied
    /// to.
    fleet: Mutex<Option<Fleet>>,
    /// Held while choosing and writing the config, which both the update
    /// loop and the app's changes do.
    applying: tokio::sync::Mutex<()>,
    /// Why the config mode or local config in the preferences could not be
    /// read, if they could not. Nothing is applied until they can, so Nebula
    /// keeps the config it runs.
    config_error: Mutex<Option<String>>,
    status: Arc<Mutex<Status>>,
    refresh: Notify,
    changes: watch::Sender<u64>,
}

pub async fn run() -> Result<()> {
    let paths = Paths::new()?;
    std::fs::create_dir_all(STATE_DIR).with_context(|| format!("could not create {STATE_DIR}"))?;
    std::fs::set_permissions(STATE_DIR, std::fs::Permissions::from_mode(0o700))?;

    let public_key = ensure_key(&paths).await?;
    let debug = Debug::new(Path::new(STATE_DIR));
    let sshd = debug.ensure().await?;
    let fleet = load_fleet(&paths.fleet);
    let (config_mode, local_config, config_error) = match prefs::load_config() {
        Ok((mode, local)) => (mode, local, None),
        Err(e) => (
            ConfigMode::default(),
            LocalConfig::default(),
            Some(format!("{e:#}")),
        ),
    };
    let mut status = Status {
        build: build(),
        public_key: Some(public_key),
        config_mode,
        config_mode_managed: prefs::config_mode_managed(),
        fleet_config: fleet
            .as_ref()
            .map(|f| LocalConfig::from_settings(&f.settings)),
        local_config,
        ..Status::default()
    };
    if let Ok(info) = cert_info(&paths).await {
        status.address = info.address;
        status.cert_expires = Some(info.not_after);
    }

    let (changes, changes_rx) = watch::channel(0u64);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let daemon = Arc::new(Daemon {
        nix: Nix::with_eval_args(
            [
                "--extra-experimental-features",
                "nix-command flakes",
                // Fetch the repository afresh rather than trusting a cached
                // copy, so a pushed cert reaches the Mac on the next update.
                "--refresh",
            ]
            .map(String::from),
        ),
        debug,
        sshd,
        fleet: Mutex::new(fleet),
        applying: tokio::sync::Mutex::new(()),
        config_error: Mutex::new(config_error),
        status: Arc::new(Mutex::new(status)),
        refresh: Notify::new(),
        changes,
        paths,
    });

    // Render the last fleet config again, in case the daemon's part of it,
    // such as its sshd, has changed since it was written.
    if let Err(e) = daemon.apply_fleet().await {
        daemon.status.lock().unwrap().update_error = Some(format!("{e:#}"));
    }

    // The last good config starts at once, so the mesh is up at boot even
    // when the repository cannot be reached.
    let supervisor = tokio::spawn(
        Supervisor {
            nebula: daemon.paths.nebula.clone(),
            config: daemon.paths.config.clone(),
            status: daemon.status.clone(),
        }
        .run(changes_rx, shutdown_rx),
    );

    let listener = listen()?;
    tokio::spawn(serve(daemon.clone(), listener));
    tokio::spawn(update_loop(daemon.clone()));

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => tracing::info!("stopping"),
        _ = int.recv() => tracing::info!("stopping"),
        // launchd keeps running the old binary after Sparkle replaces the
        // app, so exit and let KeepAlive start the new one from the bundle.
        _ = replaced(daemon.paths.exe.clone()) => {
            tracing::info!("an update replaced the daemon; exiting so launchd starts the new one");
        }
    }
    let _ = shutdown.send(true);
    let _ = supervisor.await;
    let _ = std::fs::remove_file(SOCKET_PATH);
    Ok(())
}

/// The app build this daemon came from, from the bundle it sits in.
fn build() -> Option<String> {
    let info = CFBundle::main_bundle().info_dictionary();
    let version = info.find(CFString::from_static_string("CFBundleVersion"))?;
    version.downcast::<CFString>().map(|v| v.to_string())
}

/// Resolve once the daemon's executable is no longer the file it started
/// from: the app it ships in was updated or reinstalled. A missing file is
/// an update still in progress, so only a new file counts.
async fn replaced(exe: PathBuf) {
    let identity = |path: &Path| {
        std::fs::metadata(path)
            .ok()
            .map(|m| (m.dev(), m.ino(), m.mtime(), m.mtime_nsec()))
    };
    let Some(original) = identity(&exe) else {
        tracing::warn!(exe = %exe.display(), "cannot watch the daemon's own executable for updates");
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep(REPLACED_CHECK).await;
        if identity(&exe).is_some_and(|now| now != original) {
            return;
        }
    }
}

/// The Mac's Nebula key, generated on first start. Only its public half ever
/// leaves this directory.
async fn ensure_key(paths: &Paths) -> Result<String> {
    if !paths.key.exists() || !paths.public_key.exists() {
        tracing::info!("generating this Mac's Nebula key");
        let _ = std::fs::remove_file(&paths.key);
        let _ = std::fs::remove_file(&paths.public_key);
        let output = Command::new(&paths.nebula_cert)
            .arg("keygen")
            .arg("-out-key")
            .arg(&paths.key)
            .arg("-out-pub")
            .arg(&paths.public_key)
            .output()
            .await
            .context("could not run nebula-cert keygen")?;
        if !output.status.success() {
            bail!(
                "nebula-cert keygen failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    std::fs::set_permissions(&paths.key, std::fs::Permissions::from_mode(0o600))?;
    std::fs::read_to_string(&paths.public_key)
        .with_context(|| format!("could not read {}", paths.public_key.display()))
}

async fn update_loop(daemon: Arc<Daemon>) {
    loop {
        daemon.update().await;
        tokio::select! {
            _ = tokio::time::sleep(UPDATE_INTERVAL) => {}
            _ = daemon.refresh.notified() => {}
        }
    }
}

impl Daemon {
    async fn update(&self) {
        let settings = prefs::load();
        {
            let mut status = self.status.lock().unwrap();
            status.repository_url = settings.repository_url.clone();
            status.branch = settings.branch.clone();
            status.device = settings.device.clone();
            status.settings_managed = prefs::managed();
            // A `defaults write` or a profile may have changed these too.
            // Ones that do not parse leave the last good ones in place.
            match prefs::load_config() {
                Ok((mode, local)) => {
                    (status.config_mode, status.local_config) = (mode, local);
                    *self.config_error.lock().unwrap() = None;
                }
                Err(e) => *self.config_error.lock().unwrap() = Some(format!("{e:#}")),
            }
            status.config_mode_managed = prefs::config_mode_managed();
        }

        let installable = match settings.installable() {
            Ok(Some(installable)) => installable,
            Ok(None) => {
                // Unconfigured means off: nothing says what to trust.
                *self.fleet.lock().unwrap() = None;
                let _ = std::fs::remove_file(&self.paths.fleet);
                self.status.lock().unwrap().fleet_config = None;
                let _applying = self.applying.lock().await;
                self.set_config(None).await;
                self.status.lock().unwrap().update_error = None;
                return;
            }
            Err(e) => {
                self.status.lock().unwrap().update_error = Some(e.to_string());
                self.apply_reporting().await;
                return;
            }
        };

        tracing::info!(%installable, "evaluating");
        self.status.lock().unwrap().updating = true;
        let result = self.evaluate(&installable).await;
        {
            let mut status = self.status.lock().unwrap();
            status.updating = false;
            match &result {
                Ok(()) => {
                    status.last_update = Some(Utc::now());
                    status.update_error = None;
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "update failed");
                    status.update_error = Some(report::nix_error(&format!("{e:#}")));
                }
            }
        }
        // Even when no new fleet config evaluated, the config mode and local
        // config, which a profile or `defaults write` may have changed, apply
        // to the one kept from the last evaluation.
        self.apply_reporting().await;
    }

    /// Apply the kept fleet config, adding any failure to the update error.
    async fn apply_reporting(&self) {
        if let Err(e) = self.apply_fleet().await {
            tracing::warn!(error = %format!("{e:#}"), "could not apply the config");
            let mut status = self.status.lock().unwrap();
            let error = format!("{e:#}");
            status.update_error = Some(match status.update_error.take() {
                Some(update) => format!("{update}\n{error}"),
                None => error,
            });
        }
    }

    async fn evaluate(&self, installable: &str) -> Result<()> {
        let fleet: Fleet = tokio::time::timeout(
            EVAL_TIMEOUT,
            self.nix.eval_json(installable, Some(settings::APPLY)),
        )
        .await
        .context("nix eval timed out")??;
        // Check it renders before keeping it.
        render::render(fleet.settings.clone(), None, &self.paths.key, &self.sshd)?;
        write_atomically(&self.paths.fleet, &serde_json::to_vec(&fleet)?)?;
        *self.fleet.lock().unwrap() = Some(fleet);
        Ok(())
    }

    /// Run the last fleet config that evaluated, with the local config in
    /// place of the fleet's when managing locally. Without one, there is
    /// nothing to run until an evaluation succeeds.
    async fn apply_fleet(&self) -> Result<()> {
        let _applying = self.applying.lock().await;
        let Some(settings) = self
            .fleet
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f.settings.clone())
        else {
            return Ok(());
        };
        let (mode, local) = {
            let status = self.status.lock().unwrap();
            (status.config_mode, status.local_config.clone())
        };
        self.status.lock().unwrap().fleet_config = Some(LocalConfig::from_settings(&settings));
        if let Some(e) = self.config_error.lock().unwrap().clone() {
            bail!("{e}");
        }
        let local = (mode == ConfigMode::Local).then_some(&local);
        // The app's changes are checked as they are made, but a profile's
        // are not; Nebula keeps its config rather than fail on a bad one.
        if let Some(local) = local {
            local
                .validate()
                .context("the local Nebula config is not valid")?;
        }
        let config = render::render(settings, local, &self.paths.key, &self.sshd)?;
        self.set_config(Some(config)).await;
        Ok(())
    }

    /// Make `config` the one Nebula runs, or stop Nebula for `None`, and
    /// restart Nebula only if that changed anything.
    async fn set_config(&self, config: Option<Vec<u8>>) {
        let current = std::fs::read(&self.paths.config).ok();
        if current == config {
            return;
        }
        let written = match &config {
            Some(config) => write_atomically(&self.paths.config, config).and_then(|()| {
                let cert = render::certificate(config).unwrap_or_default();
                write_atomically(&self.paths.cert, cert.as_bytes())
            }),
            None => std::fs::remove_file(&self.paths.config).map_err(Into::into),
        };
        if let Err(e) = written {
            self.status.lock().unwrap().update_error = Some(format!("{e:#}"));
            return;
        }

        let info = match config {
            Some(_) => cert_info(&self.paths).await.ok(),
            None => None,
        };
        {
            let mut status = self.status.lock().unwrap();
            status.address = info.as_ref().and_then(|i| i.address.clone());
            status.cert_expires = info.map(|i| i.not_after);
        }
        self.changes.send_modify(|generation| *generation += 1);
    }
}

/// The fleet config kept from the last evaluation, if it is still readable.
fn load_fleet(path: &Path) -> Option<Fleet> {
    let json = std::fs::read(path).ok()?;
    match serde_json::from_slice(&json) {
        Ok(fleet) => Some(fleet),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "ignoring a saved fleet config that does not parse");
            None
        }
    }
}

fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("could not write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))
}

async fn cert_info(paths: &Paths) -> Result<report::CertInfo> {
    let output = Command::new(&paths.nebula_cert)
        .arg("print")
        .arg("-json")
        .arg("-path")
        .arg(&paths.cert)
        .output()
        .await
        .context("could not run nebula-cert print")?;
    if !output.status.success() {
        bail!(
            "nebula-cert print failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    report::cert_info(&output.stdout)
}

/// Anyone on the Mac may read the status, so the socket is world-writable;
/// changing settings is checked per connection instead.
fn listen() -> Result<UnixListener> {
    let socket = Path::new(SOCKET_PATH);
    let dir = socket.parent().context("socket path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    let _ = std::fs::remove_file(socket);
    let listener =
        UnixListener::bind(socket).with_context(|| format!("could not listen on {SOCKET_PATH}"))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

async fn serve(daemon: Arc<Daemon>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(daemon.clone().handle(stream));
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

impl Daemon {
    async fn handle(self: Arc<Self>, stream: UnixStream) {
        let admin = peer::is_admin(&stream);
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let response = match serde_json::from_str::<Request>(&line) {
                Ok(request) => self.respond(request, admin).await,
                Err(e) => Response::Error(format!("bad request: {e}")),
            };
            let mut out = match serde_json::to_vec(&response) {
                Ok(out) => out,
                Err(_) => return,
            };
            out.push(b'\n');
            if write.write_all(&out).await.is_err() {
                return;
            }
        }
    }

    async fn respond(&self, request: Request, admin: bool) -> Response {
        match request {
            Request::Status => {}
            Request::Mesh => {
                return match self.mesh(admin).await {
                    Ok(mesh) => Response::Mesh(mesh),
                    Err(e) => Response::Error(format!("{e:#}")),
                };
            }
            Request::SetConfig { mode, local } => {
                if let Err(e) = self.set_local_config(mode, local, admin).await {
                    return Response::Error(format!("{e:#}"));
                }
            }
            Request::Refresh => self.refresh.notify_one(),
            Request::SetSettings {
                repository_url,
                branch,
                device,
            } => {
                let settings = settings::Settings {
                    repository_url: Some(repository_url.trim().to_owned()),
                    branch: branch
                        .map(|b| b.trim().to_owned())
                        .filter(|b| !b.is_empty()),
                    device: Some(device.trim().to_owned()),
                };
                if let Err(e) = self.set_settings(settings, admin) {
                    return Response::Error(format!("{e:#}"));
                }
                self.refresh.notify_one();
            }
        }
        Response::Status(Box::new(self.status.lock().unwrap().clone()))
    }

    fn set_settings(&self, settings: settings::Settings, admin: bool) -> Result<()> {
        if !admin {
            bail!("only an administrator can change which fleet this Mac follows");
        }
        if prefs::managed() {
            bail!("these settings are managed by a configuration profile");
        }
        settings.validate()?;
        prefs::store(&settings)?;

        let mut status = self.status.lock().unwrap();
        status.repository_url = settings.repository_url;
        status.branch = settings.branch;
        status.device = settings.device;
        Ok(())
    }

    async fn set_local_config(
        &self,
        mode: ConfigMode,
        local: LocalConfig,
        admin: bool,
    ) -> Result<()> {
        if !admin {
            bail!("only an administrator can change this Mac's Nebula config");
        }
        if prefs::config_mode_managed() {
            bail!("this Mac's Nebula config is managed by a configuration profile");
        }
        local.validate()?;
        prefs::store_config(mode, &local)?;
        // What was stored in place of a value that did not parse does.
        *self.config_error.lock().unwrap() = None;
        {
            let mut status = self.status.lock().unwrap();
            status.config_mode = mode;
            status.local_config = local;
        }
        self.apply_fleet().await
    }

    /// Ask Nebula for its tunnels and name them from the topology.
    async fn mesh(&self, admin: bool) -> Result<Mesh> {
        if !admin {
            bail!("only an administrator can see the mesh, as it shows where every peer is");
        }
        let own_address = {
            let status = self.status.lock().unwrap();
            if !matches!(status.nebula, Nebula::Running { .. }) {
                bail!("Nebula is not running");
            }
            status.address.clone()
        };
        let (hostmap, pending, relays) = tokio::try_join!(
            self.debug.run("list-hostmap -json"),
            self.debug.run("list-pending-hostmap -json"),
            self.debug.run("print-relays"),
        )?;
        let topology = self
            .fleet
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f.topology.clone())
            .unwrap_or_default();
        mesh::assemble(
            mesh::Output {
                hostmap: &hostmap,
                pending: &pending,
                relays: &relays,
            },
            &topology,
            own_address.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Install a new file at `path` the way an update does: write beside it
    /// and rename over it.
    fn reinstall(path: &Path) {
        let new = path.with_extension("new");
        std::fs::write(&new, b"new build").unwrap();
        std::fs::rename(&new, path).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reinstalled_executable_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ogygia-macos-nebulad");
        std::fs::write(&exe, b"old build").unwrap();

        let watch = tokio::spawn(replaced(exe.clone()));
        tokio::time::sleep(REPLACED_CHECK * 2).await;
        assert!(
            !watch.is_finished(),
            "an untouched executable is not an update"
        );

        reinstall(&exe);
        tokio::time::timeout(REPLACED_CHECK * 2, watch)
            .await
            .expect("the update was not noticed")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_update_in_progress_is_waited_out() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("ogygia-macos-nebulad");
        std::fs::write(&exe, b"old build").unwrap();

        let watch = tokio::spawn(replaced(exe.clone()));
        tokio::task::yield_now().await;
        std::fs::remove_file(&exe).unwrap();
        tokio::time::sleep(REPLACED_CHECK * 3).await;
        assert!(
            !watch.is_finished(),
            "a missing executable is not yet an update"
        );

        reinstall(&exe);
        tokio::time::timeout(REPLACED_CHECK * 2, watch)
            .await
            .expect("the finished update was not noticed")
            .unwrap();
    }
}
