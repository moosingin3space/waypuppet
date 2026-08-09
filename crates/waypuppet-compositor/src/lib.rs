//! The headless Wayland compositor that hosts and drives an application under
//! test.
//!
//! waypuppet is a compositor rather than a script that pokes at somebody else's
//! compositor, and every capability follows from that. Input injection is a
//! call into the seat, not a synthetic uinput device. A capture is the frame
//! the client committed, not a race against a screenshot tool. "The window is
//! mapped" is observed on the protocol, not inferred from a sleep.
//!
//! # Status
//!
//! [`probe`] reports what there is to render with. [`Compositor`] hosts a real
//! toolkit: it composites `wl_shm` and `zwp_linux_dmabuf_v1` buffers offscreen,
//! reads frames back as PNGs, delivers keyboard and pointer input through the
//! seat, and answers `wait` from its own bookkeeping rather than from a timer.
//! The accessibility tree is what remains; see `plans/`.

// Not `forbid`: `render` needs a narrowly scoped exception for smithay's
// unsafe EGL constructors, and `forbid` cannot be overridden at all. The
// workspace still denies it, so every use has to be justified in place.

pub mod bridge;
pub mod input;
mod render;
pub mod session;
mod state;

use std::{path::PathBuf, sync::Arc, time::Duration};

use smithay::{
    backend::egl::EGLDevice,
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic, ping::make_ping},
        wayland_server::{Display, protocol::wl_surface::WlSurface},
    },
    wayland::socket::ListeningSocketSource,
};
use tokio::sync::oneshot;
use waypuppet_proto::{Failure, Size};

pub use crate::{
    bridge::{Command, Commands, Heartbeat, Snapshot, Watched, Watching},
    render::{Frame, Renderer},
    state::State,
};

/// Anything that can go wrong hosting a session.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No EGL device was usable, so there is nothing to render with.
    #[error("no usable EGL device: {0}")]
    NoRenderDevice(String),
    /// EGL itself failed.
    #[error("EGL error")]
    Egl(#[from] smithay::backend::egl::Error),
    /// The compositor could not be set up or could not keep running.
    #[error("compositor error: {0}")]
    Compositor(String),
    /// Rendering or reading a frame back failed.
    #[error("render error: {0}")]
    Render(String),
    /// Encoding or writing a capture failed.
    #[error("capture failed: {0}")]
    Capture(String),
    /// The feature exists in the design but not yet in the code.
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

/// Which device a session renders on.
///
/// The choice affects pixels, not just speed: llvmpipe and a GPU driver
/// antialias and filter differently, so a capture taken under one will not be
/// byte-identical to the same capture under the other. That is why this is a
/// deliberate setting rather than "whatever is fastest" — golden images are
/// only comparable against captures from the same renderer, and CI is almost
/// always software.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Rendering {
    /// The fastest device available: hardware where there is any, software
    /// otherwise.
    #[default]
    Fastest,
    /// Software only, even where a GPU exists.
    ///
    /// The setting golden-image testing needs: it makes a developer machine
    /// rasterise the way a runner with no render node will.
    Software,
    /// One named DRM node.
    Device(PathBuf),
}

/// How a session is set up.
///
/// Everything here is fixed for the session's lifetime, and that is the point:
/// a capture is only comparable against a previous capture if the output it
/// came from was identical.
#[derive(Debug, Clone)]
pub struct Config {
    /// The output's logical size.
    pub output: Size,
    /// The output's scale factor, reported to clients via `wl_output` and
    /// `wp_fractional_scale`.
    pub scale: f64,
    /// Which device to render on.
    pub rendering: Rendering,
    /// Where captures land when a request does not name a path.
    pub capture_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output: Size {
                width: 1280,
                height: 800,
            },
            scale: 1.0,
            rendering: Rendering::default(),
            capture_dir: std::env::temp_dir(),
        }
    }
}

/// A rendering device waypuppet could use.
#[derive(Debug, Clone)]
pub struct RenderDevice {
    /// The DRM node backing it, when it has one. Software devices do not.
    pub path: Option<PathBuf>,
    /// Whether this renders in software — llvmpipe rather than a GPU. Usable,
    /// and often all a container is given, but slow.
    pub software: bool,
}

/// Enumerate the EGL devices available to this process.
///
/// This is the one assumption worth checking before anything else: waypuppet
/// needs a render node it can open, and in a container that depends entirely on
/// what was passed in. An empty result means the sandbox is missing
/// `--device=dri` (Flatpak) or `--device /dev/dri` (Podman).
///
/// # Errors
///
/// Returns [`Error::Egl`] if EGL cannot be initialised at all, which usually
/// means no EGL implementation is installed rather than no device.
pub fn probe() -> Result<Vec<RenderDevice>, Error> {
    Ok(EGLDevice::enumerate()?
        .map(|device| RenderDevice {
            path: device.drm_device_path().ok(),
            software: device.is_software(),
        })
        .collect())
}

