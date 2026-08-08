//! The headless Wayland compositor that hosts and drives an application under
//! test.
//!
//! waypuppet is a compositor rather than a script that pokes at somebody else's
//! compositor, and every capability below follows from that. Input injection is
//! a call into the seat, not a synthetic uinput device. A capture is the frame
//! the client committed, not a race against a screenshot tool. "The window is
//! mapped" is something observed on the protocol, not inferred from a sleep.
//!
//! # Status
//!
//! Scaffolding. [`probe`] works and is what `waypuppet doctor` reports;
//! [`Config`] pins down the knobs a session takes. The compositor itself is not
//! written yet — see the roadmap in `CLAUDE.md`.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use smithay::backend::egl::EGLDevice;
use waypuppet_proto::Size;

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
