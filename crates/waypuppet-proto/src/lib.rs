//! The waypuppet control protocol: a [`Waypuppet`] service and the types it
//! exchanges.
//!
//! The protocol is [tarpc] over a Unix socket, framed with bincode. The service
//! is a Rust trait rather than a schema in another language, so there is no
//! codegen step and the compiler checks both ends against the same definition.
//!
//! Two things come along for free and are load-bearing rather than incidental:
//!
//! - **Deadlines.** Every call carries one, and it propagates. A `wait` that
//!   outlives its deadline is cancelled by the framework rather than left to
//!   leak a task on the server.
//! - **Cancellation.** A client that goes away takes its outstanding requests
//!   with it, which matters because several of these calls are long-lived by
//!   design.
//!
//! Nothing here does I/O, so this crate stays cheap to depend on and cheap to
//! test.
//!
//! # Captures are paths, not payloads
//!
//! [`Capture`] carries a filesystem path and the image is written as a PNG.
//! Keeping pixels out of the protocol matches the consumer: a coding agent
//! ingests an image by reading a file.
//!
//! # Everything is concretely typed
//!
//! Note the absence of any `serde_json::Value`-shaped escape hatch, including
//! for the accessibility tree, which is the obvious candidate. Bincode is not a
//! self-describing format, so a self-describing value cannot round-trip through
//! it — [`Node`] is a real type because it has to be, and is better for it.
//!
//! [tarpc]: https://lib.rs/crates/tarpc

#![forbid(unsafe_code)]

use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

/// Identifies a window inside a session.
///
/// Assigned by the host in the order windows are mapped, starting at 1, and
/// never reused within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub u32);

/// A pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    /// The primary button.
    Left,
    /// The secondary button.
    Right,
    /// The tertiary button, usually the wheel.
    Middle,
}

/// Which window a request applies to.
///
/// Most sessions host exactly one window, so [`Target::Focused`] is the default
/// and callers rarely name a window at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Whichever window currently holds keyboard focus.
    #[default]
    Focused,
    /// Everything the session composites, including the space between windows.
    Output,
    /// One specific window.
    Window(WindowId),
}

/// A point in logical coordinates, with the origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal offset, increasing rightwards.
    pub x: i32,
    /// Vertical offset, increasing downwards.
    pub y: i32,
}

/// A size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Width in logical pixels.
    pub width: u32,
    /// Height in logical pixels.
    pub height: u32,
}

/// A rectangle in logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    /// The top-left corner.
    pub origin: Point,
    /// The extent.
    pub size: Size,
}

/// Which of the two selections a clipboard request is about.
///
/// Wayland has both, and a terminal exercises both: [`Selection::Clipboard`] is
/// what an explicit copy fills, and [`Selection::Primary`] is the X11-style
/// select-then-middle-click one that terminal users expect to work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// The ordinary clipboard, filled by copy and read by paste.
    #[default]
    Clipboard,
    /// The primary selection, filled by selecting text.
    Primary,
}

/// A condition to block on.
///
/// Conditions describe protocol-level facts the host observes directly, which
/// is the point: [`Condition::Frames`] is a claim about what the client
/// actually committed, where a sleep is only a guess about how long that takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// At least this many windows are mapped.
    Windows(usize),
    /// A mapped window's title matches this regular expression.
    ///
    /// Mapped, not merely created: a title on a window nobody can see is not
    /// what a caller waiting on one means.
    TitleMatches(String),
    /// The focused window has committed this many further frames.
    ///
    /// Counted from the moment the request arrives, so it means "draw this many
    /// more times", not "have drawn this many in total". Fails at once with
    /// [`Failure::NoSuchWindow`] when there is no window yet — waiting for
    /// frames from something that does not exist is a hang dressed up as a
    /// condition, and [`Condition::Windows`] is what that caller wanted.
    Frames(u32),
    /// No client has committed a frame for this long.
    ///
    /// A heuristic, not a fact, and a trap for clients that repaint on a timer:
    /// they are never idle. Prefer [`Condition::Windows`] or
    /// [`Condition::TitleMatches`], which are facts about what the host
    /// observed rather than about what it has not seen lately.
    Idle(Duration),
    /// The spawned client has exited.
    Exit,
}

/// How a [`Waypuppet::wait`] call ended.
///
/// Timing out is an outcome rather than an error, because "it did not happen"
/// and "waypuppet fell over" call for different reactions from the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Settled {
    /// The condition held.
    Met,
    /// The timeout elapsed first.
    TimedOut,
}

