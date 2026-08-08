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
use waypuppet_proto::{Failure, Size, Window};

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

/// Something for the compositor thread to do.
#[derive(Debug)]
pub enum Command {
    /// Describe the compositor's current state.
    Status(oneshot::Sender<Snapshot>),
    /// Composite and read back a frame.
    Capture(oneshot::Sender<Result<Frame, String>>),
    /// Stop the loop. Nothing to answer: the sender dropping is the answer.
    Stop,
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
}
