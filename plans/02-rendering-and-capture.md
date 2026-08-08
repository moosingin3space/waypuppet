# 02 — Rendering and capture

**Status:** in progress — the capture path works end to end; dmabuf is what remains
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

## What landed

`render.rs`: EGL brought up headless on a device chosen by trying candidates in order,
a `GlesRenderer`, an offscreen `GlesRenderbuffer` as the render target, `render_output`
compositing the `Space` into it, `ExportMem::copy_framebuffer` reading it back, and a PNG
encoder. `Compositor::capture` ties it together and sends frame callbacks afterwards.

`Compositor::tick` was added alongside, so a caller can drive the loop one iteration at a
time and do work in between. The capture test needs it, and so will plan 03's bridge.

`tests/capture.rs` proves it: a client paints `#20C040`, the compositor captures, and the
centre pixel is that colour. A second test asserts an empty session captures as opaque
black. `tests/fixture.rs` is the shared `wl_shm` client both test binaries use.

## Still to do

- **`zwp_linux_dmabuf_v1`.** Nothing renders through EGL against waypuppet yet, which means
  no real toolkit will draw. This needs a gbm device and `DmabufState` advertising the
  renderer's formats, and it is the gate on trying freya.
- Explicitly exercise the software device, rather than trusting that the fallback works
  because the hardware path does.

## What we learned

- **The `unsafe` exception is real, and it is one place.** `EGLDisplay::new` and
  `GlesRenderer::new` are both `unsafe fn`. The crate's `#![forbid(unsafe_code)]` had to
  become the workspace-level `deny`, because `forbid` cannot be overridden even locally.
  The single `#[expect(unsafe_code, reason = …)]` lives in `Renderer::on_device`, and the
  obligation it discharges is structural: `_display` is owned by the struct and outlives
  everything built from it.
- **`Bind` borrows the target, not the renderer.** `bind<'a>(&mut self, target: &'a mut T)
  -> Framebuffer<'a>` looks like it should keep the renderer borrowed, but the returned
  lifetime is the target's. That is what makes `copy_framebuffer(&mut self, &framebuffer)`
  legal while the framebuffer is alive — otherwise readback would need a second pass.
- **DRM format names are byte-order traps.** `Fourcc::Abgr8888` is `0xAABBGGRR` in a
  little-endian word, so in memory it runs R, G, B, A — which is what PNG wants. The
  fixture has the mirror-image problem: `wl_shm`'s `Argb8888` is B, G, R, A in memory. Both
  are commented at the point of use, because getting either backwards produces an image
  that looks plausible with red and blue swapped, and a grey test fixture would not catch it.
- **A roundtripping client plus an unticked compositor deadlocks.** The first capture test
  hung: the client thread kept calling `roundtrip` to stay healthy, but the main thread
  stopped ticking once it had its capture, so the roundtrip never returned and the `join`
  waited on it forever. The client now just holds the connection open.

  This is worth carrying into [plan 03](03-control-socket.md). It is the same hazard in
  miniature: anything that waits on the compositor while the compositor is not running is a
  deadlock, which is precisely why a request handler must never block the loop it is
  served by.

