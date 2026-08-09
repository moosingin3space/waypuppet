//! Offscreen rendering and readback: a committed frame becomes a PNG.
//!
//! Nothing here touches a screen. EGL runs on a DRM device with no surface, the
//! `Space` is composited into an offscreen renderbuffer, and the result is
//! copied back to CPU memory and encoded.
//!
//! # Rendering is on demand
//!
//! There is no repaint loop. A frame is drawn because something asked for a
//! capture, which is what makes two captures of the same state identical —
//! determinism is a feature, not an accident of timing.

use std::path::Path;

use smithay::{
    backend::{
        allocator::{Fourcc, dmabuf::Dmabuf, format::FormatSet},
        drm::DrmNode,
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            Bind, Color32F, ExportMem, ImportDma, ImportEgl, Offscreen,
            damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement,
            gles::{GlesRenderbuffer, GlesRenderer},
        },
    },
    desktop::{Space, Window, space::render_output},
    output::Output,
    reexports::wayland_server::DisplayHandle,
    utils::{Buffer as BufferCoords, Rectangle, Size},
};

use crate::{Error, RenderDevice};

/// A device EGL knows about, paired with what we can say about it publicly.
struct Candidate {
    /// What `doctor` reports.
    info: RenderDevice,
    /// The handle EGL needs. Kept out of [`RenderDevice`] so the public type
    /// stays `Debug` and `Clone` and carries no resources.
    egl: EGLDevice,
}

/// A captured frame, in memory.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
    /// Tightly packed RGBA, 8 bits per channel, top row first.
    pub pixels: Vec<u8>,
}

impl Frame {
    /// The pixel at `(x, y)` as `(r, g, b, a)`.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<(u8, u8, u8, u8)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let start = ((y * self.width + x) * 4) as usize;
        let bytes = self.pixels.get(start..start + 4)?;
        Some((bytes[0], bytes[1], bytes[2], bytes[3]))
    }

    /// Encode as a PNG at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Capture`] if the file cannot be written or encoded.
    pub fn write_png(&self, path: &Path) -> Result<(), Error> {
        let file = std::fs::File::create(path)
            .map_err(|error| Error::Capture(format!("{}: {error}", path.display())))?;
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .and_then(|mut writer| writer.write_image_data(&self.pixels))
            .map_err(|error| Error::Capture(error.to_string()))
    }
}

/// The GPU (or llvmpipe) side of the compositor.
#[expect(
    missing_debug_implementations,
    reason = "none of smithay's renderer types are Debug"
)]
pub struct Renderer {
    gles: GlesRenderer,
    damage: OutputDamageTracker,
    /// The render node this renderer is on, when it has one. Software devices
    /// do not, which is why it is optional rather than assumed.
    node: Option<DrmNode>,
    /// Kept alive because the context borrows it. Dropping it early tears the
    /// context out from under the renderer.
    _display: EGLDisplay,
}

impl Renderer {
    /// Bring up EGL on a suitable device and build a GLES renderer.
    ///
    /// Devices are tried in the order [`select`] returns them, because
    /// enumerating a device says only that EGL knows about it — whether a
    /// container is actually allowed to open it is a different question, and
    /// the only way to find out is to try.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoRenderDevice`] if every candidate fails.
    pub fn new(output: &Output, preferred: Option<&Path>) -> Result<Self, Error> {
        let candidates = select(preferred)?;
        if candidates.is_empty() {
            return Err(Error::NoRenderDevice(
                "EGL reported no devices; the container is probably missing --device=dri"
                    .to_owned(),
            ));
        }

        let mut failures = Vec::new();
        for candidate in candidates {
            let info = candidate.info.clone();
            match Self::on_device(candidate.egl, output) {
                Ok(renderer) => {
                    tracing::info!(
                        path = ?info.path,
                        software = info.software,
                        "rendering device selected",
                    );
                    return Ok(renderer);
                }
                Err(error) => {
                    tracing::debug!(path = ?info.path, %error, "device unusable");
                    failures.push(format!("{:?}: {error}", info.path));
                }
            }
        }
        Err(Error::NoRenderDevice(failures.join("; ")))
    }

