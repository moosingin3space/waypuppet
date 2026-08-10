//! The bridge between the compositor thread and the control thread.
//!
//! The compositor owns every piece of Wayland state and runs on calloop. The
//! RPC server runs on tokio. They meet here: a command carries a `oneshot`
//! sender, the compositor answers it, and the handler awaits the reply.
//!
//! # The rule
//!
//! **The compositor loop must never block on a request.** Several calls wait on
//! the very client the loop exists to service, so a handler that blocked the
//! loop would be waiting on something it had just stopped from happening.
//! Commands are therefore handled and answered inline; anything that cannot be
//! answered immediately is parked with its sender held elsewhere.
//!
//! # Why `Drop` does the cancelling
//!
//! Every waiting handler holds a `oneshot::Receiver`. If the compositor thread
//! dies — panic, shutdown, anything — its senders drop, and every outstanding
//! call fails at once with [`Failure::ShuttingDown`] rather than sitting on a
//! deadline. That is free, immediate, and cannot be forgotten, which is worth
//! more than any amount of liveness checking.
//!
//! The obligation this creates: a parked sender's lifetime must be tied to the
//! condition still being *possible*. Drop it as soon as it cannot be answered.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use calloop::ping::Ping;
use tokio::sync::{mpsc, oneshot};
use waypuppet_proto::{
    Button, Condition, Failure, Point, Selection, Settled, Size, Target, Window,
};

use crate::Frame;

/// A snapshot of what the compositor knows, for `SessionStatus`.
///
/// The compositor half only: the client's process state is the control
/// thread's business, and is filled in there.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The output's logical size.
    pub output: Size,
    /// The output's scale.
    pub scale: f64,
    /// Every window the compositor knows about.
    pub windows: Vec<Window>,
    /// How many Wayland clients are connected. Zero after a spawn means the
    /// process started but has not reached the compositor — which is a
    /// different problem from a process that connected and then crashed.
    pub connections: usize,
}

/// A condition the compositor can answer for itself.
///
/// Every [`Condition`] but [`Condition::Exit`]. The compositor has never been
/// told there is a child process — it sees Wayland connections, and a process
/// can exit without ever making one — so that condition belongs to the control
/// thread, which spawned it.
///
/// Splitting the two here rather than inside the compositor is what keeps the
/// loop's match total. A single enum carrying all five would need a "cannot
/// happen" arm, and an arm that cannot happen is an arm that answers wrongly
/// when it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watched {
    /// At least this many windows are mapped.
    Windows(usize),
    /// A mapped window's title matches this regular expression.
    Title(String),
    /// The focused window commits this many further frames.
    Frames(u32),
    /// Nothing commits for this long.
    Idle(Duration),
}

/// Which half of a session can see a [`Condition`] become true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watching {
    /// The compositor observes it, over the bridge.
    Compositor(Watched),
    /// Only the control thread does: it owns the child process.
    Exit,
}

impl From<Condition> for Watching {
    fn from(condition: Condition) -> Self {
        match condition {
            Condition::Windows(count) => Self::Compositor(Watched::Windows(count)),
            Condition::TitleMatches(pattern) => Self::Compositor(Watched::Title(pattern)),
            Condition::Frames(frames) => Self::Compositor(Watched::Frames(frames)),
            Condition::Idle(quiet) => Self::Compositor(Watched::Idle(quiet)),
            Condition::Exit => Self::Exit,
        }
    }
}

