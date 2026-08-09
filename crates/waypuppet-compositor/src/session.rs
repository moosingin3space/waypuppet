//! A session: a compositor, the process it hosts, and the RPC server that
//! exposes both.
//!
//! Three things run at once. The compositor owns Wayland state on its own
//! thread and calloop drives it. The application under test is a child process.
//! The RPC server runs on tokio and answers callers, reaching the compositor
//! only through [`crate::bridge`].
//!
//! # Nothing waits forever
//!
//! Every reply the server waits for is bounded, and every bound produces an
//! error that says which kind of failure it was — see [`crate::bridge`]. That is
//! the standing rule here: waypuppet cannot promise never to hang, so it
//! promises to explain.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use tarpc::{
    context::Context,
    server::{BaseChannel, Channel as _},
    tokio_serde::formats::Bincode,
};
use tokio::sync::Notify;
use waypuppet_proto::{
    Button, Capture, ClientState, Condition, Cursor, Failure, LogChunk, Node, Point, Selection,
    SessionStatus, Settled, Size, Target, Waypuppet,
};

use crate::{Command, Commands, Compositor, Config, Error, Watching, a11y, a11y::Accessibility};

/// How long the server waits for the compositor to answer a command.
///
/// Generous enough for software rendering of a large output, short enough that
/// a wedged loop is reported while somebody is still watching.
const COMPOSITOR_DEADLINE: Duration = Duration::from_secs(15);

/// The longest a `logs` long poll is held open before answering emptily.
const LOG_POLL: Duration = Duration::from_secs(5);

/// How long a mutating verb waits for the client to draw before giving up.
///
/// Shorter than [`COMPOSITOR_DEADLINE`], because the answer here is about the
/// application rather than about waypuppet: a client that has not repainted in
/// two seconds of software rendering is telling the caller something, and
/// [`Failure::NotSettled`] says it faster than a long wait would.
const SETTLE_DEADLINE: Duration = Duration::from_secs(2);

/// Where a named session's control socket lives.
///
/// # Errors
///
/// Returns [`Error::Compositor`] if `XDG_RUNTIME_DIR` is not set, which means
/// there is nowhere private to put a socket.
pub fn socket_path(name: &str) -> Result<PathBuf, Error> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| Error::Compositor("XDG_RUNTIME_DIR is not set".to_owned()))?;
    Ok(PathBuf::from(base)
        .join("waypuppet")
        .join(format!("{name}.sock")))
}

/// Everything the RPC handlers share.
struct Inner {
    commands: Commands,
    /// The accessibility bus and registry, or why there is not one.
    ///
    /// A session whose bus could not start still serves the whole protocol; it
    /// reports that at startup rather than at the first `tree`, which is the
    /// same bargain the renderer makes.
    a11y: Result<Accessibility, String>,
    capture_dir: PathBuf,
    /// Counts captures, so unnamed ones get distinct file names.
    captures: AtomicU64,
    client: Mutex<Life>,
    logs: Mutex<Logs>,
    /// Woken when logs grow or the child exits, so a long poll can return early.
    activity: Notify,
    /// Woken when the session should shut down.
    shutdown: Notify,
}

/// What is known about the spawned process.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Life {
    /// This session hosts no child of its own — `serve` with nothing to run.
    #[default]
    None,
    /// Spawned, and still going.
    Running,
    /// Gone. `None` means a signal killed it.
    Exited(Option<i32>),
}

/// Output captured from the child.
#[derive(Debug, Default)]
struct Logs {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Lock, tolerating poisoning: a panicking task should not take the session
/// with it when the data is this simple.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run a session until it is asked to stop.
///
/// Starts the compositor, spawns `argv` against it if given, and serves the
/// control protocol on `socket`.
///
/// # Errors
///
/// Returns [`Error::Compositor`] if the compositor, the socket or the child
/// cannot be started. On success, yields the child's exit code where there was
/// a child and it exited normally.
pub async fn serve(
    config: &Config,
    socket: &Path,
    argv: &[String],
    exit_with_child: bool,
) -> Result<Option<i32>, Error> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let config_for_thread = config.clone();

