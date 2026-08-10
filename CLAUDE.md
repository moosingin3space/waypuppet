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
  waypuppet-proto        # the `Waypuppet` service and its types. No I/O.
  waypuppet-select       # the selector grammar: parse, print, resolve, validate,
                         #   and the AT-SPI role vocabulary. No I/O either.
  waypuppet-sandbox      # which runtime a session needs and how to re-enter it.
                         #   `resolve` is pure; `detect` is where the I/O is kept.
  waypuppet-compositor   # the compositor, and the session built around it:
    src/state.rs         #   protocol handlers and window bookkeeping
    src/render.rs        #   EGL, offscreen compositing, readback, PNG
    src/bridge.rs        #   commands, deadlines, the heartbeat
    src/session.rs       #   compositor thread, child process, RPC server
    src/a11y.rs          #   the private D-Bus session, and the AT-SPI registry
  waypuppet              # the `waypuppet` binary: `serve`, plus every client verb.
    src/sandbox.rs       #   the edge: resolving paths, starting a runtime
```

The load-bearing seams are the capture path (GLES offscreen render → `ExportMem` → PNG),
input injection (CLI verb → seat → client), and the bridge, which is the only place the two
threads meet.

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

The mechanism is a parked `oneshot::Sender` held in `State::settling`, answered on the next
commit *after* the events went out — not on the loop going quiet, because a client that
repaints continuously is never quiet. `Commands::settle` is the control-thread half, and it
is `ask` with exactly one substitution: a healthy loop that ran out of time is
`NotSettled`, a loop that stopped ticking is still `LoopStalled`. Keep it a wrapper, so the
heartbeat stays the single place that decides whose fault a timeout is.

### Conditions are woken by events, not swept for

`wait` parks a sender in `State::waiting` and the registry is woken from the places the
facts change: `commit`, `title_changed`, and `park_wait` itself for a condition that is
already true. It is never swept on the loop. waypuppet is the compositor — it is *told*
when a window maps or a title changes — so polling for those would be choosing not to
listen.

`Idle` is the exception, and `check_idle` on the loop is the exception's shape. Going quiet
is the *absence* of commits, so there is no event to hang it off.

Evaluating everything per tick would be fewer lines and correct to within 16ms. It was the
first design and it was dropped, because it makes the arrival check unfalsifiable: with a
per-tick sweep, deleting that check still passes every test one tick later. Keeping the
conditions event-driven is what makes "a defence you can break" possible here at all.

Two consequences worth carrying forward:

- **Resolve a condition when it arrives, not when it is checked.** `Frames(2)` becomes an
  absolute commit count against a named surface; a pattern becomes a compiled `Regex`. A
  bad pattern is then reported once, to the caller who wrote it, instead of silently
  matching nothing until the timeout.
- **A new hook is a new way to hang.** Adding a condition means finding every place its
  fact can change. `title_changed` was nearly missed: `set_title` is not double buffered,
  so a client can rename its window and never draw again, and a registry woken only by
  frames leaves that wait parked forever.

### Timing out is an outcome, not an error

`Settled::TimedOut` is a value the caller receives, not a `Failure`. "It did not happen" is
something a script may well be expecting, and it must be distinguishable from "waypuppet
fell over". On the CLI that is exit 3 — not 2, which clap already spends on usage errors.

`Commands::until` is the third wrapper around `ask`, alongside `settle`. All three are the
same wait with one substitution each, so `Heartbeat::diagnose` stays the single place that
decides whose fault an expired wait was. A stalled loop is still `LoopStalled` through every
one of them; laundering that into a clean timeout is precisely what this project refuses to
do.

### Some conditions belong to the control thread

`wait --exit` is about the child process, which the compositor has never been told about: a
process can exit without ever making a Wayland connection. `bridge::Watching` is that split,
with a `Compositor(Watched)` arm and an `Exit` arm, so neither half is ever handed a
condition it cannot see. The first draft passed all five conditions to the compositor and
needed a "cannot happen" arm — and an arm that cannot happen is an arm that answers wrongly
when it does.

### waypuppet is the accessibility registry too

The same reasoning that makes waypuppet a compositor makes it an AT-SPI registry. An
application publishes its tree by registering with whatever owns `org.a11y.atspi.Registry`;
if that is waypuppet, the tree arrives without anything else on the bus and without the
application ever touching the developer's desktop.

The bus underneath is a `dbus-daemon` where the image has one and an embedded `busd` where
it does not. That order is deliberate: `dbus-daemon` is the reference implementation and is
in most images that can run a GUI application, so preferring it means waypuppet is not also
on the hook for being a correct message bus. `busd` calls itself alpha, so it is the
fallback — and it is what makes this work in the Flatpak SDK, which has no bus at all.
`dbus-daemon` gets a config file of waypuppet's own rather than `--session`, because the
stock session config is a separate package from the binary and is often missing.

**The failure mode here has no symptom, which shapes the tests.** If the `Embed` reply
carries a well-known bus name instead of a unique one, `atspi` cannot decode it, the failure
is swallowed as recoverable, and the application registers successfully and then publishes
nothing — silently, on both sides. So `tests/tree.rs` reads a node out rather than checking
the handshake happened, and it deserialises the reply with the same type a real client uses,
because a looser one accepts the bug. The first version of that test had exactly the defect
its own docs claimed to guard against; it was caught by reintroducing the bug and watching
the test pass.

`tree` is answered entirely on the control thread. The compositor has never heard of D-Bus
and does not need to — the same split as `wait --exit`.

### A transfer is an event source, not a read

Clipboard transfers are fd-based and cooperative: reading a selection means handing the
client a pipe it fills whenever it likes, and writing one means filling a pipe the client
drains whenever it likes. Neither can happen inline on the compositor thread, so both are
calloop sources carrying their payload across wake-ups, and both fds are made non-blocking
first. `rustix` is a dependency for exactly one call — `fcntl(F_SETFL, O_NONBLOCK)` — because
`std::io::pipe` gives no way to do it and there is no acceptable amount of blocking here.

Reads are tracked in `State`; writes are not. A read has a parked reply, so an abandoned one
must be reachable for `prune_parked` to close its pipe. A write ends by itself: when the
client stops reading, it fails with `EPIPE` and unregisters. Note also that finishing a write
means *removing the source*, because dropping the fd is the EOF the reader is waiting for —
leaving it registered hangs the client instead.

The general shape: **anything cooperative belongs on the loop.** The rule that the compositor
must never block on a request covers waiting for a client to draw; it covers waiting for a
client to write down a pipe in exactly the same way and for exactly the same reason.

### Track lifecycles, not counts

`State::clients` is a `HashSet<ClientId>`, not a counter, and arrival is recorded where
waypuppet accepts the connection rather than from `ClientData::initialized`.

Both halves of that were bought with a real bug. The C-library backend — which
`use_system_lib` selects, and which waypuppet needs for `wl_drm` — **never calls
`initialized`**, though it does call `disconnected`. A count incremented in one and
decremented in the other was therefore stuck at zero while a client was connected, and
wrapped to `usize::MAX` on the first disconnect, because atomics wrap silently even in
debug. `ps` reported "starting" for a running application and "running" for an empty
session.

The general lesson is worth more than the fix: **a count is a lossy summary of a lifecycle,
and it cannot self-correct.** A set of identities can — inserting twice or removing
something absent is a no-op, so a missed or duplicated callback costs nothing. Prefer
identities wherever the two ends of a life are reported by different mechanisms.

And prefer a fact this crate observes to one it is told: `insert_client` returning `Ok` is
waypuppet accepting a client, which no backend can decline to mention.

### Input resolves against a pinned keymap

The layout is `us` and is stated in `input::LAYOUT`, not inherited from the environment:
chord parsing resolves against the keymap, so a layout that varied by machine would make
`waypuppet key at` mean different things in different places. The seat and the reverse
index are built from one `input::xkb_config()` for the same reason.

The reverse index asks an `xkb::State` under a modifier mask what each keycode produces,
rather than assuming level 0 is unshifted and level 1 is shifted. That assumption holds for
`us` and is not a promise; the state query is the same computation the client will run
against the same keymap, so the two cannot disagree.

A request is resolved in full before any of it is sent — every chord parsed, every
character checked — so a typo names itself instead of surfacing as whatever else is wrong,
and `type` is all-or-nothing. Half a string in a text field is a worse outcome than an
error.

### The pointer always has a position

When a window maps it gets keyboard focus *and* the pointer, parked at its centre. Without
a starting position, `scroll` and a `click` with no coordinates have no pointer focus to go
to: the events vanish and the call fails as `NotSettled`, which blames the application for
waypuppet's omission. The centre is arbitrary but fixed, which is what determinism needs.

### The renderer exists from startup, because dmabuf needs its formats

`State::renderer` is a `Result<Renderer, String>` built in `State::new`, not on first
capture. `zwp_linux_dmabuf_v1` advertises *that renderer's* importable formats, and a
global that appears after the client has read the registry may as well not exist — so the
renderer has to be up before the child process connects.

A session whose EGL is unusable still serves the whole protocol; it just reports that at
startup instead of at the first `shot`, and offers no dmabuf global. `DmabufHandler::
dmabuf_imported` imports each buffer and answers the `ImportNotifier` honestly, so a client
finds out at the moment it asks rather than by getting a blank window.

### Frame callbacks are pacing, not rendering

A toolkit requests a `wl_surface.frame` callback before it commits and will not draw again
until it fires. waypuppet sent them only from `capture`, on the reasoning that rendering is
on demand — which conflated two different jobs and made every real toolkit look hung: GTK
mapped a window, drew one frame, and then ignored input forever.

They are now answered from the event loop at the 60Hz the output advertises. Rendering is
still on demand, and determinism still rests on that; pacing a client is a separate
obligation that has to be met whether or not anyone is capturing.

### The `wl_shm` fixture is not a toolkit

Two bugs got past the whole suite because the test client does what it is told rather than
what a real client does: it never waits on a frame callback, and it does not need a seat to
exist before it can receive input. GTK does both. When something works in `tests/` and not
in a real application, suspect the fixture's good manners first.

The concrete case: GTK will not create a seat at all unless `wl_data_device_manager` is
advertised, so the clipboard global — filed under plan 06 as unrelated — turned out to gate
all keyboard input. It is advertised now, with default handlers and no selection support.

### Forcing software rendering is a filter, not a preference

`Config::rendering` is a `Rendering` — `Fastest`, `Software`, or `Device(path)` — and
`--software` on `run` and `serve` picks the second. It never falls back to a GPU: a machine
with no llvmpipe fails to start.

That is the whole point. llvmpipe and a GPU driver antialias and filter differently, so a
golden image is only comparable against a capture from the same renderer, and CI has no
render node. A silent fallback would make every golden wrong without anyone noticing, which
is worse than not starting. `run --detach` forwards the flag for the same reason.

Tests that ask for a renderer assert they got it, via `Harness::renders_in_software`.
Without that, a `Rendering::Software` test passes on a GPU and covers nothing.

### smithay features

Default features are off; the selection and its reasoning live on
`[workspace.dependencies.smithay]` in the root `Cargo.toml`. Two things to know: the
defaults pull `libinput` and `libseat`, neither of which is in the Flatpak SDK, and
`default-features = false` has to sit on the *workspace* dependency because cargo forbids a
member from turning off what the workspace left on.

### Selectors are CSS, and the rejections are the feature

`waypuppet-select` parses a subset of CSS. Familiarity is the obvious reason and the weaker
one; the decisive one is that **CSS's default combinator is the descendant axis**, which is
what a tree query wants. XPath's default is the child axis, so every XPath a caller wrote
would be silently widened — and XPath does not even dodge the multi-word-role problem, its
element names being NCNames too. Datalog was priced and dropped: its value is joins across
relations, there is one relation here, so the syntax would promise far more than the engine
delivers.

No CSS library. `selectors` costs ten associated types and twenty-six `Element` methods to
buy the half already written — matching happens server side against a `Query`, because the
wire carries structure, not a selector string — and it would parse `button#save > .primary`
happily and then match nothing. **The catalogue of things CSS has that this does not, each
refused by name with the equivalent spelled out, is the product.** That is what no library
provides and what the hand-written parser exists for.

