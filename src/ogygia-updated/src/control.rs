//! Control socket for triggering update cycles in the running daemon.
//!
//! The daemon is the sole owner of the repository clone, the system
//! profile, and the update schedule; manual updates ask it to run a
//! cycle now rather than running the engine themselves. Authorization
//! is delegated to filesystem permissions on the socket.
//!
//! A request is one JSON line. The daemon answers with a stream of JSON
//! [`Frame`]s, one per line, ending in exactly one [`Frame::Finished`]:
//! progress while the cycle runs, then its typed result. The daemon sends
//! data and never prose; presenting it is up to the client.

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::canary::CanaryState;
use crate::engine::Outcome;
use crate::system::Activation;

pub const DEFAULT_SOCKET_PATH: &str = "/run/ogygia-updated/control.sock";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a write to a client may stall before it is treated as gone.
/// The daemon waits for the writer to drain before it exits after an
/// activation, so a client that stops reading must not hold it forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// A command for the daemon, sent over the control socket. Each variant
/// carries only the data its operation needs.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    /// Reset the target to the configured branch tip and run a cycle now,
    /// clearing any active canary.
    Update,
    /// Run one cycle now on the daemon's own terms — an active canary is
    /// held, and a deliberately off-branch system is left alone — and
    /// answer once it has been served. Callers that must not fall behind
    /// use this to force a cycle and confirm it happened.
    Tick,
    /// Trial `branch`: pin its tip and hold the host there until `timeout`
    /// seconds pass (no timeout if `None`), the commit merges, or an
    /// update supersedes it. `persist` also makes it the boot default, so
    /// a reboot does not end it.
    Canary {
        branch: String,
        timeout: Option<u64>,
        persist: bool,
    },
    /// Report the current or most-recent canary without running a cycle.
    CanaryStatus,
}

/// A phase of an update cycle, announced before the work in it starts and
/// attached to the output it produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Fetching the tracked repository.
    Fetch,
    /// Substituting the cache-warm closure.
    Prefetch,
    /// Building the system.
    Build,
    /// Pointing the system profile at the new system.
    SetProfile,
    /// Running the new system's switch-to-configuration.
    Activate { action: Activation },
    /// Scheduling the reboot a new kernel needs.
    ScheduleReboot,
}

/// One message of a response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// The cycle entered `step`.
    Step { step: Step },
    /// A nix `internal-json` log event, parsed from the line after its
    /// `@nix ` prefix. Sent during `Prefetch` and `Build`, and consumable
    /// as-is by tools such as nom.
    Nix { event: serde_json::Value },
    /// A line a child process wrote that is not a nix event, such as
    /// switch-to-configuration's output or an error nix printed outside
    /// its log format.
    Output { step: Step, line: String },
    /// The last frame of a response.
    Finished { result: Result<Reply, Failure> },
    /// A frame from a newer daemon, which an older client skips.
    #[serde(other)]
    Unknown,
}

/// The successful result of a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// How the update cycle that was run ended. Whether that counts as
    /// the caller's request being served is [`Outcome::served`].
    Cycle { outcome: Outcome },
    /// The current or most-recent canary, if there has been one, and the
    /// commits the host currently runs and will next boot.
    CanaryStatus {
        state: Option<CanaryState>,
        running: String,
        next_boot: String,
    },
}

/// Why a request failed: the error and its causes, outermost first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub chain: Vec<String>,
}

impl Failure {
    pub fn new(message: impl Into<String>) -> Self {
        Failure {
            chain: vec![message.into()],
        }
    }

    /// Rebuild an error with the original chain of causes.
    pub fn into_error(self) -> anyhow::Error {
        let mut messages = self.chain.into_iter().rev();
        let root = messages.next().unwrap_or_else(|| "unknown error".into());
        messages.fold(anyhow::anyhow!(root), |error, message| {
            error.context(message)
        })
    }
}

impl From<&anyhow::Error> for Failure {
    fn from(error: &anyhow::Error) -> Self {
        Failure {
            chain: error.chain().map(ToString::to_string).collect(),
        }
    }
}

/// The daemon's side of one connection: where a cycle sends its progress
/// and its result. Frames queue without bound for a writer thread, so a
/// slow client never holds up the cycle, and a client that disconnects
/// only loses the frames, never the cycle.
pub struct Events {
    sender: Option<mpsc::Sender<Frame>>,
    writer: Option<JoinHandle<()>>,
}