/// Something for the compositor thread to do.
#[derive(Debug)]
pub enum Command {
    /// Describe the compositor's current state.
    Status(oneshot::Sender<Snapshot>),
    /// Composite and read back a frame.
    Capture(oneshot::Sender<Result<Frame, String>>),
    /// Stop the loop. Nothing to answer: the sender dropping is the answer.
    Stop,
    /// Block the compositor thread for this long, on purpose.
    ///
    /// Fault injection. [`Heartbeat`] claims to tell a wedged loop from a
    /// client that is merely quiet, and the only way to know whether it does is
    /// to wedge the loop and look — so this exists to make that testable rather
    /// than assumed.
    ///
    /// Nothing reachable from the CLI or the control protocol sends it; it is a
    /// library-level affordance for waypuppet's own tests.
    Stall(Duration),
    /// Deliver keyboard chords and settle on the next client frame.
    Key {
        /// Chords in caller order.
        chords: Vec<String>,
        /// Where to click first, to move the toolkit's own focus.
        ///
        /// A resolved point, never a query: selectors are the control thread's
        /// business, and the compositor has never heard of the accessibility
        /// tree — the same split that keeps `tree` off this channel entirely.
        focus: Option<Point>,
        /// Reply after the client settles.
        reply: oneshot::Sender<Result<(), Failure>>,
    },
    /// Type text and settle on the next client frame.
    Write {
        /// Text to type.
        text: String,
        /// Where to click first, to move the toolkit's own focus.
        focus: Option<Point>,
        /// Reply after the client settles.
        reply: oneshot::Sender<Result<(), Failure>>,
    },
    /// Move the pointer and settle on the next client frame.
    Pointer {
        /// Destination in target coordinates.
        to: Point,
        /// Coordinate target.
        target: Target,
        /// Reply after the client settles.
        reply: oneshot::Sender<Result<(), Failure>>,
    },
    /// Click a button and settle on the next client frame.
    Click {
        /// Optional destination in target coordinates.
        at: Option<Point>,
        /// Button to click.
        button: Button,
        /// Coordinate target.
        target: Target,
        /// Reply after the client settles.
        reply: oneshot::Sender<Result<(), Failure>>,
    },
    /// Scroll and settle on the next client frame.
    Scroll {
        /// Horizontal wheel clicks.
        dx: i32,
        /// Vertical wheel clicks.
        dy: i32,
        /// Reply after the client settles.
        reply: oneshot::Sender<Result<(), Failure>>,
    },
    /// Resize and settle on the next client frame.
    Resize {
        /// New size.
        size: Size,
        /// Window target.
        target: Target,
        /// Reply after the client settles.
        reply: oneshot::Sender<Result<(), Failure>>,
    },
    /// Offer text to the client as a selection.
    ///
    /// Answered immediately: owning a selection is just bookkeeping on the
    /// seat. The transfer happens later, if the client ever asks to read it.
    ClipboardSet {
        /// Which selection to take ownership of.
        selection: Selection,
        /// The text to offer.
        text: String,
        /// Reply once the seat owns it.
        reply: oneshot::Sender<Result<(), Failure>>,
    },
    /// Read a selection back as text.
    ///
    /// Usually *not* answerable on the spot: when a client owns the selection,
    /// waypuppet hands it a pipe and the client writes down it whenever it
    /// feels like it. The reply is parked on that transfer.
    ClipboardGet {
        /// Which selection to read.
        selection: Selection,
        /// Reply once the transfer finishes.
        reply: oneshot::Sender<Result<Option<String>, Failure>>,
    },
    /// Park a reply until a fact the compositor observes is true.
    ///
    /// Unlike the settling verbs above, nothing is delivered first — there is
    /// no event to send, only a condition to watch. The reply is answered when
    /// the condition holds, failed when it becomes impossible, and dropped if
    /// the compositor goes away.
    Wait {
        /// What to watch for.
        condition: Watched,
        /// Answered once it holds.
        reply: oneshot::Sender<Result<(), Failure>>,
    },
}

/// Proof that the compositor loop is still running.
///
/// One relaxed store per iteration, which buys the distinction between "the
/// application is not drawing" — a normal outcome — and "waypuppet is stuck",
/// which is a bug here. Without it both are the same timeout and a caller
/// cannot tell whose fault it is.
#[derive(Debug)]
pub struct Heartbeat {
    ticks: AtomicU64,
    /// Milliseconds since [`Heartbeat::started`], as of the last tick.
    last: AtomicU64,
    started: Instant,
}