    // The compositor is not `Send`, so it is built on the thread that runs it
    // and only its socket name and command channel come back.
    let compositor_thread = std::thread::Builder::new()
        .name("waypuppet-compositor".to_owned())
        .spawn(move || {
            let mut compositor = match Compositor::new(&config_for_thread) {
                Ok(compositor) => compositor,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            let ready = (compositor.socket_name().to_owned(), compositor.commands());
            if ready_tx.send(Ok(ready)).is_err() {
                return;
            }
            if let Err(error) = compositor.run() {
                tracing::error!(%error, "the compositor loop stopped");
            }
            // Falling out of here drops every `oneshot::Sender` the compositor
            // was holding, which is what makes outstanding calls fail promptly
            // instead of sitting on their deadlines.
        })
        .map_err(|error| Error::Compositor(error.to_string()))?;

    let (wayland_display, commands) =
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| {
                Error::Compositor("the compositor thread never reported ready".to_owned())
            })??;

    // Before the child, always: an application looks for the accessibility bus
    // as it starts, and one that appears afterwards may as well not exist.
    let a11y = match a11y::runtime_dir() {
        Ok(directory) => Accessibility::start(&directory).await,
        Err(why) => Err(why),
    };
    if let Err(why) = &a11y {
        tracing::warn!(%why, "no accessibility bus: `tree` will not work");
    }

    let inner = Arc::new(Inner {
        commands,
        a11y,
        capture_dir: config.capture_dir.clone(),
        captures: AtomicU64::new(0),
        client: Mutex::new(Life::default()),
        logs: Mutex::new(Logs::default()),
        activity: Notify::new(),
        shutdown: Notify::new(),
    });

    if !argv.is_empty() {
        spawn_child(&inner, argv, &wayland_display, exit_with_child)?;
    }
    // Dropped before the compositor thread is joined, so the bus and any
    // daemon behind it go with the session rather than outliving it.

    let result = accept(&inner, socket).await;

    // Stop the compositor and let the socket go, whatever happened.
    let _ = inner.commands.send(Command::Stop);
    let _ = std::fs::remove_file(socket);
    let _ = compositor_thread.join();

    let exit = match *lock(&inner.client) {
        Life::Exited(code) => code,
        Life::None | Life::Running => None,
    };
    result.map(|()| exit)
}

/// Serve the control socket until told to shut down.
async fn accept(inner: &Arc<Inner>, socket: &Path) -> Result<(), Error> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::Compositor(error.to_string()))?;
    }
    reclaim(socket).await;

    let mut listener = tarpc::serde_transport::unix::listen(socket, Bincode::default)
        .await
        .map_err(|error| Error::Compositor(format!("{}: {error}", socket.display())))?;
    listener.config_mut().max_frame_length(usize::MAX);

    tracing::info!(socket = %socket.display(), "control socket listening");

    let serving = async {
        while let Some(Ok(transport)) = listener.next().await {
            let server = Server(Arc::clone(inner));
            tokio::spawn(
                BaseChannel::with_defaults(transport)
                    .execute(server.serve())
                    .for_each(|response| async move {
                        tokio::spawn(response);
                    }),
            );
        }
    };

    tokio::select! {
        () = serving => {}
        () = inner.shutdown.notified() => {}
    }
    Ok(())
}

/// Remove a socket file whose listener is gone.
///
/// A leftover socket from a crashed session is indistinguishable from a live
/// one by looking at the filesystem, so ask it: a connection refused means
/// nobody is listening and the path is ours to take.
async fn reclaim(socket: &Path) {
    if !socket.exists() {
        return;
    }
    if tokio::net::UnixStream::connect(socket).await.is_ok() {
        tracing::warn!(socket = %socket.display(), "a session is already listening here");
    } else {
        tracing::debug!(socket = %socket.display(), "reclaiming a stale socket");
        let _ = std::fs::remove_file(socket);
    }
}

