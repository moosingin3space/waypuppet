//! Wire types for the waypuppet control protocol.
//!
//! The control protocol is newline-delimited JSON over a Unix socket: one
//! [`Request`] per line, one [`Response`] per line back. That choice is
//! deliberate — it is inspectable with `socat`, needs no code generation, and
//! lets anything that can write a line drive a session, not just the CLI in
//! this workspace.
//!
//! Nothing here does I/O, so this crate stays cheap to depend on and cheap to
//! test.
//!
//! # Screenshots are paths, not payloads
//!
//! Captures are written to a file and the response carries the path. Keeping
//! image bytes out of the protocol keeps every message human-readable, and it
//! matches how the intended consumer works: a coding agent ingests an image by
//! reading a file, not by decoding base64 out of a JSON blob.

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
    /// Everything the session composites, including window borders and the
    /// space between windows.
    Output,
    /// One specific window.
    Window(WindowId),
}

/// A point in output-local logical coordinates, with the origin top-left.
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

/// A condition to block on.
///
/// Conditions describe protocol-level facts the host observes directly, which
/// is the point: `Frames(2)` is a claim about what the client actually
/// committed, where a sleep is only a guess about how long that takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// At least this many windows are mapped.
    Windows(usize),
    /// A mapped window's title matches this regular expression.
    TitleMatches(String),
    /// The target has committed this many further frames.
    Frames(u32),
    /// No client has committed a frame for this long.
    Idle(Duration),
    /// The spawned client has exited.
    Exit,
}

/// A request from a client of the control socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Describe the session and every mapped window.
    Status,
    /// Capture the target and write it to disk as a PNG.
    Shot {
        /// What to capture.
        #[serde(default)]
        target: Target,
        /// Where to write the image. The host picks a path when this is
        /// `None`.
        #[serde(default)]
        output: Option<PathBuf>,
    },
    /// Deliver key presses, each a chord such as `ctrl+shift+t` or `Return`.
    Key {
        /// Chords to deliver in order.
        chords: Vec<String>,
    },
    /// Deliver a string as individual key events.
    Type {
        /// The text to type.
        text: String,
    },
    /// Move the pointer to a point.
    Move {
        /// Destination, relative to `target`.
        to: Point,
        /// What the point is relative to.
        #[serde(default)]
        target: Target,
    },
    /// Press and release a pointer button, optionally moving there first.
    Click {
        /// Where to click. The pointer stays put when this is `None`.
        #[serde(default)]
        at: Option<Point>,
        /// Which button.
        button: Button,
        /// What `at` is relative to.
        #[serde(default)]
        target: Target,
    },
    /// Scroll by a discrete number of wheel clicks.
    Scroll {
        /// Horizontal clicks, positive rightwards.
        dx: i32,
        /// Vertical clicks, positive downwards.
        dy: i32,
    },
    /// Resize a window by reconfiguring it, as a real compositor would.
    Resize {
        /// The new size.
        size: Size,
        /// Which window to reconfigure.
        #[serde(default)]
        target: Target,
    },
    /// Block until a condition holds or the timeout elapses.
    Wait {
        /// The condition to wait for.
        condition: Condition,
        /// How long to wait before giving up.
        timeout: Duration,
    },
    /// Fetch the accessibility tree the client publishes over AT-SPI.
    Tree {
        /// Which window's tree to fetch.
        #[serde(default)]
        target: Target,
    },
    /// Fetch output captured from the spawned client.
    Logs,
    /// Terminate the client and shut the session down.
    Kill,
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
    /// Where the window sits on the output.
    pub origin: Point,
    /// The window's current size.
    pub size: Size,
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
    /// Gone, with the exit status it left behind. `None` means it was killed
    /// by a signal.
    Exited {
        /// The process exit code.
        code: Option<i32>,
    },
}

/// A reply to a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// The request succeeded and had nothing to report.
    Ok,
    /// A description of the session.
    Status {
        /// The session's output size.
        output: Size,
        /// How the spawned client is doing.
        client: ClientState,
        /// Every window the session knows about.
        windows: Vec<Window>,
    },
    /// A capture landed on disk.
    Shot {
        /// Where the PNG was written.
        path: PathBuf,
        /// The image's dimensions in physical pixels.
        size: Size,
    },
    /// Output captured from the spawned client.
    Logs {
        /// Everything it wrote to stdout.
        stdout: String,
        /// Everything it wrote to stderr.
        stderr: String,
    },
    /// An accessibility tree, serialised as it came off the bus.
    Tree {
        /// The root node.
        root: serde_json::Value,
    },
    /// The request failed.
    Error {
        /// What went wrong.
        message: String,
    },
}
