# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this
repository.

## Project

waypuppet runs a GUI application inside a headless Wayland compositor and exposes it — as
screenshots, as synthetic input, as an accessibility tree — over a Unix socket and a CLI.
See [README.md](README.md) for the user-facing pitch.

It exists because an agent working on [example-app](../example-app) could build the app and launch
it, but had no way to see whether anything rendered. waypuppet is that missing sense.

**waypuppet is itself a compositor.** That is the central fact, and most design questions
resolve by asking what a compositor already knows. It knows when a client commits a buffer,
so `wait --frames 2` is exact. It owns the seat, so injected input travels the same path
real input would. It draws the frame, so a capture is a specific frame rather than a race.
Whenever a feature could be built either inside the compositor or by scripting around it,
build it inside.

### Crates the design is built on

- **`smithay` 0.7** — the compositor framework. Provides the wayland-server protocol
  implementations, `desktop::Space`/`Window` for window management, the EGL/GLES backend,
  and the seat with `KeyboardHandle`/`PointerHandle`. Its own `anvil` example is the
  reference compositor; read it before writing new protocol handling.
- **`calloop` 0.14** — smithay's event loop, and the only one in the process.

## Workspace layout

```
crates/
  waypuppet-proto   # control-protocol types (serde). No I/O.
  waypuppet-host    # the compositor and the session it hosts.
  waypuppet         # the `waypuppet` binary: `serve`, plus every client verb.
```

The load-bearing seams are the capture path (GLES offscreen render → `ExportMem` →
PNG) and input injection (CLI verb → seat → client). Expect most complexity there.

## Design decisions

These are the ones that will look arbitrary later, so they are written down.

### No async runtime

calloop drives everything in `waypuppet-host` — the Wayland display, the control socket,
timers, the child process. There is no tokio anywhere. Mixing an async runtime with
smithay's callback-driven loop means bridging two schedulers for no benefit, since the
whole process is one thread servicing one client. The CLI side is plain blocking
`std::os::unix::net::UnixStream`. Keep it that way; if something seems to need tokio, it
probably needs a calloop event source instead.

### Captures are paths, not payloads

`Response::Shot` carries a filesystem path, and the image is written as a PNG. Keeping
bytes out of the protocol keeps every message greppable, and it matches the consumer: a
coding agent ingests an image by reading a file.

### Mutating verbs settle before returning

`click`, `key`, `type`, `resize` and friends do not return the moment the event is sent.
They return once the client has committed a frame in response, or the settle timeout
elapses. This is the whole reason waypuppet exists rather than a shell script with
`sleep 0.5` in it, so it belongs in the host, not in each caller.

### smithay features

Default features are off; the selection and its reasoning live on
`[workspace.dependencies.smithay]` in the root `Cargo.toml`. Two things to know: the
defaults pull `libinput` and `libseat`, neither of which is in the Flatpak SDK, and
`default-features = false` has to sit on the *workspace* dependency because cargo forbids a
member from turning off what the workspace left on.

### CLI first, MCP maybe

The CLI is the product. It composes with shell loops, it prints a path that
`Read` can ingest directly, and it needs no server lifecycle to manage while developing.
The control socket is newline-delimited JSON, so an MCP server would be a thin shim over
the same protocol — write one if the CLI proves awkward in practice, not before.

### Determinism is a feature

Fixed output size, fixed scale, one client, nothing else on the desktop. Anything that
introduces nondeterminism into a capture — a cursor that blinks on wall-clock time, a
compositor animation, a variable frame deadline — is a bug, because it is what stands
between waypuppet and golden-image tests.

## Commands

```
just              # list recipes
just setup        # install the SDK and pinned tools. Once, after checkout.
just doctor       # what can waypuppet render with. Run first in a new container.
just check        # clippy + fmt + tests. Run before opening a PR.
just build
just test
just cli ARGS     # run the binary: `just cli shot -o /tmp/o.png`. No `--`.
just shell        # interactive shell in the SDK, toolchain active
```