/// Start the application under test.
fn spawn_child(
    inner: &Arc<Inner>,
    argv: &[String],
    wayland_display: &str,
    exit_with_child: bool,
) -> Result<(), Error> {
    let mut command = tokio::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env("WAYLAND_DISPLAY", wayland_display)
        // Never inherit these: a client that picked up the caller's real
        // desktop would open a window there and look like it had worked, which
        // is the worst available failure. The same reasoning covers the bus —
        // an application that found the developer's session would announce
        // itself to their actual desktop.
        .env_remove("DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .stdin(Stdio::null())
        // Captured rather than inherited. Losing the client's last words is
        // precisely the failure waypuppet exists to fix.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Both variables, not just the session one. `AT_SPI_BUS_ADDRESS` is checked
    // first by every implementation that reads it, which saves a lookup and —
    // more usefully — means one bus can do both jobs.
    if let Ok(a11y) = &inner.a11y {
        command
            .env("DBUS_SESSION_BUS_ADDRESS", a11y.address())
            .env("AT_SPI_BUS_ADDRESS", a11y.address());
    }

    let mut child = command
        .spawn()
        .map_err(|error| Error::Compositor(format!("{}: {error}", argv[0])))?;

    *lock(&inner.client) = Life::Running;

    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(drain(Arc::clone(inner), stdout, Stream::Stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(drain(Arc::clone(inner), stderr, Stream::Stderr));
    }

    tokio::spawn({
        let inner = Arc::clone(inner);
        async move {
            let started = Instant::now();
            let status = child.wait().await;
            let code = status.ok().and_then(|status| status.code());
            *lock(&inner.client) = Life::Exited(code);
            // Wake every long poll: a follower needs to stop, not keep waiting
            // for output that will never come.
            inner.activity.notify_waiters();

            if exit_with_child {
                report_exit(&inner, code, started.elapsed()).await;
                let _ = inner.commands.send(Command::Stop);
                inner.shutdown.notify_waiters();
            }
        }
    });

    Ok(())
}

/// Explain what happened to a client that has exited.
///
/// This is the diagnostic waypuppet exists for. A process that dies during
/// startup otherwise leaves nothing behind but an exit code, which is exactly
/// the situation that made this project necessary — so say what was seen, and
/// hand back what the process managed to write.
async fn report_exit(inner: &Arc<Inner>, code: Option<i32>, lifetime: Duration) {
    let mapped = inner
        .commands
        .ask(COMPOSITOR_DEADLINE, Command::Status)
        .await
        .map(|snapshot| snapshot.windows.iter().filter(|w| w.mapped).count())
        .unwrap_or_default();

    let outcome = match code {
        Some(code) => format!("exited {code}"),
        None => "was killed by a signal".to_owned(),
    };
    let windows = match mapped {
        0 => "without mapping a window".to_owned(),
        1 => "having mapped 1 window".to_owned(),
        n => format!("having mapped {n} windows"),
    };
    eprintln!(
        "session: client {outcome} after {}ms {windows}",
        lifetime.as_millis()
    );

    // Only worth printing when something went wrong; a clean run that drew
    // something does not need its output repeated back.
    if code != Some(0) || mapped == 0 {
        let logs = lock(&inner.logs);
        for (name, bytes) in [("stdout", &logs.stdout), ("stderr", &logs.stderr)] {
            if !bytes.is_empty() {
                eprintln!("--- {name} ---");
                eprint!("{}", String::from_utf8_lossy(bytes));
            }
        }
    }
}

/// Which of the child's streams a reader is draining.
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Accumulate one of the child's streams into the log buffer.
async fn drain<R>(inner: Arc<Inner>, mut reader: R, stream: Stream)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let mut buffer = [0_u8; 8192];
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        {
            let mut logs = lock(&inner.logs);
            match stream {
                Stream::Stdout => logs.stdout.extend_from_slice(&buffer[..read]),
                Stream::Stderr => logs.stderr.extend_from_slice(&buffer[..read]),
            }
        }
        inner.activity.notify_waiters();
    }
}

/// The RPC server. One per connection, all sharing [`Inner`].
#[derive(Clone)]
struct Server(Arc<Inner>);

impl Server {
    /// How the child is doing, in protocol terms.
    fn client_state(&self, connections: usize) -> ClientState {
        match *lock(&self.0.client) {
            Life::Exited(code) => ClientState::Exited { code },
            // A session hosting nothing has nothing to be starting.
            Life::None => ClientState::Running,
            // Spawned is not the same as connected: a process that started but
            // never reached the compositor is a different problem from one that
            // connected and then crashed, and `Starting` is how that shows.
            Life::Running if connections > 0 => ClientState::Running,
            Life::Running => ClientState::Starting,
        }
    }

    /// Where an unnamed capture should be written.
    fn capture_path(&self) -> PathBuf {
        let n = self.0.captures.fetch_add(1, Ordering::Relaxed);
        self.0.capture_dir.join(format!("waypuppet-{n:04}.png"))
    }