impl Heartbeat {
    /// A heartbeat that has just started beating.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ticks: AtomicU64::new(0),
            last: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    /// Record one iteration of the loop.
    pub fn beat(&self) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
        let millis = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.store(millis, Ordering::Relaxed);
    }

    /// How many iterations the loop has completed.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    /// How long since the loop last completed an iteration.
    #[must_use]
    pub fn stalled_for(&self) -> Duration {
        let last = self.last.load(Ordering::Relaxed);
        self.started
            .elapsed()
            .saturating_sub(Duration::from_millis(last))
    }

    /// Diagnose a call that ran out of time.
    ///
    /// The whole reason this type exists: if the loop advanced while the caller
    /// was waiting, the compositor is healthy and the failure belongs to
    /// whatever was being waited for. If it did not, waypuppet is stuck and
    /// should say so rather than blaming the application.
    #[must_use]
    pub fn diagnose(&self, ticks_at_start: u64, waited: Duration) -> Failure {
        if self.ticks() == ticks_at_start {
            let stalled = self.stalled_for().max(waited);
            Failure::LoopStalled {
                millis: u64::try_from(stalled.as_millis()).unwrap_or(u64::MAX),
            }
        } else {
            Failure::TimedOut {
                millis: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
            }
        }
    }
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self::new()
    }
}

/// The control thread's end of the bridge.
///
/// Cloneable and `Sync`, because every RPC handler needs one.
#[derive(Debug, Clone)]
pub struct Commands {
    sender: mpsc::UnboundedSender<Command>,
    /// Wakes the compositor's event loop after a command is queued. Without
    /// this the command sits in the queue until the loop happens to wake for
    /// some other reason.
    ping: Ping,
    heartbeat: Arc<Heartbeat>,
}