impl Events {
    /// A cycle with no client, such as one the daemon's interval started.
    pub fn none() -> Self {
        Events {
            sender: None,
            writer: None,
        }
    }

    pub(crate) fn attach(stream: UnixStream) -> Self {
        let (sender, receiver) = mpsc::channel();
        Events {
            sender: Some(sender),
            writer: Some(thread::spawn(move || write_frames(stream, receiver))),
        }
    }

    pub fn emit(&self, frame: Frame) {
        if let Some(sender) = &self.sender {
            // A closed receiver is a client that went away.
            let _ = sender.send(frame);
        }
    }

    /// Announce that the cycle entered `step`.
    pub fn step(&self, step: Step) {
        info!(?step, "step");
        self.emit(Frame::Step { step });
    }

    /// Send the final frame and wait for the client's queue to drain, so
    /// the result is written even if the daemon exits right after.
    pub fn finish(mut self, result: Result<Reply, Failure>) {
        self.emit(Frame::Finished { result });
        self.sender = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn write_frames(mut stream: UnixStream, frames: mpsc::Receiver<Frame>) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    for frame in frames {
        let mut payload = match serde_json::to_vec(&frame) {
            Ok(payload) => payload,
            Err(error) => {
                warn!(%error, "failed to encode control frame");
                continue;
            }
        };
        payload.push(b'\n');
        if let Err(error) = stream.write_all(&payload) {
            // Returning drops the receiver, which makes every later
            // emit a no-op.
            debug!(%error, "control client went away");
            return;
        }
    }
}

/// Send `request` to the daemon listening on `socket_path`, calling
/// `on_frame` with every frame as it arrives, the final one included.
/// Returns the daemon's reply, or its failure as an error. Errors if the
/// daemon cannot be reached or hangs up before finishing.
pub fn send(
    socket_path: &Path,
    request: &Request,
    mut on_frame: impl FnMut(&Frame),
) -> Result<Reply> {
    let mut stream = UnixStream::connect(socket_path).with_context(|| {
        format!(
            "connecting to {}; is ogygia-updated running, and are you root?",
            socket_path.display()
        )
    })?;

    let mut payload = serde_json::to_vec(request)?;
    payload.push(b'\n');
    stream.write_all(&payload).context("sending request")?;

    // No read timeout: a cycle legitimately takes as long as a build.
    for line in BufReader::new(&stream).lines() {
        let line = line.context("reading daemon response")?;
        let frame: Frame = serde_json::from_str(&line).context("parsing daemon response")?;
        on_frame(&frame);
        if let Frame::Finished { result } = frame {
            return result.map_err(Failure::into_error);
        }
    }
    bail!("daemon closed the connection without finishing")
}

/// Bind the control socket, replacing a stale one from a previous run
/// and restricting it to root.
pub fn bind(socket_path: &Path) -> Result<UnixListener> {
    if socket_path.exists() {
        fs::remove_file(socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding control socket {}", socket_path.display()))?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))
        .context("restricting control socket permissions")?;
    Ok(listener)
}

/// Accept connections, parse each request, and hand it with its [`Events`]
/// to the daemon loop, which streams progress and finishes it once it has
/// served the command. Invalid requests are failed immediately without
/// reaching the loop.
pub fn listen(listener: UnixListener, requests: mpsc::Sender<(Request, Events)>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));

        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }
        let events = Events::attach(stream);
        let Ok(request) = serde_json::from_str::<Request>(&line) else {
            events.finish(Err(Failure::new("invalid request")));
            continue;
        };