Three rules fall out and generalise past selectors:

- **A closed vocabulary makes an unknown name a parse error.** `textbox` is not a selector
  that matches nothing, it is one that *cannot*, and reporting it as `NoMatch` is the
  `NotSettled`/`LoopStalled` collapse again. Suggest the closest real names; edit distance
  alone is not enough, since `pushbutton` is four edits from `button` and exactly right.
- **Generous at the edge, canonical on the wire.** `list-item`, `list_item` and `ListItem`
  all resolve, once, at the point they were written; the resolver then compares literally.
  Both doors into the structure — text and JSON — canonicalise identically, and
  `parse(print(q)) == q` plus "a parsed query is a fixed point of `validate`" are what keep
  them from becoming two dialects.
- **A diagnostic that only says no is half a diagnostic.** Every `ErrorKind` carries a fix,
  checked over every variant by an exhaustive match a new variant cannot skip.

The one collision worth remembering: AT-SPI role names contain spaces and CSS's descendant
combinator *is* a space. Roles hyphenate in a type selector, and `list item` gets its own
error saying so — found by scanning the words around the unknown token for a run that
spells a role. Reported as a plain unknown role it complains about `item` and offers
`icon`, which is true and points away from the answer.

### CLI first, MCP maybe

The CLI is the product. It composes with shell loops, it prints a path that `Read` can
ingest directly, and it needs no server lifecycle to manage while developing. An MCP server
would be another `WaypuppetClient` over the same service — write one if the CLI proves
awkward in practice, not before.