/// A window the session is hosting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// The window's identifier.
    pub id: WindowId,
    /// The title the client set, if any.
    pub title: Option<String>,
    /// The `app_id` the client set, if any.
    pub app_id: Option<String>,
    /// Where the window sits on the output, and how big it is.
    pub geometry: Rect,
    /// Whether the window is mapped and has content.
    pub mapped: bool,
    /// How many frames the client has committed since it connected.
    pub frames: u64,
}

/// How the spawned client is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientState {
    /// Spawned, but it has not yet bound to the Wayland socket.
    Starting,
    /// Connected and running.
    Running,
    /// Gone. `code` is `None` when a signal killed it.
    Exited {
        /// The process exit code.
        code: Option<i32>,
    },
}

/// A description of the session.
// No `Eq`: the scale is a float. Comparing scales for exact equality is a thing
// callers should have to write out rather than get for free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    /// The session's output size.
    pub output: Size,
    /// The output's scale factor.
    pub scale: f64,
    /// How the spawned client is doing.
    pub client: ClientState,
    /// Every window the session knows about.
    pub windows: Vec<Window>,
}

/// A capture that landed on disk.
// No `Eq`: see `SessionStatus`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    /// Where the PNG was written.
    pub path: PathBuf,
    /// The image's dimensions in physical pixels.
    pub size: Size,
    /// The scale the capture was taken at. Comparing a 2x capture against a 1x
    /// golden image is a confusing way to fail, so the scale travels with it.
    pub scale: f64,
}

/// How far through the client's output a reader has got.
///
/// Byte offsets into each stream. A caller polls with the cursor it last
/// received; the host holds the call open until there is more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// Bytes of stdout already delivered.
    pub stdout: u64,
    /// Bytes of stderr already delivered.
    pub stderr: u64,
}

/// Output captured from the spawned client since a [`Cursor`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogChunk {
    /// New stdout.
    pub stdout: String,
    /// New stderr.
    pub stderr: String,
    /// Where to resume from.
    pub cursor: Cursor,
    /// Whether the client has exited, so a follower knows to stop.
    pub done: bool,
}

/// A node in the accessibility tree the client publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// The AT-SPI role, such as `push_button` or `terminal`.
    pub role: String,
    /// The accessible name, where the widget has one.
    pub name: Option<String>,
    /// Where the widget sits, in the same coordinates pointer input takes.
    pub bounds: Option<Rect>,
    /// Child nodes, in the order the application reports them.
    pub children: Vec<Node>,
}

/// Why a call failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    /// The named window does not exist, or nothing has focus.
    #[error("no such window")]
    NoSuchWindow,
    /// The spawned client has exited, so there is nothing to drive.
    #[error("the client has exited")]
    ClientGone,
    /// The session hosts no child process at all, so there is nothing that
    /// could ever exit.
    ///
    /// Distinct from [`Failure::ClientGone`], which says a client was there and
    /// is not any more. Collapsing the two would tell somebody whose
    /// application never started that it had died.
    #[error("this session hosts no client")]
    NoClient,
    /// A [`Condition::TitleMatches`] pattern is not a valid regular expression.
    ///
    /// Reported once, when the wait is parked, rather than being retried
    /// against every window: a pattern that cannot compile will not compile
    /// later either, and a caller who mistyped one wants to hear about it
    /// immediately rather than at the timeout.
    #[error("{pattern:?} is not a valid regular expression: {why}")]
    BadPattern {
        /// The pattern as the caller wrote it.
        pattern: String,
        /// What the regex engine objected to.
        why: String,
    },
    /// A key chord did not parse, or names something the keymap cannot produce.
    #[error("cannot produce {chord:?} with the active keymap")]
    UnmappableChord {
        /// The chord as the caller wrote it.
        chord: String,
    },
    /// Events were delivered but the client drew nothing before the settle
    /// timeout, so the reply cannot promise the effect is visible.
    #[error("the client did not draw within the settle timeout")]
    NotSettled,
    /// Rendering or writing the capture failed.
    #[error("capture failed: {0}")]
    Capture(String),
    /// A selection could not be read or written.
    ///
    /// Transfers are fd-based and cooperative: the other side may offer a type
    /// nobody asked for, write something that is not text, or hand over a pipe
    /// and never fill it. None of those is waypuppet failing, and none of them
    /// is an empty clipboard either — which is why this is distinct from a
    /// successful [`Waypuppet::clipboard_get`] returning `None`.
    #[error("clipboard transfer failed: {0}")]
    Clipboard(String),
    /// The compositor loop stopped making progress.
    ///
    /// **This is a bug in waypuppet, not in the application under test.** It is
    /// reported separately from [`Failure::TimedOut`] precisely so a caller can
    /// tell "my application is not drawing" from "waypuppet is stuck", which
    /// otherwise look identical from outside.
    #[error("the compositor loop has not ticked for {millis}ms")]
    LoopStalled {
        /// How long the loop has been stuck.
        millis: u64,
    },
    /// The call ran out of time while the compositor was healthy.
    ///
    /// Distinct from [`Failure::NotSettled`], which is specifically about the
    /// client not drawing.
    #[error("timed out after {millis}ms")]
    TimedOut {
        /// The deadline that elapsed.
        millis: u64,
    },
    /// The session is shutting down, so nothing further will be answered.
    #[error("the session is shutting down")]
    ShuttingDown,
    /// The feature exists in the design but not yet in the code.
    #[error("not implemented yet: {0}")]
    Unimplemented(String),
}