        if requests.send((request, events)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_round_trip_with_their_frames_and_reply() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket_path = dir.path().join("control.sock");
        let listener = bind(&socket_path).unwrap();

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || listen(listener, sender));
        let server = thread::spawn(move || {
            let (request, events) = receiver.recv().unwrap();
            assert_eq!(request, Request::Update);
            events.step(Step::Build);
            events.emit(Frame::Nix {
                event: json!({"action": "msg", "level": 3, "msg": "building"}),
            });
            events.emit(Frame::Output {
                step: Step::Activate {
                    action: Activation::Switch,
                },
                line: "activating".to_string(),
            });
            events.finish(Ok(Reply::Cycle {
                outcome: Outcome::Switched {
                    commit: "abc".to_string(),
                },
            }));

            let (request, events) = receiver.recv().unwrap();
            assert_eq!(
                request,
                Request::Canary {
                    branch: "feature".to_string(),
                    timeout: Some(3600),
                    persist: true,
                }
            );
            events.finish(Err(Failure {
                chain: vec!["starting canary".to_string(), "no such branch".to_string()],
            }));

            let (request, events) = receiver.recv().unwrap();
            assert_eq!(request, Request::CanaryStatus);
            events.finish(Ok(Reply::CanaryStatus {
                state: None,
                running: "abc".to_string(),
                next_boot: "abc".to_string(),
            }));
        });

        let mut frames = Vec::new();
        let reply = send(&socket_path, &Request::Update, |frame| {
            frames.push(frame.clone())
        })
        .unwrap();
        let switched = Reply::Cycle {
            outcome: Outcome::Switched {
                commit: "abc".to_string(),
            },
        };
        assert_eq!(reply, switched);
        assert_eq!(
            frames,
            vec![
                Frame::Step { step: Step::Build },
                Frame::Nix {
                    event: json!({"action": "msg", "level": 3, "msg": "building"}),
                },
                Frame::Output {
                    step: Step::Activate {
                        action: Activation::Switch
                    },
                    line: "activating".to_string(),
                },
                Frame::Finished {
                    result: Ok(switched)
                },
            ]
        );

        // The chain of causes survives the socket.
        let request = Request::Canary {
            branch: "feature".to_string(),
            timeout: Some(3600),
            persist: true,
        };
        let error = send(&socket_path, &request, |_| {}).unwrap_err();
        assert_eq!(format!("{error:#}"), "starting canary: no such branch");

        let reply = send(&socket_path, &Request::CanaryStatus, |_| {}).unwrap();
        assert!(matches!(reply, Reply::CanaryStatus { state: None, .. }));
        server.join().unwrap();
    }

    #[test]
    fn frames_have_a_readable_wire_format() {
        let step = Frame::Step {
            step: Step::Activate {
                action: Activation::Boot,
            },
        };
        assert_eq!(
            serde_json::to_value(&step).unwrap(),
            json!({"type": "step", "step": {"activate": {"action": "boot"}}})
        );

        let finished = Frame::Finished {
            result: Ok(Reply::Cycle {
                outcome: Outcome::UpToDate,
            }),
        };
        assert_eq!(
            serde_json::to_value(&finished).unwrap(),
            json!({
                "type": "finished",
                "result": {"Ok": {"reply": "cycle", "outcome": {"kind": "up_to_date"}}},
            })
        );
    }

    #[test]
    fn frames_from_a_newer_daemon_are_skipped_not_fatal() {
        let frame: Frame = serde_json::from_str(r#"{"type":"progress","percent":50}"#).unwrap();
        assert_eq!(frame, Frame::Unknown);
    }

    #[test]
    fn a_disconnected_client_does_not_stop_the_cycle() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket_path = dir.path().join("control.sock");
        let listener = bind(&socket_path).unwrap();

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || listen(listener, sender));

        // The client hangs up straight after asking.
        let mut stream = UnixStream::connect(&socket_path).unwrap();
        stream.write_all(b"{\"command\":\"update\"}\n").unwrap();
        drop(stream);

        let (_, events) = receiver.recv().unwrap();
        for _ in 0..1000 {
            events.step(Step::Build);
        }
        events.finish(Ok(Reply::Cycle {
            outcome: Outcome::UpToDate,
        }));
    }

    #[test]
    fn stale_sockets_are_replaced_and_garbage_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket_path = dir.path().join("control.sock");
        drop(bind(&socket_path).unwrap());

        // A leftover socket file from a previous run is replaced.
        let listener = bind(&socket_path).unwrap();

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || listen(listener, sender));

        let mut stream = UnixStream::connect(&socket_path).unwrap();
        stream.write_all(b"not json\n").unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let frame: Frame = serde_json::from_str(&line).unwrap();
        assert!(matches!(frame, Frame::Finished { result: Err(_) }));

        // The garbage request never reached the update loop.
        assert!(receiver.try_recv().is_err());
    }
}
