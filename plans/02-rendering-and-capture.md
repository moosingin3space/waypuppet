# 02 — Rendering and capture

**Status:** not started
**Depends on:** [01](01-compositor-core.md)

## Goal

The compositor draws the window into an offscreen buffer and writes that buffer to disk as
a PNG. This is the capability the whole project exists for.

## Scope

- Bring up EGL headless: pick a device from `waypuppet_host::probe` — hardware first,
  software fallback — build an `EGLDisplay` and `EGLContext` from it, then a
  `GlesRenderer`.
- A gbm device on the same render node, for allocating the render target and for the
  dmabuf global below.
- Render target via `Offscreen::create_buffer`, then `Bind` it. Render the `Space` into it
  each time a frame is needed.
- Readback: `ExportMem::copy_framebuffer` gives a `TextureMapping`; map it, and encode the
  pixels with the `png` crate. Mind the format — the renderer's `Fourcc` is not
  necessarily RGBA in the order png wants.
- `zwp_linux_dmabuf_v1`, advertising the renderer's dmabuf formats with default feedback.
  **Not optional:** a Skia-backed client renders through EGL and will not produce frames
  without somewhere to put its buffers. `wl_shm` import stays supported for simple clients.
- Frame callbacks. Nothing has committed a second frame until it has been told the first
  one was presented, so a compositor that never sends `wl_surface.frame` done events makes
  every animated client look frozen.
- Render on demand rather than on a clock: a capture request draws a frame. Determinism is
  a feature, and a free-running repaint loop quietly costs it.

## Done when

- A `wl_shm` test client that fills its surface with a known colour is hosted, captured,
  and the PNG on disk has the expected dimensions and that colour at its centre. This is
  the end-to-end test the rest of the project leans on, so it is worth building properly.
- The same works with the software EGL device, so a machine with no render node still
  passes.

Deliberately not a criterion: a real toolkit rendering. `example-app-app` needs more than this
phase provides, and the first honest attempt at it belongs after input ([04](04-input.md))
when there is something to do with the window once it appears.

## Notes

- **`probe` currently reports the card node, not the render node.** On this setup it
  returns `/dev/dri/card1` and a software device, while `/dev/dri/renderD128` also exists.
  Card nodes need privileges that render nodes do not, so device selection should prefer a
  `renderD*` node and fall back to a card node only if there is nothing else. Fix this when
  wiring EGL up rather than living with it.
- **`unsafe_code` is denied workspace-wide** and headless EGL setup is the most likely
  place to need a raw handle. Try the safe constructors first; if one is genuinely
  unavoidable, relax the lint at the narrowest possible scope with a comment saying which
  invariant makes it sound — do not lift it for the crate.
- Scale: capture at the output's physical size, and record the scale in the response.
  Comparing a 2x capture against a 1x golden image is a confusing way to fail.
- Software rendering through llvmpipe works and is sometimes all a container gets. It is
  slow enough that timeouts tuned on hardware will be wrong there.
