# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this
repository.

## Project

waypuppet runs a GUI application inside a headless Wayland compositor and exposes it — as
screenshots, as synthetic input, as an accessibility tree — over an RPC socket and a CLI.
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
- **`calloop` 0.14** — smithay's event loop, driving the compositor thread.
- **`tarpc` 0.37** — the control protocol, over a Unix socket framed with bincode. The
  service is the `Waypuppet` trait in `waypuppet-proto`; deadlines and cancellation come
  from the framework.
- **`tokio` 1** — runs the RPC server on its own thread.

## Workspace layout

```
crates/
  waypuppet-proto   # the `Waypuppet` service and its types. No I/O.
  waypuppet-host    # the compositor and the session it hosts.
  waypuppet         # the `waypuppet` binary: `serve`, plus every client verb.
```

"Host" here means the thing hosting the application under test, not the machine anything
runs on.

The load-bearing seams are the capture path (GLES offscreen render → `ExportMem` →
PNG) and input injection (CLI verb → seat → client). Expect most complexity there.

## Design decisions

These are the ones that will look arbitrary later, so they are written down.

### Two loops, one bridge

The compositor thread runs calloop and owns every piece of Wayland state. The control
thread runs tokio and serves RPC. They meet over an `mpsc` channel of commands, each
carrying a `oneshot` sender for its reply; the compositor wakes on a calloop channel source
and never blocks.

The rule that matters: **the compositor loop must never block on a request**, because
several calls wait on the very client the loop is there to service. Async makes that
natural rather than delicate — a handler holds its `oneshot` and awaits, so a parked `wait`
costs a suspended task instead of a stalled compositor.

This replaces an earlier "one calloop, no async" design. Owning the loop end to end looked
simpler, but it meant hand-rolling request framing, correlation, deadlines, cancellation
and long-polling — reimplementing an RPC library, worse. See "The protocol is a Rust trait".

### Waiting is bounded, and failures say which kind they are

waypuppet cannot promise never to hang. It can promise to explain, and that is the design
target: **every wait carries a deadline, and every deadline produces an error that names
what went wrong.**

Three failures look identical from the outside and want different fixes:

| | Failure | Whose bug |
|---|---|---|
| **A** | the compositor loop is not being driven | waypuppet's |
| **B** | waiting for something that can no longer happen | waypuppet's |
| **C** | the loop is running, the client is not drawing | the application's |

Distinguishing them is worth real effort, because C is a normal outcome — a slow or broken
application under test, reported as `NotSettled` — while A and B are defects in waypuppet
that would otherwise hide behind the same "timed out" message. A monotonic tick counter on
the compositor loop separates them exactly: if it has not advanced, the loop is stuck; if
it has, the client is.

Two things make this cheap rather than laborious:

- **Structure beats detection.** A dropped `oneshot::Sender` wakes its receiver
  immediately, so if parked requests are held such that the only way to keep waiting is for
  someone to keep the sender alive, a compositor thread that dies cancels every outstanding
  request for free. Tie a parked sender's lifetime to the condition still being *possible*,
  and B mostly stops existing.
- **There is no lock cycle to find.** This is not deadlock in the textbook sense, so
  lock-order detectors have nothing to look at. The wait-for edge is "waiting on somebody
  else's event loop to run", which no such tool can see. Do not reach for one.

### The protocol is a Rust trait

`waypuppet_proto::Waypuppet` is a `#[tarpc::service]`, so client and server are checked
against one definition and there is no schema in another language and no codegen step.
Deadlines propagate and a departing client cancels its outstanding work, both of which
matter because `wait` and `logs` are long-lived by design.

Bincode is not self-describing, so no type on the wire may be a
`serde_json::Value`-shaped escape hatch — it cannot round-trip. The accessibility tree is
the obvious temptation; it is `proto::Node`, a real type, and better for it.

### Captures are paths, not payloads

`Capture` carries a filesystem path and the image is written as a PNG. Keeping pixels out
of the protocol matches the consumer: a coding agent ingests an image by reading a file.

### Mutating verbs settle before returning

`click`, `key`, `type`, `resize` and friends do not return the moment the event is sent.
They return once the client has committed a frame in response, or the settle timeout
elapses. This is the whole reason waypuppet exists rather than a shell script with
`sleep 0.5` in it, so it belongs in the compositor, not in each caller.

### smithay features

Default features are off; the selection and its reasoning live on
`[workspace.dependencies.smithay]` in the root `Cargo.toml`. Two things to know: the
defaults pull `libinput` and `libseat`, neither of which is in the Flatpak SDK, and
`default-features = false` has to sit on the *workspace* dependency because cargo forbids a
member from turning off what the workspace left on.

### CLI first, MCP maybe