    /// Block until the child exits, which only this thread can see.
    ///
    /// The compositor's registry cannot answer this one: a process may exit
    /// without ever having connected, so there is no Wayland event to hang it
    /// off. [`Inner::activity`] is already woken when the child goes, for the
    /// benefit of `logs`, and serves here for the same reason.
    async fn wait_for_exit(&self, timeout: Duration) -> Result<Settled, Failure> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Registered *before* the check, or an exit landing between the two
            // would go unnoticed until the deadline.
            let waiting = self.0.activity.notified();

            match *lock(&self.0.client) {
                Life::Exited(_) => return Ok(Settled::Met),
                // Nothing was ever spawned, so nothing can ever exit. Parking
                // here would be failure mode B — waiting on the impossible —
                // and a caller who asked this of a childless session has made a
                // mistake worth hearing about now. Not `ClientGone`: telling
                // somebody whose application never started that it has died is
                // the worst available answer.
                Life::None => return Err(Failure::NoClient),
                Life::Running => {}
            }

            if tokio::time::timeout_at(deadline, waiting).await.is_err() {
                return Ok(Settled::TimedOut);
            }
        }
    }
}

impl Waypuppet for Server {
    async fn status(self, _: Context) -> Result<SessionStatus, Failure> {
        let snapshot = self
            .0
            .commands
            .ask(COMPOSITOR_DEADLINE, Command::Status)
            .await?;
        Ok(SessionStatus {
            output: snapshot.output,
            scale: snapshot.scale,
            client: self.client_state(snapshot.connections),
            windows: snapshot.windows,
        })
    }