/// A running compositor and the loop that drives it.
#[expect(
    missing_debug_implementations,
    reason = "neither Display nor EventLoop is Debug"
)]
pub struct Compositor {
    event_loop: EventLoop<'static, State>,
    display: Display<State>,
    state: State,
    socket_name: String,
    /// The control thread's end of the bridge, handed out by
    /// [`Compositor::commands`].
    commands: Commands,
    /// Bumped every iteration, so a stalled loop is distinguishable from a
    /// client that is simply not drawing.
    heartbeat: Arc<Heartbeat>,
}

impl Compositor {
    /// Set up the compositor and start listening on a Wayland socket.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compositor`] if the event loop, the display or the
    /// socket cannot be created.
    pub fn new(config: &Config) -> Result<Self, Error> {
        let event_loop =
            EventLoop::try_new().map_err(|error| Error::Compositor(error.to_string()))?;
        let mut display =
            Display::<State>::new().map_err(|error| Error::Compositor(error.to_string()))?;
        let display_handle = display.handle();
        let handle = event_loop.handle();

        // The loop handle goes in because a clipboard transfer is a pipe the
        // client fills at its own pace, and the only way to wait for that
        // without stopping the compositor is to let the compositor do the
        // waiting — as one more event source.
        let state = State::new(display_handle, handle.clone(), config)?;

        let socket = ListeningSocketSource::new_auto()
            .map_err(|error| Error::Compositor(error.to_string()))?;
        let socket_name = socket.socket_name().to_string_lossy().into_owned();
        handle
            .insert_source(socket, |stream, (), state: &mut State| {
                let data = std::sync::Arc::new(state.new_client_state());
                match state.display_handle.insert_client(stream, data) {
                    // Here, rather than from `ClientData::initialized`: this is
                    // the moment waypuppet itself accepts a client, so it is a
                    // fact this crate owns. `initialized` is not called by the
                    // backend `use_system_lib` selects — see `state::Clients`.
                    Ok(client) => state.client_connected(client.id()),
                    Err(error) => tracing::warn!(%error, "could not add wayland client"),
                }
            })
            .map_err(|error| Error::Compositor(error.to_string()))?;

        // Wake the loop when a client has something to say. The source only
        // exists to interrupt the poll — the actual dispatch happens in `run`,
        // where `display` and `state` can be borrowed separately.
        //
        // anvil instead puts the `Display` *inside* the calloop source and
        // reaches back into it with an `unsafe` `get_mut`. Cloning the poll fd
        // avoids that entirely, which matters here because the workspace denies
        // `unsafe_code`.
        let poll_fd = display
            .backend()
            .poll_fd()
            .try_clone_to_owned()
            .map_err(|error| Error::Compositor(error.to_string()))?;
        handle
            .insert_source(
                Generic::new(poll_fd, Interest::READ, Mode::Level),
                |_, _, _| Ok(PostAction::Continue),
            )
            .map_err(|error| Error::Compositor(error.to_string()))?;

        // The bridge. A ping wakes the loop when a command is queued; without
        // it the command would sit in the queue until the loop happened to wake
        // for some other reason, which is a latency bug that looks like a hang.
        let heartbeat = Arc::new(Heartbeat::new());
        let (ping, ping_source) =
            make_ping().map_err(|error| Error::Compositor(error.to_string()))?;
        let (commands, mut receiver) = Commands::new(ping, Arc::clone(&heartbeat));

        handle
            .insert_source(ping_source, move |(), (), state: &mut State| {
                // Drain rather than handle one: several commands may have been
                // queued between wake-ups, and a ping does not count them.
                while let Ok(command) = receiver.try_recv() {
                    handle_command(state, command);
                }
            })
            .map_err(|error| Error::Compositor(error.to_string()))?;

        Ok(Self {
            event_loop,
            display,
            state,
            socket_name,
            commands,
            heartbeat,
        })
    }

    /// The control thread's end of the bridge.
    #[must_use]
    pub fn commands(&self) -> Commands {
        self.commands.clone()
    }

    /// The compositor's liveness signal.
    #[must_use]
    pub fn heartbeat(&self) -> Arc<Heartbeat> {
        Arc::clone(&self.heartbeat)
    }

    /// The name of the Wayland socket clients should connect to.
    ///
    /// This is what goes in the child process's `WAYLAND_DISPLAY`.
    #[must_use]
    pub fn socket_name(&self) -> &str {
        &self.socket_name
    }

