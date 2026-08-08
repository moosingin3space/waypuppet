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
//! [`probe`] reports what there is to render with. [`Compositor`] runs far
//! enough for a client to connect and map a window; it does not draw anything
//! yet. See `plans/` for what lands next.

#![forbid(unsafe_code)]

mod state;

use std::{path::PathBuf, time::Duration};

use smithay::{
    backend::egl::EGLDevice,
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic},
        wayland_server::Display,
    },
    wayland::socket::ListeningSocketSource,
};
use waypuppet_proto::Size;

pub use crate::state::State;

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
    /// The feature exists in the design but not yet in the code.
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
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
    /// Which DRM render node to render on. `None` picks the first device that
    /// [`probe`] reports, preferring hardware over software.
    pub render_node: Option<PathBuf>,
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
            render_node: None,
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

        let state = State::new(display_handle, config);

        let socket = ListeningSocketSource::new_auto()
            .map_err(|error| Error::Compositor(error.to_string()))?;
        let socket_name = socket.socket_name().to_string_lossy().into_owned();

        let handle = event_loop.handle();
        handle
            .insert_source(socket, |stream, (), state: &mut State| {
                if let Err(error) = state
                    .display_handle
                    .insert_client(stream, std::sync::Arc::new(state::ClientState::default()))
                {
                    tracing::warn!(%error, "could not add wayland client");
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

        Ok(Self {
            event_loop,
            display,
            state,
            socket_name,
        })
    }

    /// The name of the Wayland socket clients should connect to.
    ///
    /// This is what goes in the child process's `WAYLAND_DISPLAY`.
    #[must_use]
    pub fn socket_name(&self) -> &str {
        &self.socket_name
    }

    /// Run until something asks the compositor to stop.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Compositor`] if the event loop or client dispatch
    /// fails.
    pub fn run(&mut self) -> Result<(), Error> {
        while self.state.running {
            self.event_loop
                .dispatch(Some(Duration::from_millis(16)), &mut self.state)
                .map_err(|error| Error::Compositor(error.to_string()))?;

            self.display
                .dispatch_clients(&mut self.state)
                .map_err(|error| Error::Compositor(error.to_string()))?;
            self.display
                .flush_clients()
                .map_err(|error| Error::Compositor(error.to_string()))?;

            self.state.space.refresh();
        }
        Ok(())
    }
}
