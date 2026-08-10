# Plans

One file per phase of building waypuppet, numbered in the order they should land. Each
plan states a goal, what is in scope, and — the part that matters — what has to be true
before it counts as done.

These are working documents. Edit them as reality intrudes; the point is that a plan is
wrong in a visible place rather than in nobody's head. When a phase lands, set its status
to `done` and leave the file: it is the record of why the code is shaped the way it is.
Durable guidance about the project as a whole belongs in [CLAUDE.md](../CLAUDE.md)
instead.

| | Plan | Status |
|---|---|---|
| 01 | [Compositor core](01-compositor-core.md) — a client can connect and map a window | **done** |
| 02 | [Rendering and capture](02-rendering-and-capture.md) — a frame becomes a PNG | **done** |
| 03 | [Control socket](03-control-socket.md) — `run`, `ps`, `shot`, `logs`, `kill` | mostly done |
| 04 | [Input](04-input.md) — `key`, `type`, `click`, `move`, `scroll` | **done** |
| 05 | [Wait](05-wait.md) — block on protocol facts, not on sleeps | **done** |
| 06 | [Clipboard and resize](06-clipboard-and-resize.md) — what a terminal exercises | **done** |
| 07 | [Accessibility tree](07-accessibility-tree.md) — semantic selectors | **done** |
| 08 | [Sandbox autonomy](08-sandbox-autonomy.md) — the caller never types `flatpak run` | **done** |

Phases 01–03 were the spine, and they have landed: a session can be started, hosted,
screenshotted and stopped. What is left of them is listed in each plan's "Still to do".

04 has landed as well: input goes through the seat and every mutating verb settles on the
client's next frame, which is what makes `click` followed by `shot` correct with nothing in
between. It is verified against GTK 3 rather than only against waypuppet's own fixture,
which is what turned up the two things the fixture was too well-behaved to catch.

05 has landed on top of it, reusing that parked-sender machinery for conditions rather than
frames. Between them they are what makes waypuppet trustworthy rather than merely possible:
there is now no sleep used for synchronisation anywhere in the project, including in the
smoke test that drives a real GTK application.

06 has landed too: both selections move in both directions, over pipes driven by the event
loop rather than read inline, and a window can now be smaller than the output.

07 has landed too, selectors included. waypuppet runs a private D-Bus session and *is* the
accessibility registry on it, so `waypuppet tree` reads 261 nodes out of
`gtk3-widget-factory` with roles, names and bounds; the bus is a `dbus-daemon` where the
image has one and an embedded `busd` where it does not, which covers both CI and the
Flatpak SDK. Selectors were deliberately deferred until that tree was real enough to shape
them around, and shaping them around it changed the answer: AT-SPI role names contain
spaces, which collides with CSS's descendant combinator, so roles hyphenate. The grammar is
a CSS subset in `waypuppet-select`, and the catalogue of things CSS has that it does not —
each refused by name with the equivalent — turned out to be the product rather than the
trimming.

08 has landed, which was the last thing between waypuppet and being pleasant to reach for.
`waypuppet run -- ./my-app` now works out whether a container is needed, finds a runtime,
derives the mounts and puts both halves inside — and `doctor` prints the plan it resolved,
because a wrong path is the likeliest failure and the hardest to guess at.

What it cost was almost entirely spent on one class of bug: a path that means two different
things on the two sides of the boundary, and means them silently. `/home` is a symlink that
does not exist inside a sandbox; `XDG_RUNTIME_DIR` keeps its value and changes its
referent; `/tmp` is private to every container and cannot even be relabelled for podman. So
paths are resolved before they are used, the socket directory is named outright rather than
inferred, captures moved out of `/tmp` into the session's own directory, and a capture
aimed somewhere the caller cannot read is refused rather than written into the void.

That last one is the house rule showing up again: waypuppet would rather fail with a name
than succeed invisibly.

`zwp_linux_dmabuf_v1` has landed, so a client that renders through EGL now has somewhere
to put its buffers and a real toolkit can be tried. What is left in
[02](02-rendering-and-capture.md)'s "Still to do" is a way to force software rendering,
which golden-image testing needs and which also gates proving the dmabuf path on llvmpipe
rather than only on a GPU.

## Not planned

Deliberately out of scope, and worth staying out of scope until something concrete demands
otherwise:

- **XWayland.** The applications under test are Wayland-native. Adding it means a whole
  second protocol surface for no current user.
- **Multiple outputs.** One fixed output is what makes captures comparable.
- **Video recording.** Interesting for debugging, but a sequence of `shot` calls covers
  most of it and costs nothing to maintain.
- **Touch and gestures beyond the basics.** The seat can carry them; there is no consumer
  yet.