### The CLI hides the sandbox

A caller should never have to know that waypuppet runs in a container, which flags that
container needs, or that their application needs the same one. `waypuppet run -- ./my-app`
is the whole interface. Anything that pushes container mechanics back onto the caller is a
bug in waypuppet, not something for them to configure.

`waypuppet-sandbox` is how: `resolve` turns a `Request` and a set of `Facts` into a `Plan`,
which is either "run here" or "re-execute inside this runtime with these directories
visible". The four mounts are derived from the command rather than asked for — socket
directory, capture directory, working directory, and the two binaries' own directories —
and reduced to the shortest covering set.

Two structural choices hold it together:

- **The decision is pure; only the looking is not.** `resolve` does no I/O. Everything
  about the machine arrives as data from `examine`. That is what makes the interesting
  cases — a host with no flatpak, a container with no render node, a runtime at the wrong
  branch — three lines of test each instead of a machine nobody will arrange.
- **`serve` has no `--sandbox`.** It is the verb that means "here, now", and it is what
  `run` re-executes inside a runtime. Keeping it incapable of entering one makes recursion
  unrepresentable rather than something the detection has to notice — the same move as
  `Watching` splitting `wait` conditions so neither half is handed one it cannot see.

`org.freedesktop.Platform` is preferred over the SDK, which is the opposite of what the
Justfile does and deliberately so: the SDK's extra is a compiler toolchain, which building
waypuppet needs and running an application does not, and the Platform is already installed
on any machine with one graphical flatpak on it.