/// The control surface a session exposes.
///
/// Mutating calls settle before they return: they wait for the client to commit
/// a frame in response, so [`Waypuppet::click`] followed by [`Waypuppet::shot`]
/// is correct with nothing in between. That is the property the whole project
/// is for, so it lives here rather than in each caller.
#[tarpc::service]
pub trait Waypuppet {
    /// Describe the session and every window it knows about.
    async fn status() -> Result<SessionStatus, Failure>;

    /// Capture the target and write it to disk as a PNG.
    ///
    /// The host picks a path when `output` is `None`.
    async fn shot(target: Target, output: Option<PathBuf>) -> Result<Capture, Failure>;

    /// Deliver key chords, each one like `ctrl+shift+t` or `Return`.
    async fn key(chords: Vec<String>) -> Result<(), Failure>;

    /// Deliver a string as individual key events.
    async fn write(text: String) -> Result<(), Failure>;

    /// Move the pointer.
    async fn pointer(to: Point, target: Target) -> Result<(), Failure>;

    /// Press and release a pointer button, moving there first when `at` is set.
    async fn click(at: Option<Point>, button: Button, target: Target) -> Result<(), Failure>;

    /// Scroll by a discrete number of wheel clicks.
    async fn scroll(dx: i32, dy: i32) -> Result<(), Failure>;

    /// Resize a window by reconfiguring it, as a real compositor would.
    async fn resize(size: Size, target: Target) -> Result<(), Failure>;

    /// Block until a condition holds or `timeout` elapses.
    ///
    /// The timeout is explicit rather than left to the call's deadline so that
    /// the outcome is a [`Settled`] value instead of a transport error. Set the
    /// deadline a little beyond it as a backstop.
    ///
    /// The condition is evaluated when the request arrives, so waiting for
    /// something that is already true returns immediately rather than blocking
    /// until it next changes. A [`Failure`] here is never "it did not happen" —
    /// that is [`Settled::TimedOut`] — but something that makes the wait
    /// pointless: a pattern that will not compile, or a window that has gone
    /// away with the wait still counting its frames.
    async fn wait(condition: Condition, timeout: Duration) -> Result<Settled, Failure>;

    /// Offer `text` to the client as the given selection.
    ///
    /// waypuppet becomes the selection's owner, so the client sees a paste
    /// arrive from the compositor exactly as it would from another application.
    /// That is a genuinely different thing from [`Waypuppet::write`]: a
    /// terminal distinguishes a paste from typing — bracketed paste exists for
    /// precisely that reason — so both are worth having.
    async fn clipboard_set(selection: Selection, text: String) -> Result<(), Failure>;

    /// Read the given selection as UTF-8 text.
    ///
    /// `None` means nothing owns the selection, which is an ordinary answer
    /// rather than a failure. A selection that exists but cannot be had as text
    /// is [`Failure::Clipboard`], because "there is nothing" and "there is
    /// something you cannot read" call for different reactions.
    ///
    /// Reads the selection whether the client set it or waypuppet did, so
    /// `clipboard_set` followed by `clipboard_get` round-trips.
    async fn clipboard_get(selection: Selection) -> Result<Option<String>, Failure>;

    /// Fetch the accessibility tree the client publishes over AT-SPI.
    async fn tree(target: Target) -> Result<Node, Failure>;

    /// Fetch client output recorded after `after`.
    ///
    /// A long poll: the call stays open until there is something to return, the
    /// client exits, or the deadline passes. Following is a loop over this,
    /// resuming from [`LogChunk::cursor`].
    async fn logs(after: Cursor) -> Result<LogChunk, Failure>;

    /// Terminate the client and shut the session down.
    async fn kill() -> Result<(), Failure>;
}
