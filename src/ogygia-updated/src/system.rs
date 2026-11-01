use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::str;
use std::thread;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use tracing::warn;

use crate::control::Events;
use crate::control::Frame;
use crate::control::Step;

/// A switch-to-configuration action, mirroring nixos-rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    /// Activate now without touching the boot loader.
    Test,
    /// Activate now and make it the boot default.
    Switch,
    /// Make it the boot default for the next boot, without activating now.
    Boot,
}

impl Activation {
    fn arg(self) -> &'static str {
        match self {
            Activation::Test => "test",
            Activation::Switch => "switch",
            Activation::Boot => "boot",
        }
    }
}

/// Side effects of an update cycle, abstracted for testing.
pub trait System {
    /// Announce that the cycle entered `step`, for phases the engine runs
    /// itself; the methods below announce their own.
    fn step(&self, _step: Step) {}
    /// Substitute `flake_ref` from binary caches without building locally.
    fn prefetch(&self, flake_ref: &str) -> Result<()>;
    /// Whether `flake_ref`'s attribute is defined by the flake, checked by
    /// evaluation without building. An attribute added to the flake only
    /// recently is absent from an older commit, so its system cannot be
    /// built there. An absent attribute is distinct from one that fails to
    /// evaluate, which is an error.
    fn attr_defined(&self, flake_ref: &str) -> Result<bool>;
    /// Build `flake_ref` and return its store path.
    fn build(&self, flake_ref: &str) -> Result<PathBuf>;
    /// Point `profile` at `store_path` as a new generation.
    fn set_profile(&self, profile: &Path, store_path: &Path) -> Result<()>;
    /// Run `store_path`'s switch-to-configuration with `action`.
    fn switch_to_configuration(&self, store_path: &Path, action: Activation) -> Result<()>;
    /// Kernel, initrd, and kernel-modules links of a system, used to
    /// detect updates that need a reboot. Missing links resolve to None.
    fn kernel_links(&self, system: &Path) -> [Option<PathBuf>; 3];
    /// Schedule a reboot in `delay_minutes` minutes.
    fn schedule_reboot(&self, delay_minutes: u32) -> Result<()>;
}

/// The host's real side effects. What the commands print goes to `events`
/// as it appears; nix is always run with its `internal-json` log format so
/// a client can follow the build.
pub struct RealSystem<'a> {
    pub events: &'a Events,
}

const EXPERIMENTAL_FEATURES: [&str; 2] = ["--extra-experimental-features", "nix-command flakes"];

const NIX_LOG_FORMAT: [&str; 3] = ["--log-format", "internal-json", "-v"];

