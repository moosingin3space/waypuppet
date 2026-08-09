# 02 — Rendering and capture

**Status:** done
**Depends on:** [01](01-compositor-core.md)

## Goal

The compositor draws the window into an offscreen buffer and writes that buffer to disk as
a PNG. This is the capability the whole project exists for.

## Scope

- Bring up EGL headless: pick a device from `waypuppet_compositor::probe` — hardware first,
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

## What dmabuf landed

`Renderer` now exposes its importable formats, its render node and an `import_dmabuf`, and
`State` advertises `zwp_linux_dmabuf_v1` from them — version 4 with default feedback where
there is a node to name, version 3 otherwise. `DmabufHandler::dmabuf_imported` imports the
buffer and answers the `ImportNotifier` with the result, so a client learns at the moment
it asks rather than by getting a blank window. `bind_wl_display` is called alongside, which
is what the `use_system_lib` feature was in the list for.

`tests/dmabuf.rs` allocates a linear gbm buffer on a render node, writes a colour into it,
exports it, hands the descriptors over the protocol, and asserts the capture shows that
colour. A second test paints the same colour through both `wl_shm` and dmabuf and requires
the two captures to be byte-identical, so a golden image is valid whichever kind of client
produced it.

## What `--software` landed

`Config::render_node: Option<PathBuf>` became `Config::rendering: Rendering` — `Fastest`,
`Software`, or `Device(path)` — and `--software` on `run` and `serve` selects the middle
one. `run --detach` forwards it, because a detached session quietly rendering on the GPU
while the foreground one used llvmpipe is precisely the bug the flag exists to prevent.

It is a filter, not a preference: a machine with no llvmpipe fails to start rather than
falling back, and the "nothing matched" error names which of the three requests went
unsatisfied, since the fixes are entirely different.

`tests/capture.rs` and `tests/dmabuf.rs` now run every case twice, once on `Fastest` and
once on `Software`, which closes the "explicitly exercise the software device" item. On
this machine llvmpipe does import dmabufs — `EGL_EXT_image_dma_buf_import` is present — so
the software dmabuf path is real rather than assumed.

## Still to do

Nothing.

## What we learned

- **The renderer had to stop being lazy.** It was built on first capture, so that a session
  whose EGL is unusable still served the protocol. The dmabuf global advertises *the
  renderer's* formats and has to exist before the client looks at the registry, so the
  renderer now comes up in `State::new` and is held as `Result<Renderer, String>`. The
  property that motivated the deferral survives — a session with no EGL still answers
  everything but `shot` — and it now says so at startup rather than at the first capture.

- **A test that skips can swallow the bug it was written for.** The first version of
  `tests/dmabuf.rs` treated any failure from the client half as "this machine cannot
  allocate" and returned early. Mutating `dmabuf_imported` to reject every buffer made the
  compositor post a protocol error, the client's roundtrip failed, and the test *skipped* —
  passing against a compositor that accepted nothing. The fixture now draws the line at the
  moment the buffer is handed over: everything before it can be an environment failure,
  everything after it is waypuppet's and panics. The test no longer skips at all, which is
  right, because the rest of the suite already needs a rendering device.

- **Only the test that needs the global should require it.** Binding
  `zwp_linux_dmabuf_v1` unconditionally in the shared fixture coupled `handshake`,
  `control` and `input` — none of which need to render — to EGL coming up. It is bound
  optionally, and `paint_dmabuf` is the only thing that complains when it is missing.

- **A test that asks for a renderer has to check it got one.** Running the same body under
  `Rendering::Software` proves nothing on its own: if the flag silently fell back, the test
  would pass on the GPU and the software path would be untested while looking covered.
  `Harness::renders_in_software` makes the assertion possible, and mutating `Software` from
  a filter into a preference fails exactly those tests. This is the same shape as the
  dmabuf skip: a test whose subject can quietly change is a test that cannot fail.

- **The `unsafe` exception is real, and it is narrow.** `EGLDisplay::new` and
  `GlesRenderer::new` are both `unsafe fn`. The crate's `#![forbid(unsafe_code)]` had to
  become the workspace-level `deny`, because `forbid` cannot be overridden even locally.
  The `#[expect(unsafe_code, reason = …)]` in `Renderer::on_device` discharges a structural
  obligation: `_display` is owned by the struct and outlives everything built from it.

  There is now a second, in `tests/fixture.rs`, for the `memfd_create` / `F_ADD_SEALS` /
  udmabuf ioctl sequence that lets a client produce a dmabuf with no GPU. Two is still a
  number worth keeping countable — if a third is ever proposed, it should have to argue
  against these.
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