### A path is the thing most likely to mean two things

Every real bug in the sandbox work was a path that named one directory outside and a
different one inside, and **none of them had a symptom**. That makes them the shape of
failure this project cares most about, so the defences are structural rather than
documented:

- **Resolve symlinks before deriving anything.** On an image-based system `/home` is a
  symlink to `/var/home`, and inside a flatpak sandbox `/home` does not exist at all. A
  mount derived from the unresolved spelling is a mount of nothing, surfacing as an
  application that cannot find its own files. `settle` is applied at the edge, once, so the
  pure core never sees an unresolved path.
- **Name the socket directory outright.** `XDG_RUNTIME_DIR` keeps its *value* inside a
  sandbox and changes its *referent*. Two processes computing a socket path from it agree
  on the string and disagree on the file, so the session starts and nothing ever finds it.
  `WAYPUPPET_SOCKET_DIR` states it instead, and `session::socket_path` prefers it.
- **Captures live with the session, not in `/tmp`.** Every container has its own `/tmp`, so
  a capture written there vanishes and the reported path names nothing; podman will not
  even relabel `/tmp` for SELinux, and mounting it unrelabelled yields a container that
  cannot read it. The default is now `<socket>.captures`, which is already mounted and is
  per-session — which also fixed two sessions overwriting each other's
  `waypuppet-0000.png`.
- **Refuse a capture aimed outside the sandbox.** `Failure::Unshared` rather than a write
  that succeeds into a private filesystem. This is the `NotSettled`/`LoopStalled` rule in
  another costume: the dangerous outcome is not the error, it is the success that reports a
  path nobody can open.