    /// Build a renderer on one specific device.
    fn on_device(device: EGLDevice, output: &Output) -> Result<Self, Error> {
        // Read before the device is consumed below. A software device has no
        // node, which is not an error — it only means there is nothing to name
        // in dmabuf feedback.
        let node = device.try_get_render_node().ok().flatten();

        // Both EGL entry points are `unsafe` in smithay. The obligation in each
        // case is that the handle is not used after the display is destroyed,
        // which is structural here: `_display` is owned by this struct and
        // outlives the context and renderer it is passed to, and neither escapes.
        //
        // The workspace denies `unsafe_code`; this is the narrowest scope that
        // can hold the exception, and it is the only one in the crate.
        #[expect(
            unsafe_code,
            reason = "smithay's EGL constructors are unsafe; the display outlives everything built from it"
        )]
        let (display, gles) = unsafe {
            let display = EGLDisplay::new(device)?;
            let context = EGLContext::new(&display)?;
            let gles =
                GlesRenderer::new(context).map_err(|error| Error::Render(error.to_string()))?;
            (display, gles)
        };

        Ok(Self {
            gles,
            damage: OutputDamageTracker::from_output(output),
            node,
            _display: display,
        })
    }

    /// The dmabuf formats this renderer can import.
    ///
    /// What goes in the `zwp_linux_dmabuf_v1` global: a client picks from this
    /// list, so anything advertised here has to be something
    /// [`Renderer::import_dmabuf`] will actually accept.
    #[must_use]
    pub fn dmabuf_formats(&self) -> FormatSet {
        self.gles.dmabuf_formats()
    }

    /// The render node this renderer is on, if it has one.
    #[must_use]
    pub fn node(&self) -> Option<DrmNode> {
        self.node
    }

    /// Let EGL answer for `wl_drm` buffers on this display.
    ///
    /// The modern path is `zwp_linux_dmabuf_v1`; this is the older one, which
    /// some client stacks still look for before they will use hardware buffers
    /// at all. It is why `use_system_lib` is in the smithay feature list.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Render`] if the EGL implementation cannot bind a
    /// Wayland display, which is not fatal — dmabuf still works without it.
    pub fn bind_wl_display(&mut self, display: &DisplayHandle) -> Result<(), Error> {
        self.gles
            .bind_wl_display(display)
            .map_err(|error| Error::Render(error.to_string()))
    }

    /// Import a client's dmabuf, so it can be composited.
    ///
    /// The texture is dropped immediately: this is the compositor answering
    /// "can you actually use this buffer?" at the moment the client asks, which
    /// is the point at which the protocol lets us say no. Rendering imports it
    /// again through the same cache when the surface is drawn.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Render`] if the buffer cannot be imported.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> Result<(), Error> {
        self.gles
            .import_dmabuf(dmabuf, None)
            .map(|_texture| ())
            .map_err(|error| Error::Render(error.to_string()))
    }

    /// Composite the space into an offscreen buffer and read it back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Render`] if allocation, rendering or readback fails.
    pub fn capture(&mut self, output: &Output, space: &Space<Window>) -> Result<Frame, Error> {
        let mode = output
            .current_mode()
            .ok_or_else(|| Error::Render("the output has no mode".to_owned()))?;
        let size: Size<i32, BufferCoords> = (mode.size.w, mode.size.h).into();

        let mut target: GlesRenderbuffer = self
            .gles
            .create_buffer(Fourcc::Abgr8888, size)
            .map_err(|error| Error::Render(error.to_string()))?;

        // `bind` borrows the target, not the renderer, so the renderer is free
        // again once this returns — which is what lets the readback below take
        // `&mut self.gles` while the framebuffer is still alive.
        let mut framebuffer = self
            .gles
            .bind(&mut target)
            .map_err(|error| Error::Render(error.to_string()))?;

        // `age: 0` means "assume nothing about what is already in this buffer",
        // so the whole frame is redrawn. The buffer is freshly allocated every
        // capture, so that is the truth rather than a pessimisation.
        render_output::<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>, Window, _>(
            output,
            &mut self.gles,
            &mut framebuffer,
            1.0,
            0,
            [space],
            &[],
            &mut self.damage,
            Color32F::from([0.0, 0.0, 0.0, 1.0]),
        )
        .map_err(|error| Error::Render(error.to_string()))?;

        // DRM names formats most-significant-byte first within a little-endian
        // 32-bit word, so `Abgr8888` is 0xAABBGGRR — which in memory is R, G, B,
        // A, exactly what PNG wants. Getting this backwards produces an image
        // that looks right but has red and blue swapped.
        let region = Rectangle::from_size(size);
        let mapping = self
            .gles
            .copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888)
            .map_err(|error| Error::Render(error.to_string()))?;
        let pixels = self
            .gles
            .map_texture(&mapping)
            .map_err(|error| Error::Render(error.to_string()))?
            .to_vec();

        Ok(Frame {
            width: mode.size.w.unsigned_abs(),
            height: mode.size.h.unsigned_abs(),
            pixels,
        })
    }
}

/// Rendering devices worth trying, best first.
///
/// Hardware before software, and if `preferred` is given, only that device.
fn select(preferred: Option<&Path>) -> Result<Vec<Candidate>, Error> {
    let mut devices: Vec<Candidate> = EGLDevice::enumerate()?
        .map(|egl| Candidate {
            info: RenderDevice {
                path: egl.drm_device_path().ok(),
                software: egl.is_software(),
            },
            egl,
        })
        .collect();

    if let Some(wanted) = preferred {
        devices.retain(|device| device.info.path.as_deref() == Some(wanted));
        return Ok(devices);
    }

    // Software rendering works and is often all a container is given, but it is
    // several times slower, so it is the fallback rather than the default.
    devices.sort_by_key(|device| device.info.software);
    Ok(devices)
}