impl System for RealSystem<'_> {
    fn step(&self, step: Step) {
        self.events.step(step);
    }

    fn prefetch(&self, flake_ref: &str) -> Result<()> {
        let mut command = Command::new("nix");
        command
            .arg("build")
            .args(EXPERIMENTAL_FEATURES)
            .args(NIX_LOG_FORMAT)
            .args(["--max-jobs", "0"])
            .args(["--option", "always-allow-substitutes", "true"])
            .arg("--no-link")
            .arg(flake_ref);
        self.run(Step::Prefetch, &mut command)
    }

    fn attr_defined(&self, flake_ref: &str) -> Result<bool> {
        // `_: true` selects the attribute without forcing its value, so a
        // present attribute resolves cheaply and an absent one fails at the
        // selection with a message flake attribute resolution reserves for
        // exactly that case.
        let mut command = Command::new("nix");
        command
            .arg("eval")
            .args(EXPERIMENTAL_FEATURES)
            .arg(flake_ref)
            .args(["--apply", "_: true"]);
        info!(?command, "checking attribute is defined");
        let output = command.output().context("running nix eval")?;
        if output.status.success() {
            return Ok(true);
        }
        let stderr = str::from_utf8(&output.stderr).context("decoding nix eval output")?;
        if stderr.contains("does not provide attribute") {
            return Ok(false);
        }
        bail!(
            "nix eval of {flake_ref} failed with {}:\n{stderr}",
            output.status
        );
    }

    fn build(&self, flake_ref: &str) -> Result<PathBuf> {
        self.events.step(Step::Build);
        let mut command = Command::new("nix");
        command
            .arg("build")
            .args(EXPERIMENTAL_FEATURES)
            .args(NIX_LOG_FORMAT)
            .args(["--option", "always-allow-substitutes", "true"])
            .args(["--no-link", "--print-out-paths"])
            .arg(flake_ref);
        // Stdout carries the out paths, not output to show.
        let (status, stdout) = self.stream(Step::Build, &mut command, false)?;
        if !status.success() {
            bail!("nix build {flake_ref} failed with {status}");
        }
        let path = stdout
            .lines()
            .next_back()
            .context("nix build printed no out paths")?;
        Ok(PathBuf::from(path))
    }

    fn set_profile(&self, profile: &Path, store_path: &Path) -> Result<()> {
        self.run(
            Step::SetProfile,
            Command::new("nix-env")
                .arg("-p")
                .arg(profile)
                .arg("--set")
                .arg(store_path),
        )
    }

    fn switch_to_configuration(&self, store_path: &Path, action: Activation) -> Result<()> {
        self.run(
            Step::Activate { action },
            Command::new(store_path.join("bin/switch-to-configuration")).arg(action.arg()),
        )
    }

    fn kernel_links(&self, system: &Path) -> [Option<PathBuf>; 3] {
        ["initrd", "kernel", "kernel-modules"].map(|name| fs::read_link(system.join(name)).ok())
    }

    fn schedule_reboot(&self, delay_minutes: u32) -> Result<()> {
        self.run(
            Step::ScheduleReboot,
            Command::new("shutdown")
                .arg("-r")
                .arg(format!("+{delay_minutes}")),
        )
    }
}

impl RealSystem<'_> {
    /// Announce `step`, then run `command` to success, forwarding all of
    /// its output.
    fn run(&self, step: Step, command: &mut Command) -> Result<()> {
        self.events.step(step);
        let (status, _) = self.stream(step, command, true)?;
        if !status.success() {
            bail!("{:?} failed with {status}", command.get_program());
        }
        Ok(())
    }

    /// Run `command` to completion, forwarding what it writes to the
    /// client and the journal line by line as it appears. Returns its
    /// status and stdout; `forward_stdout` is whether stdout is also
    /// output to show, rather than data for the caller.
    fn stream(
        &self,
        step: Step,
        command: &mut Command,
        forward_stdout: bool,
    ) -> Result<(ExitStatus, String)> {
        info!(?command, ?step, "running");
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {:?}", command.get_program()))?;
        let stdout = child.stdout.take().context("child stdout not piped")?;
        let stderr = child.stderr.take().context("child stderr not piped")?;

        // Both pipes are drained at once so a full one never blocks the
        // child.
        let stdout = thread::scope(|scope| {
            let stdout = scope.spawn(move || {
                let mut collected = String::new();
                for_each_line(stdout, |line| {
                    if forward_stdout {
                        self.forward(step, line);
                    }
                    collected.push_str(line);
                    collected.push('\n');
                });
                collected
            });
            for_each_line(stderr, |line| self.forward(step, line));
            stdout.join()
        })
        .map_err(|_| anyhow!("reading child stdout panicked"))?;

        let status = child
            .wait()
            .with_context(|| format!("waiting for {:?}", command.get_program()))?;
        Ok((status, stdout))
    }

    /// Send one line of child output to the client, and note it in the
    /// journal. Nix log events are parsed so the client gets them as data.
    fn forward(&self, step: Step, line: &str) {
        let event = line
            .strip_prefix("@nix ")
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok());
        match event {
            Some(event) => {
                log_nix_event(&event);
                self.events.emit(Frame::Nix { event });
            }
            None => {
                info!(?step, line, "output");
                self.events.emit(Frame::Output {
                    step,
                    line: line.to_string(),
                });
            }
        }
    }
}