The CLI is the product. It composes with shell loops, it prints a path that `Read` can
ingest directly, and it needs no server lifecycle to manage while developing. An MCP server
would be another `WaypuppetClient` over the same service — write one if the CLI proves
awkward in practice, not before.

### The CLI hides the sandbox

A caller should never have to know that waypuppet runs in a container, which flags that
container needs, or that their application needs the same one. `waypuppet run -- ./my-app`
is the whole interface; working out how to make that true is
[plan 08](plans/08-sandbox-autonomy.md). Anything that pushes container mechanics back onto
the caller is a bug in waypuppet, not something for them to configure.

### anvil is a reference, not a template

smithay's `anvil` is the compositor to read before writing new protocol handling, but do
not copy it verbatim. Several of its `unsafe` blocks are structural rather than essential —
its display dispatch puts `Display` inside a calloop source and reaches back in with an
unsafe `get_mut`, where registering a clone of the poll fd as a wake-only source and
dispatching in the run loop is equivalent and safe. anvil also carries XWayland, multiple
backends and a dozen protocols waypuppet does not want. Take the shape, check the details.

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

Every recipe builds inside the Flatpak SDK — see [Toolchain](#toolchain). For anything
ad-hoc, including a single test, use `just shell` and run cargo in there. Bare `cargo`
outside the sandbox is unsupported: whether it links depends on what a given machine has
installed, which is exactly the variable the SDK exists to remove.

## Toolchain

Two layers, the same split as example-app:

- **The Flatpak SDK** (`org.freedesktop.Sdk//25.08`) supplies *system libraries only* —
  libwayland-server, EGL, GBM, libdrm and xkbcommon, plus a C toolchain for gbm's bindings.
  Pinning them to the SDK means the build does not depend on which distribution a
  contributor runs, whether its `-devel` packages are installed, or whether installing them
  is even convenient — on an image-based system it can mean a reboot.
- **mise** (`mise.toml`) supplies the *toolchain* — `just` and `rust`.

Recipes run mise **inside** the sandbox: the Justfile mounts mise's installs plus the
rustup and cargo homes, and calls `mise exec`. `mise.toml` is the single source of truth
for the compiler; the SDK's own rust extension goes unused.

Consequences worth knowing:

- `CARGO_TARGET_DIR` is `target/flatpak` inside the sandbox, so sandbox artifacts never
  collide with a build done any other way.
- `--device=dri` is load-bearing: without a render node there is nothing to render with.
  `just doctor` is the fastest way to tell a missing device apart from a missing library.
- The sandbox deliberately does **not** pass `--socket=wayland`. waypuppet has to work with
  no compositor to nest inside, and developing it that way is how that stays true. `just
  nested` opts back in for side-by-side debugging.
- This is how waypuppet is *developed*. How a user's application gets the same treatment
  without having to know any of it is a separate problem —
  [plan 08](plans/08-sandbox-autonomy.md).

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

waypuppet's own tests do not need a GUI application. `tests/fixture.rs` is a minimal
`wl_shm` client that fills its surface with a known colour; the tests host it, capture, and
assert on pixels, which exercises the whole path end to end and fails legibly.

**Go through `Harness`.** It owns the compositor and the client together, and every method
on it that waits also ticks. That is not a convenience — it is what makes the hang
described above unrepresentable, because there is no way to reach a blocking client
operation except through a call already driving the compositor. Adding a fixture method
that blocks without ticking reintroduces the bug.

Wrap tests in `fixture::guarded`, which runs the body under a watchdog and aborts with the
name of the test and what it was waiting on. It is a backstop for waits the harness does
not mediate; the harness's own limit is much shorter, so the precise failure normally wins.

## Current state

Working: the workspace, the `Waypuppet` service definition, the full CLI surface (parsing
and help text), `waypuppet doctor`, and — as of plan 01 — a compositor a real Wayland
client can connect to and map a window on. `waypuppet serve` runs it; `cargo test -p
waypuppet-compositor` proves it against a `wayland-client` test client.

Nothing is drawn yet, and nothing can be driven from outside the process.

**The roadmap lives in [`plans/`](plans/)** — one file per phase, in the order they should
land, each with its own acceptance criteria and, once landed, what it taught us.
[`plans/README.md`](plans/README.md) is the index and also records what is deliberately out
of scope. Public documentation lives in [`docs/`](docs/), including how CI works here and
how a consumer wires waypuppet into theirs.

Keep it that way: plans are working documents that change as reality intrudes, and this
file is for guidance that outlives them. When a phase lands, update its plan's status and
move anything durable it taught us into "Design decisions" above.

## Version control

If the use has colocated `jj` in this repository (visible with the presence of a `.jj/` directory),
prefer `jj` for interacting with the repository.