The general lesson, worth more than any of the fixes: **when a value crosses a boundary,
state it rather than letting both sides derive it.** Agreement by coincidence is agreement
until it is not, and it fails silently when it goes.

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
just ci           # what CI runs, in the image CI runs it in. Needs podman or docker.
just toolkit      # drive a real GTK app under waypuppet, in that image.
```

Every recipe builds inside the Flatpak SDK — see [Toolchain](#toolchain), and note the two
exceptions: `just ci` and `just toolkit` use the *CI image* instead, which is a different
container with the same libraries. For anything ad-hoc, including a single test, use `just
shell` and run cargo in there. Bare `cargo` outside either container is unsupported:
whether it links depends on what a given machine has installed, which is exactly the
variable both images exist to remove.

The Flatpak path is the inner loop; `just ci` is how to find out whether CI will agree
without pushing to find out. They differ deliberately — CI has no render node, and
[`docs/project-ci.md`](docs/project-ci.md) explains what that costs.

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
  tests, **`expect-test`** for fixture-driven example tests. `arbtest` is in use in
  `waypuppet-select`, which is the pattern to copy — generators that produce *valid* input
  from the real vocabulary, so a property cannot pass vacuously. `expect-test` is not wired
  up yet.
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

**Verify a defence by breaking it.** Anything whose failure mode is a wrong answer rather
than a crash — the stall diagnosis is the type case, since a version that always blamed the
application would still fail every call "correctly" — should be checked by mutating the
code and confirming the test fails. One test written for `Heartbeat` passed against a
deliberately inverted `diagnose`; it was deleted rather than kept, because a test that
cannot fail reads as coverage and is worse than none.

## Current state

Working end to end: `waypuppet run` starts a session, hosts a child process against a real
compositor, and `ps`, `shot`, `logs`, `kill` and `sessions` answer over the control socket.
A client that dies on startup is reported with its exit status, how long it lasted, whether
it ever mapped a window, and everything it wrote. Input works too — `key`, `type`, `click`,
`move`, `scroll` and `resize` deliver real events through the seat and return only once the
client has drawn in response.

Clients may draw through `wl_shm` or `zwp_linux_dmabuf_v1`, which is what a Skia-backed
toolkit needs. A real one is hosted end to end: GTK 3's `gtk3-widget-factory` is started,
clicked into and typed at, and `hello` appears in the next capture with no sleep anywhere.
`ci/toolkit-smoke.sh` keeps it that way.

`wait` blocks on facts rather than sleeps: `--windows`, `--title`, `--frames`, `--idle` and
`--exit` are answered from bookkeeping the compositor already keeps, and a condition that is
already true returns immediately. `--windows` and `--title` are the dependable ones;
`--frames` and `--idle` carry stated caveats, because frames arrive whether or not the
caller caused them and anything with a spinner is never idle.

The clipboard works both ways, for both selections: `waypuppet clipboard set` offers text
the application can paste, and `clipboard get` reads what the application owns. Verified
against GTK 3 in both directions.

There is now **no sleep used for synchronisation anywhere in the project**, including
`ci/toolkit-smoke.sh`, which drives a real GTK application. `tests/stall.rs` sleeps because
sleeping is the fault under test, and two socket polls check the condition they wait on
rather than guessing at it; a sleep standing in for a condition is a bug.

CI runs on every push — see [`docs/project-ci.md`](docs/project-ci.md). `just ci` runs the
same thing locally, in the same image, so "will CI agree?" does not need a push to answer.

`waypuppet tree` prints the accessibility tree: waypuppet runs a private D-Bus session and
is the AT-SPI registry on it, so an application registers against waypuppet rather than
against the developer's desktop. 261 nodes out of `gtk3-widget-factory`, with roles, names
and bounds in the coordinates `click --at` takes.

Selectors work end to end. `click --on 'button[name="Save"]'`, `key`/`type --focus-on`,
`--index N`, `select` as the dry run, and `tree --selectors`, which puts the shortest
selector for each widget beside it so a caller copies rather than composes. The grammar is
a CSS subset in `waypuppet-select` — `dialog > button[name*="Save" i]:not([name="Cancel"])`
— resolved on
the control thread before anything crosses the bridge, so it stays one call with one
settle. `ci/toolkit-smoke.sh` drives `gtk3-widget-factory` by selector and checks the
resolved point against the bounds the application itself published.

The sandbox is hidden, which was [plan 08](plans/08-sandbox-autonomy.md). `waypuppet run --
./my-app` works out whether a container is needed, finds a runtime, derives the mounts and
puts waypuppet and the application in it together; `doctor` prints the plan it resolved.
`gtk3-widget-factory` has been hosted inside a podman container and driven from the host —
`wait`, `ps`, `tree`, `click --on` and a capture — with the caller passing nothing but
`--image`.

Not yet: pointing any of this at [example-app](../example-app) rather than at GTK, which is now
unblocked rather than done.

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