impl Commands {
    /// Build both ends of the bridge.
    pub(crate) fn new(
        ping: Ping,
        heartbeat: Arc<Heartbeat>,
    ) -> (Self, mpsc::UnboundedReceiver<Command>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                sender,
                ping,
                heartbeat,
            },
            receiver,
        )
    }

    /// The compositor's liveness signal.
    #[must_use]
    pub fn heartbeat(&self) -> &Arc<Heartbeat> {
        &self.heartbeat
    }

    /// Queue a command without waiting for it.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::ShuttingDown`] if the compositor thread is gone.
    pub fn send(&self, command: Command) -> Result<(), Failure> {
        self.sender
            .send(command)
            .map_err(|_| Failure::ShuttingDown)?;
        self.ping.ping();
        Ok(())
    }

    /// Send a command and await its reply, bounded by `deadline`.
    ///
    /// Every wait in waypuppet goes through something like this. There is no
    /// unbounded await on the bridge, on purpose: a hang that reports itself in
    /// five seconds is worth far more than one that runs until CI gives up.
    ///
    /// # Errors
    ///
    /// - [`Failure::ShuttingDown`] if the compositor thread is gone, which
    ///   arrives immediately via the dropped sender rather than after
    ///   `deadline`.
    /// - [`Failure::LoopStalled`] or [`Failure::TimedOut`] on expiry,
    ///   whichever the heartbeat says is true.
    pub async fn ask<T>(
        &self,
        deadline: Duration,
        build: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, Failure> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let ticks = self.heartbeat.ticks();
        self.send(build(reply_tx))?;

        let started = Instant::now();
        match tokio::time::timeout(deadline, reply_rx).await {
            Ok(Ok(value)) => Ok(value),
            // The compositor dropped the sender: it is gone, and no amount of
            // waiting would have helped.
            Ok(Err(_)) => Err(Failure::ShuttingDown),
            Err(_) => Err(self.heartbeat.diagnose(ticks, started.elapsed())),
        }
    }

    /// Send a mutating command and await the frame it should provoke.
    ///
    /// The same wait as [`Commands::ask`], with one substitution: a healthy
    /// loop that ran out of time means the *client* did not draw, which is
    /// [`Failure::NotSettled`] — a normal outcome about the application under
    /// test — rather than a bare timeout. A loop that stopped ticking still
    /// reports [`Failure::LoopStalled`], because that is waypuppet's bug and
    /// collapsing the two is exactly what this project refuses to do.
    ///
    /// # Errors
    ///
    /// - [`Failure::NotSettled`] if the client drew nothing in time.
    /// - [`Failure::LoopStalled`] if the compositor loop stopped.
    /// - [`Failure::ShuttingDown`] if the compositor thread is gone.
    /// - Whatever the command itself failed with, such as
    ///   [`Failure::UnmappableChord`] or [`Failure::NoSuchWindow`].
    pub async fn settle(
        &self,
        deadline: Duration,
        build: impl FnOnce(oneshot::Sender<Result<(), Failure>>) -> Command,
    ) -> Result<(), Failure> {
        match self.ask(deadline, build).await {
            Ok(result) => result,
            Err(Failure::TimedOut { .. }) => Err(Failure::NotSettled),
            Err(other) => Err(other),
        }
    }

    /// Park a request until the compositor says its condition holds.
    ///
    /// The same wait again, with the substitution a `wait` needs: a healthy
    /// loop that ran out of time means **the condition did not happen**, which
    /// is [`Settled::TimedOut`] — an outcome the caller may well have expected,
    /// and not an error at all. A loop that stopped ticking still reports
    /// [`Failure::LoopStalled`].
    ///
    /// That is the whole reason this is a wrapper rather than its own timeout:
    /// three verbs now describe an expired wait three different ways, and
    /// [`Heartbeat::diagnose`] remains the single place that decides whose
    /// fault it was.
    ///
    /// # Errors
    ///
    /// - [`Failure::LoopStalled`] if the compositor loop stopped.
    /// - [`Failure::ShuttingDown`] if the compositor thread is gone.
    /// - Whatever made the condition impossible, such as
    ///   [`Failure::ClientGone`] or [`Failure::BadPattern`].
    pub async fn until(
        &self,
        timeout: Duration,
        build: impl FnOnce(oneshot::Sender<Result<(), Failure>>) -> Command,
    ) -> Result<Settled, Failure> {
        match self.ask(timeout, build).await {
            Ok(Ok(())) => Ok(Settled::Met),
            Ok(Err(failure)) => Err(failure),
            Err(Failure::TimedOut { .. }) => Ok(Settled::TimedOut),
            Err(other) => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use calloop::ping::make_ping;

    use super::{Command, Commands, Failure, Heartbeat, Settled, Watched};
    use std::{sync::Arc, time::Duration};

    /// Short enough to keep the suite quick, long enough not to be flaky on a
    /// loaded machine.
    const DEADLINE: Duration = Duration::from_millis(200);

    /// Build a bridge whose compositor end is whatever the caller does with the
    /// receiver.
    fn bridge() -> (
        Commands,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
        calloop::ping::PingSource,
    ) {
        let (ping, source) = make_ping().expect("ping");
        let (commands, receiver) = Commands::new(ping, Arc::new(Heartbeat::new()));
        (commands, receiver, source)
    }

    #[tokio::test]
    async fn a_loop_that_never_ticks_is_reported_as_stalled() {
        // The receiver is held but never drained, which is what a wedged
        // compositor looks like from here: the command is queued and nothing
        // ever answers it.
        let (commands, _receiver, _source) = bridge();

        let failure = commands
            .ask(DEADLINE, Command::Status)
            .await
            .expect_err("nothing answered, so this cannot succeed");

        assert!(
            matches!(failure, Failure::LoopStalled { .. }),
            "a loop that never ticked should be blamed, not the caller: {failure:?}",
        );
    }

    #[tokio::test]
    async fn a_healthy_loop_is_reported_as_a_plain_timeout() {
        let (commands, _receiver, _source) = bridge();
        let heartbeat = Arc::clone(commands.heartbeat());

        // The loop is alive and ticking; it just has not got to this command.
        // That is the application's problem, not waypuppet's, and the failure
        // has to say so.
        tokio::spawn(async move {
            loop {
                heartbeat.beat();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let failure = commands
            .ask(DEADLINE, Command::Status)
            .await
            .expect_err("nothing answered, so this cannot succeed");

        assert!(
            matches!(failure, Failure::TimedOut { .. }),
            "a ticking loop must not be blamed: {failure:?}",
        );
    }

    #[tokio::test]
    async fn a_dropped_reply_fails_at_once_rather_than_on_the_deadline() {
        let (commands, mut receiver, _source) = bridge();

        // Take the command and drop it without answering — exactly what a
        // compositor thread that panics does to everything it was holding.
        tokio::spawn(async move {
            drop(receiver.recv().await);
            // Keep the receiver's channel open, so this tests the dropped
            // *reply* rather than the dropped channel.
            pending::<()>().await;
        });

        let started = std::time::Instant::now();
        let failure = commands
            .ask(Duration::from_secs(30), Command::Status)
            .await
            .expect_err("the reply was dropped");

        assert!(
            matches!(failure, Failure::ShuttingDown),
            "expected shutdown, got {failure:?}",
        );
        // The point of defence 2: this is `Drop` waking the receiver, not the
        // deadline expiring. A thirty-second deadline would have hidden it.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "should have failed immediately, took {:?}",
            started.elapsed(),
        );
    }

    #[tokio::test]
    async fn a_settling_verb_blames_the_client_when_the_loop_is_healthy() {
        let (commands, _receiver, _source) = bridge();
        let heartbeat = Arc::clone(commands.heartbeat());

        tokio::spawn(async move {
            loop {
                heartbeat.beat();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let failure = commands
            .settle(DEADLINE, |reply| Command::Write {
                text: "hello".to_owned(),
                focus: None,
                reply,
            })
            .await
            .expect_err("nothing answered, so this cannot succeed");

        assert!(
            matches!(failure, Failure::NotSettled),
            "a healthy loop that ran out of time means the client did not draw, \
             which is a fact about the application, not a timeout: {failure:?}",
        );
    }

    #[tokio::test]
    async fn a_settling_verb_still_blames_the_compositor_when_the_loop_stops() {
        // The receiver is held but never drained: the loop is wedged. Reporting
        // `NotSettled` here would tell the caller to go and debug a perfectly
        // healthy application, which is exactly the confusion the two variants
        // exist to prevent.
        let (commands, _receiver, _source) = bridge();

        let failure = commands
            .settle(DEADLINE, |reply| Command::Write {
                text: "hello".to_owned(),
                focus: None,
                reply,
            })
            .await
            .expect_err("nothing answered, so this cannot succeed");

        assert!(
            matches!(failure, Failure::LoopStalled { .. }),
            "a settling verb must not launder a stalled loop into NotSettled: {failure:?}",
        );
    }

    #[tokio::test]
    async fn a_wait_that_ran_out_of_time_is_an_outcome_rather_than_an_error() {
        let (commands, _receiver, _source) = bridge();
        let heartbeat = Arc::clone(commands.heartbeat());

        tokio::spawn(async move {
            loop {
                heartbeat.beat();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let settled = commands
            .until(DEADLINE, |reply| Command::Wait {
                condition: Watched::Windows(1),
                reply,
            })
            .await
            .expect("a timeout is not an error here — that is the point");

        assert_eq!(
            settled,
            Settled::TimedOut,
            "a condition that did not happen is a fact about the session, \
             not a failure of the call",
        );
    }

    #[tokio::test]
    async fn a_wait_still_blames_the_compositor_when_the_loop_stops() {
        // Held and never drained: the loop is wedged. Reporting `TimedOut` here
        // would tell the caller their condition simply did not occur, when in
        // fact nothing was ever looking for it.
        let (commands, _receiver, _source) = bridge();

        let failure = commands
            .until(DEADLINE, |reply| Command::Wait {
                condition: Watched::Windows(1),
                reply,
            })
            .await
            .expect_err("a stalled loop cannot answer a condition");

        assert!(
            matches!(failure, Failure::LoopStalled { .. }),
            "a wait must not launder a stalled loop into a clean timeout: {failure:?}",
        );
    }

    #[tokio::test]
    async fn a_gone_compositor_fails_without_queueing() {
        let (commands, receiver, _source) = bridge();
        drop(receiver);

        let failure = commands
            .ask(Duration::from_secs(30), Command::Status)
            .await
            .expect_err("there is nothing to answer");

        assert!(
            matches!(failure, Failure::ShuttingDown),
            "expected shutdown, got {failure:?}",
        );
    }
}