/// Note a nix event in the journal. The client sees every event, so the
/// journal keeps only what an operator reading it afterwards needs —
/// errors and warnings — and the rest at levels it normally hides.
fn log_nix_event(event: &serde_json::Value) {
    let action = event["action"].as_str().unwrap_or_default();
    match action {
        // Nix reports real errors and warnings with a `raw_msg`, the
        // message without terminal colours; other messages can share their
        // levels (nix logs "linking ..." at error level), so the level
        // alone does not make one a problem.
        "msg" => match (event["raw_msg"].as_str(), event["level"].as_u64()) {
            (Some(msg), Some(0)) => error!(msg = strip_colours(msg), "nix"),
            (Some(msg), Some(1)) => warn!(msg = strip_colours(msg), "nix"),
            _ => debug!(msg = event["msg"].as_str().unwrap_or_default(), "nix"),
        },
        // resBuildLogLine, resPostBuildLogLine
        "result" if matches!(event["type"].as_u64(), Some(101 | 107)) => {
            debug!(
                log = event["fields"][0].as_str().unwrap_or_default(),
                "nix build log"
            );
        }
        _ => trace!(%event, "nix event"),
    }
}

/// `text` without ANSI colour sequences, which nix puts in its messages
/// whatever its output is, and which read as noise in the journal.
fn strip_colours(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            stripped.push(c);
        } else if chars.next() == Some('[') {
            // Skip to the final byte of the sequence.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    stripped
}

/// Call `f` with each line of `reader`, without its terminator.
fn for_each_line(reader: impl Read, mut f: impl FnMut(&str)) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    while matches!(reader.read_until(b'\n', &mut buf), Ok(read) if read > 0) {
        f(String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']));
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use serde_json::json;

    use super::*;
    use crate::control::Failure;

    #[test]
    fn child_output_streams_as_nix_events_and_plain_lines() {
        let (server, client) = UnixStream::pair().unwrap();
        let events = Events::attach(server);
        let system = RealSystem { events: &events };

        let (status, stdout) = system
            .stream(
                Step::Build,
                Command::new("sh").args([
                    "-c",
                    r#"echo '@nix {"action":"msg","level":3,"msg":"hi"}' >&2
                       echo 'error: not json' >&2
                       echo '@nix {broken' >&2
                       echo /nix/store/out"#,
                ]),
                false,
            )
            .unwrap();
        assert!(status.success());
        // Stdout is data for the caller, not output to show.
        assert_eq!(stdout, "/nix/store/out\n");

        events.finish(Err(Failure::new("done")));
        let frames: Vec<Frame> = BufReader::new(client)
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect();
        let output = |line: &str| Frame::Output {
            step: Step::Build,
            line: line.to_string(),
        };
        assert_eq!(
            frames,
            vec![
                Frame::Nix {
                    event: json!({"action": "msg", "level": 3, "msg": "hi"}),
                },
                output("error: not json"),
                output("@nix {broken"),
                Frame::Finished {
                    result: Err(Failure::new("done")),
                },
            ]
        );
    }

    #[test]
    fn colours_are_stripped_for_the_journal() {
        assert_eq!(
            strip_colours("Cannot build '\u{1b}[35;1m/nix/store/x.drv\u{1b}[0m'.\n"),
            "Cannot build '/nix/store/x.drv'.\n"
        );
        assert_eq!(strip_colours("plain"), "plain");
    }
    #[test]
    fn plain_commands_forward_stdout_too() {
        let (server, client) = UnixStream::pair().unwrap();
        let events = Events::attach(server);
        let system = RealSystem { events: &events };

        system
            .run(
                Step::SetProfile,
                Command::new("sh").args(["-c", "echo hello"]),
            )
            .unwrap();
        let error = system
            .run(Step::SetProfile, Command::new("sh").args(["-c", "exit 3"]))
            .unwrap_err();
        assert!(error.to_string().contains("failed"));

        events.finish(Err(Failure::new("done")));
        let frames: Vec<Frame> = BufReader::new(client)
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect();
        assert_eq!(
            frames[0],
            Frame::Step {
                step: Step::SetProfile
            }
        );
        assert_eq!(
            frames[1],
            Frame::Output {
                step: Step::SetProfile,
                line: "hello".to_string(),
            }
        );
    }
}
