//! Control socket for triggering update cycles in the running daemon.
//!
//! The daemon is the sole owner of the repository clone, the system
//! profile, and the update schedule; manual updates ask it to run a
//! cycle now rather than running the engine themselves. Authorization
//! is delegated to filesystem permissions on the socket.

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use crate::canary::CanaryState;
use crate::engine::Outcome;

pub const DEFAULT_SOCKET_PATH: &str = "/run/ogygia-updated/control.sock";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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

/// The client's side of one connection: a request the daemon has been
/// sent, with its answer still to read.
pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    /// Send `request` to the daemon listening on `socket_path`. The
    /// connection is closed for writing afterwards, so holding it lets
    /// nothing further reach the daemon. Errors if the daemon cannot be
    /// reached.
    pub fn send(socket_path: &Path, request: &Request) -> Result<Self> {
        let mut stream = UnixStream::connect(socket_path).with_context(|| {
            format!(
                "connecting to {}; is ogygia-updated running, and are you root?",
                socket_path.display()
            )
        })?;

        let mut payload = serde_json::to_vec(request)?;
        payload.push(b'\n');
        stream.write_all(&payload).context("sending request")?;
        stream
            .shutdown(Shutdown::Write)
            .context("closing the request")?;
        Ok(Connection { stream })
    }

    /// Wait for the daemon's reply. Errors with its failure if it reports
    /// one, or if it hangs up without answering.
    pub fn receive(self) -> Result<Reply> {
        // No read timeout: a cycle legitimately takes as long as a build.
        let mut line = String::new();
        BufReader::new(&self.stream)
            .read_line(&mut line)
            .context("reading daemon response")?;
        let result: Result<Reply, Failure> =
            serde_json::from_str(&line).context("parsing daemon response")?;
        result.map_err(Failure::into_error)
    }
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

/// Accept connections, parse each request, and hand it with its stream to
/// the daemon loop, which answers once it has served the command. Invalid
/// requests are answered immediately without reaching the loop.
pub fn listen(listener: UnixListener, requests: mpsc::Sender<(Request, UnixStream)>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));

        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Request>(&line) else {
            respond(&mut stream, Err(Failure::new("invalid request")));
            continue;
        };

        if requests.send((request, stream)).is_err() {
            return;
        }
    }
}

/// Answer a trigger connection. Failures only lose the reply, never the
/// update cycle, so they are logged and swallowed.
pub fn respond(stream: &mut UnixStream, response: Result<Reply, Failure>) {
    let mut payload = match serde_json::to_vec(&response) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(%error, "failed to encode control response");
            return;
        }
    };
    payload.push(b'\n');
    if let Err(error) = stream.write_all(&payload) {
        warn!(%error, "failed to answer on the control socket");
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::thread;

    use super::*;

    #[test]
    fn requests_round_trip_with_their_replies() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket_path = dir.path().join("control.sock");
        let listener = bind(&socket_path).unwrap();

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || listen(listener, sender));
        let server = thread::spawn(move || {
            let (request, mut stream) = receiver.recv().unwrap();
            assert_eq!(request, Request::Update);
            respond(
                &mut stream,
                Ok(Reply::Cycle {
                    outcome: Outcome::Switched {
                        commit: "abc".to_string(),
                    },
                }),
            );

            let (request, mut stream) = receiver.recv().unwrap();
            assert_eq!(
                request,
                Request::Canary {
                    branch: "feature".to_string(),
                    timeout: Some(3600),
                    persist: true,
                }
            );
            respond(
                &mut stream,
                Err(Failure {
                    chain: vec!["starting canary".to_string(), "no such branch".to_string()],
                }),
            );

            let (request, mut stream) = receiver.recv().unwrap();
            assert_eq!(request, Request::CanaryStatus);
            respond(
                &mut stream,
                Ok(Reply::CanaryStatus {
                    state: None,
                    running: "abc".to_string(),
                    next_boot: "abc".to_string(),
                }),
            );

            let (request, mut stream) = receiver.recv().unwrap();
            assert_eq!(request, Request::Tick);
            respond(
                &mut stream,
                Ok(Reply::Cycle {
                    outcome: Outcome::CanaryHeld {
                        commit: "def".to_string(),
                        next_boot: "abc".to_string(),
                    },
                }),
            );
        });

        let ask = |request| Connection::send(&socket_path, &request)?.receive();
        assert_eq!(
            ask(Request::Update).unwrap(),
            Reply::Cycle {
                outcome: Outcome::Switched {
                    commit: "abc".to_string()
                }
            }
        );
        let error = ask(Request::Canary {
            branch: "feature".to_string(),
            timeout: Some(3600),
            persist: true,
        })
        .unwrap_err();
        // The chain of causes survives the socket.
        assert_eq!(format!("{error:#}"), "starting canary: no such branch");
        assert!(matches!(
            ask(Request::CanaryStatus).unwrap(),
            Reply::CanaryStatus { state: None, .. }
        ));
        assert!(matches!(
            ask(Request::Tick).unwrap(),
            Reply::Cycle {
                outcome: Outcome::CanaryHeld { .. }
            }
        ));
        server.join().unwrap();
    }

    #[test]
    fn a_sent_request_is_all_the_daemon_can_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket_path = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let connection = Connection::send(&socket_path, &Request::Tick).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let mut received = String::new();
        stream.read_to_string(&mut received).unwrap();
        assert_eq!(received, "{\"command\":\"tick\"}\n");
        drop(connection);
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
        let response: Result<Reply, Failure> = serde_json::from_str(&line).unwrap();
        assert!(response.is_err());

        // The garbage request never reached the update loop.
        assert!(receiver.try_recv().is_err());
    }
}