    async fn shot(
        self,
        _: Context,
        _target: Target,
        output: Option<PathBuf>,
    ) -> Result<Capture, Failure> {
        let frame = self
            .0
            .commands
            .ask(COMPOSITOR_DEADLINE, Command::Capture)
            .await?
            .map_err(Failure::Capture)?;

        let path = output.unwrap_or_else(|| self.capture_path());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Failure::Capture(error.to_string()))?;
        }
        frame
            .write_png(&path)
            .map_err(|error| Failure::Capture(error.to_string()))?;

        let snapshot = self
            .0
            .commands
            .ask(COMPOSITOR_DEADLINE, Command::Status)
            .await?;
        Ok(Capture {
            path,
            size: Size {
                width: frame.width,
                height: frame.height,
            },
            scale: snapshot.scale,
        })
    }

    // `done` means "no more output is coming", which is true both when the
    // child has exited and when there is no child at all. Treating only the
    // former as done would make `logs` against a childless session block for
    // the whole poll window before answering emptily.
    async fn logs(self, context: Context, after: Cursor) -> Result<LogChunk, Failure> {
        let limit = remaining(&context).min(LOG_POLL);
        let deadline = tokio::time::Instant::now() + limit;

        loop {
            // Register interest *before* reading, or output arriving between
            // the read and the wait would be missed until the next poll.
            let waiting = self.0.activity.notified();

            let (chunk, has_new) = {
                let logs = lock(&self.0.logs);
                let done = !matches!(*lock(&self.0.client), Life::Running);
                let chunk = slice_from(&logs, after, done);
                let has_new = !chunk.stdout.is_empty() || !chunk.stderr.is_empty();
                (chunk, has_new)
            };

            if has_new || chunk.done || tokio::time::Instant::now() >= deadline {
                return Ok(chunk);
            }

            if tokio::time::timeout_at(deadline, waiting).await.is_err() {
                // Nothing new before the deadline. An empty chunk is the right
                // answer: the caller resumes from the same cursor.
                let logs = lock(&self.0.logs);
                let done = !matches!(*lock(&self.0.client), Life::Running);
                return Ok(slice_from(&logs, after, done));
            }
        }
    }

    async fn kill(self, _: Context) -> Result<(), Failure> {
        self.0.commands.send(Command::Stop)?;
        self.0.shutdown.notify_waiters();
        Ok(())
    }

    // Every mutating verb settles: it returns once the client has drawn in
    // response, so `click` followed by `shot` is correct with nothing in
    // between. That is the property waypuppet exists for, and it lives here
    // rather than in each caller.

    async fn key(self, _: Context, chords: Vec<String>) -> Result<(), Failure> {
        self.0
            .commands
            .settle(SETTLE_DEADLINE, |reply| Command::Key { chords, reply })
            .await
    }

    async fn write(self, _: Context, text: String) -> Result<(), Failure> {
        self.0
            .commands
            .settle(SETTLE_DEADLINE, |reply| Command::Write { text, reply })
            .await
    }

    async fn pointer(self, _: Context, to: Point, target: Target) -> Result<(), Failure> {
        self.0
            .commands
            .settle(SETTLE_DEADLINE, |reply| Command::Pointer {
                to,
                target,
                reply,
            })
            .await
    }

    async fn click(
        self,
        _: Context,
        at: Option<Point>,
        button: Button,
        target: Target,
    ) -> Result<(), Failure> {
        self.0
            .commands
            .settle(SETTLE_DEADLINE, |reply| Command::Click {
                at,
                button,
                target,
                reply,
            })
            .await
    }

    async fn scroll(self, _: Context, dx: i32, dy: i32) -> Result<(), Failure> {
        self.0
            .commands
            .settle(SETTLE_DEADLINE, |reply| Command::Scroll { dx, dy, reply })
            .await
    }

    async fn resize(self, _: Context, size: Size, target: Target) -> Result<(), Failure> {
        self.0
            .commands
            .settle(SETTLE_DEADLINE, |reply| Command::Resize {
                size,
                target,
                reply,
            })
            .await
    }

    // `wait` is the one verb the session answers from both halves of itself.
    // Four of the five conditions are protocol facts the compositor observes;
    // the fifth is about a process the compositor has never been told about.
    // [`Watching`] is that split, made total so neither half can be handed a
    // condition it cannot see.
    async fn wait(
        self,
        _: Context,
        condition: Condition,
        timeout: Duration,
    ) -> Result<Settled, Failure> {
        match Watching::from(condition) {
            Watching::Compositor(condition) => {
                self.0
                    .commands
                    .until(timeout, |reply| Command::Wait { condition, reply })
                    .await
            }
            Watching::Exit => self.wait_for_exit(timeout).await,
        }
    }

    async fn clipboard_set(
        self,
        _: Context,
        selection: Selection,
        text: String,
    ) -> Result<(), Failure> {
        self.0
            .commands
            .ask(COMPOSITOR_DEADLINE, |reply| Command::ClipboardSet {
                selection,
                text,
                reply,
            })
            .await?
    }

    // Deliberately on the compositor deadline rather than the shorter settle
    // one: this waits on the client to write down a pipe, which is the client
    // being slow rather than the client failing to draw, and the two do not
    // deserve the same patience.
    async fn clipboard_get(
        self,
        _: Context,
        selection: Selection,
    ) -> Result<Option<String>, Failure> {
        self.0
            .commands
            .ask(COMPOSITOR_DEADLINE, |reply| Command::ClipboardGet {
                selection,
                reply,
            })
            .await?
    }

    // Answered entirely on this thread. The compositor has never heard of
    // D-Bus and does not need to: the tree is the application describing
    // itself, over a bus this side owns — the same split as `wait --exit`.
    async fn tree(self, _: Context, _target: Target) -> Result<Node, Failure> {
        match &self.0.a11y {
            Ok(a11y) => a11y.tree().await,
            Err(why) => Err(Failure::Tree(why.clone())),
        }
    }
}

/// How long a call has left before its deadline.
///
/// tarpc propagates the caller's deadline, so a long poll can honour it rather
/// than inventing its own idea of patience.
fn remaining(context: &Context) -> Duration {
    context.deadline.saturating_duration_since(Instant::now())
}

/// Everything after `cursor`, and where to resume from.
fn slice_from(logs: &Logs, cursor: Cursor, done: bool) -> LogChunk {
    let take = |bytes: &[u8], from: u64| -> String {
        let from = usize::try_from(from).unwrap_or(usize::MAX).min(bytes.len());
        String::from_utf8_lossy(&bytes[from..]).into_owned()
    };

    LogChunk {
        stdout: take(&logs.stdout, cursor.stdout),
        stderr: take(&logs.stderr, cursor.stderr),
        cursor: Cursor {
            stdout: logs.stdout.len() as u64,
            stderr: logs.stderr.len() as u64,
        },
        done,
    }
}