    /// What the compositor currently knows.
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// Service the event loop once.
    ///
    /// Exposed so a caller can interleave its own work — a test asserting
    /// between iterations, and eventually the control-socket bridge — without
    /// handing the whole thread to [`Compositor::run`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compositor`] if the event loop or client dispatch
    /// fails.
    pub fn tick(&mut self, timeout: Duration) -> Result<(), Error> {
        self.event_loop
            .dispatch(Some(timeout), &mut self.state)
            .map_err(|error| Error::Compositor(error.to_string()))?;

        self.display
            .dispatch_clients(&mut self.state)
            .map_err(|error| Error::Compositor(error.to_string()))?;
        self.display
            .flush_clients()
            .map_err(|error| Error::Compositor(error.to_string()))?;

        self.state.space.refresh();
        // Clients throttle their drawing on frame callbacks, so this is what
        // lets a real toolkit draw more than its first frame.
        self.state.pace_frames();
        // The one `wait` condition no event can announce: going quiet is the
        // absence of commits, so only the loop can notice it. Everything else
        // is answered where the fact changes.
        self.state.check_idle();
        // A caller that hit its timeout dropped its receiver; sweeping here is
        // what keeps those entries from accumulating.
        self.state.prune_parked();
        self.heartbeat.beat();
        Ok(())
    }

    /// Composite the current state and hand back the frame.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoRenderDevice`] if EGL cannot be brought up, or
    /// [`Error::Render`] if compositing or readback fails.
    pub fn capture(&mut self) -> Result<Frame, Error> {
        self.state.capture()
    }

    /// Run until something asks the compositor to stop.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compositor`] if the event loop or client dispatch
    /// fails.
    pub fn run(&mut self) -> Result<(), Error> {
        while self.state.running {
            self.tick(Duration::from_millis(16))?;
        }
        Ok(())
    }
}

/// Answer one command from the control thread.
///
/// Everything here is immediate. A command that cannot be answered on the spot
/// must park its sender somewhere the loop can reach later — never block, since
/// the loop is what the caller is usually waiting on.
fn handle_command(state: &mut State, command: Command) {
    match command {
        Command::Status(reply) => {
            // A closed receiver means the caller gave up. Nothing to do about
            // it, and nothing worth logging.
            let _ = reply.send(state.snapshot());
        }
        Command::Capture(reply) => {
            let _ = reply.send(state.capture().map_err(|error| error.to_string()));
        }
        Command::Stop => state.running = false,
        // Fault injection: block the loop for real, so the heartbeat's claim
        // to detect a stall can be tested rather than believed.
        Command::Stall(duration) => std::thread::sleep(duration),

        // Every mutating verb has the same shape: deliver, then hold the reply
        // until the client draws. Delivery is immediate; only the *reply* is
        // parked, so the loop keeps running — which it must, because the frame
        // being waited for can only arrive through it.
        Command::Key { chords, reply } => settle(state, reply, |state| state.deliver_key(&chords)),
        Command::Write { text, reply } => settle(state, reply, |state| state.deliver_text(&text)),
        Command::Pointer { to, target, reply } => {
            settle(state, reply, |state| state.deliver_pointer(to, target));
        }
        Command::Click {
            at,
            button,
            target,
            reply,
        } => settle(state, reply, |state| {
            state.deliver_click(at, button, target)
        }),
        Command::Scroll { dx, dy, reply } => {
            settle(state, reply, |state| state.deliver_scroll(dx, dy));
        }
        Command::Resize {
            size,
            target,
            reply,
        } => settle(state, reply, |state| state.deliver_resize(size, target)),

        // No events to deliver — only a fact to watch for. The reply is parked
        // the same way, and the registry answers it from wherever that fact can
        // change.
        Command::Wait { condition, reply } => state.park_wait(condition, reply),

        // Owning a selection is bookkeeping and answers at once. Reading one
        // usually cannot: the client writes down a pipe whenever it likes, so
        // the reply is parked on a transfer the loop drives.
        Command::ClipboardSet {
            selection,
            text,
            reply,
        } => {
            state.set_selection(selection, text);
            let _ = reply.send(Ok(()));
        }
        Command::ClipboardGet { selection, reply } => state.read_selection(selection, reply),
    }
}

/// Deliver events, then park `reply` on the frame they should provoke.
///
/// A delivery that fails answers straight away — there is no frame to wait for
/// if nothing was sent — which is also what keeps a mistyped chord from turning
/// into a settle timeout.
fn settle(
    state: &mut State,
    reply: oneshot::Sender<Result<(), Failure>>,
    deliver: impl FnOnce(&mut State) -> Result<WlSurface, Failure>,
) {
    match deliver(state) {
        Ok(surface) => state.settle_on(surface, reply),
        Err(failure) => {
            let _ = reply.send(Err(failure));
        }
    }
}