Every recipe runs inside the Flatpak SDK — see [Toolchain](#toolchain). For anything
ad-hoc, including a single test, use `just shell` and run cargo in there.

Do **not** run bare `cargo` on the host; it fails, and the reason is below.

## Toolchain

Two layers, the same split as example-app:

- **The Flatpak SDK** (`org.freedesktop.Sdk//25.08`) supplies *system libraries only* —
  libwayland-server, EGL, GBM, libdrm and xkbcommon, plus a C toolchain for gbm's
  bindings. This host has none of them as linkable `-devel` symlinks, and being an
  rpm-ostree system, installing them means a reboot.
- **mise** (`mise.toml`) supplies the *toolchain* — `just` and `rust`.

Recipes run mise **inside** the sandbox: the Justfile mounts mise's installs plus the
rustup and cargo homes, and calls `mise exec`. `mise.toml` is the single source of truth
for the compiler; the SDK's own rust extension goes unused.

Consequences worth knowing:

- **Bare `cargo` on the host fails** at the link step. This is expected.
- `CARGO_TARGET_DIR` is `target/flatpak` inside the sandbox, so it never collides with a
  stray host build.
- `--device=dri` is load-bearing: without a render node there is nothing to render with.
  `just doctor` is the fastest way to tell a missing device apart from a missing library.
- The sandbox deliberately does **not** pass `--socket=wayland`. waypuppet has to work with
  no compositor to nest inside, and developing it that way is how that stays true. `just
  nested` opts back in for side-by-side debugging.

## Conventions

Carried from [example-app](../example-app/CLAUDE.md), the reference for house style:

- Prefer rich types (enums) over strings or integers, so the compiler checks us.
- Workspace lints inherited per-crate via `[lints] workspace = true`: `warnings`,
  `missing_docs`, and `unsafe_code` denied, plus clippy `all` + `pedantic`. Write docs and
  satisfy pedantic clippy as you go.
- `mod_module_files` is denied: use `foo.rs` alongside `foo/`, never `foo/mod.rs`.
- Edition 2024, resolver 3, shared `[workspace.package]` metadata.
- Test for classes of problems over examples: **`arbtest`** for property and model/oracle
  tests, **`expect-test`** for fixture-driven example tests. Neither is wired up yet.
- For crate information, [lib.rs](https://lib.rs) carries the same content as crates.io
  without requiring JavaScript, so it is easier to fetch. docs.rs example code is often the
  fastest way to get an API exactly right.

### Testing a compositor

waypuppet's own tests should not need a GUI application. Write a minimal `wl_shm` client
that fills its surface with a known colour, host it, capture, and assert on pixels — that
exercises the whole path end to end and fails legibly. `unsafe_code` is denied, so if
headless EGL setup turns out to need raw handles, relax the lint at the narrowest possible
scope with a comment saying why.

## Current state

Working: the workspace, the control-protocol types, the full CLI surface (parsing and help
text), and `waypuppet doctor`, which enumerates EGL devices.

Not written yet, in the order it should land:

1. **The compositor core.** A calloop loop, a `wl_display`, and the globals a client needs
   to start: `wl_compositor`, `wl_subcompositor`, `wl_shm`, `wl_seat`, `wl_output`,
   `xdg_shell`. Enough for a client to connect and map a window.
2. **Rendering and capture.** EGL on the render node picked by `probe`, a `GlesRenderer`
   bound to an offscreen buffer via `Offscreen::create_buffer`, `Space::render_elements`
   into it, then `ExportMem::copy_framebuffer` and a PNG write. Add
   `zwp_linux_dmabuf_v1` here — a Skia-backed client will not render without it.
3. **The control socket.** A calloop `Generic` source over a `UnixListener`, JSONL in and
   out, plus process spawning with stdout/stderr capture. This is what makes `run`, `ps`,
   `shot` and `logs` real.
4. **Input.** `KeyboardHandle::input` and `PointerHandle` for `key`, `type`, `click`,
   `move`, `scroll`, with an xkb keymap for chord parsing. Then settle-on-frame, which is
   what makes them trustworthy.
5. **`wait`.** The conditions in `waypuppet_proto::Condition`, backed by the commit
   bookkeeping that step 2 already needs.
6. **Clipboard and resize.** `wl_data_device` and `zwp_primary_selection`, plus
   `xdg_toplevel` reconfigure. These are what example-app, a terminal application, will
   actually exercise.
7. **The accessibility tree.** Run a session D-Bus, let the client register its AT-SPI
   tree, and expose it through `tree`. Then selectors — `click --on 'button[name="Save"]'`
   — which is the point of having it.

Deliberately out of scope for now: XWayland, multiple outputs, video recording,
touch/gesture input beyond the basics.

## Version control

The repo is **colocated git + jj** — `.git` and `.jj` both exist, and the user drives it
with [jj](https://jj-vcs.github.io). `git status` can look misleading; use `jj log`,
`jj status`, and `jj diff`. Do not run `git commit` here — it will fight the jj working
copy.
