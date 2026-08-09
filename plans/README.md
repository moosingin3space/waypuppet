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
| 07 | [Accessibility tree](07-accessibility-tree.md) — semantic selectors | spiked |
| 08 | [Sandbox autonomy](08-sandbox-autonomy.md) — the caller never types `flatpak run` | not started |

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

07 is **spiked but not implemented**, which is what its own plan asked for. The uncertain
part is settled — an AccessKit application does come up against a bus carrying only the two
names waypuppet would own, and a ninety-line throwaway registry read its whole tree back.
What is left is a deliberate choice about where the bus comes from, and the plan sets out
the three options. It is a decision, not a discovery.

08 is numbered last but not ranked last: it only needs 03, and it is what decides whether
waypuppet is pleasant to reach for or a thing you have to remember how to set up. It is now
also what gates checking any of this against [example-app](../../example-app) itself, which is the
test that would most directly justify the project.

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
