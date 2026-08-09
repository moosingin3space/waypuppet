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
| 02 | [Rendering and capture](02-rendering-and-capture.md) — a frame becomes a PNG | mostly done |
| 03 | [Control socket](03-control-socket.md) — `run`, `ps`, `shot`, `logs`, `kill` | mostly done |
| 04 | [Input](04-input.md) — `key`, `type`, `click`, `move`, `scroll` | **done** |
| 05 | [Wait](05-wait.md) — block on protocol facts, not on sleeps | not started |
| 06 | [Clipboard and resize](06-clipboard-and-resize.md) — what a terminal exercises | not started |
| 07 | [Accessibility tree](07-accessibility-tree.md) — semantic selectors | not started |
| 08 | [Sandbox autonomy](08-sandbox-autonomy.md) — the caller never types `flatpak run` | not started |

Phases 01–03 were the spine, and they have landed: a session can be started, hosted,
screenshotted and stopped. What is left of them is listed in each plan's "Still to do".

04 has landed as well: input goes through the seat and every mutating verb settles on the
client's next frame, which is what makes `click` followed by `shot` correct with nothing in
between. 05 is what remains of making waypuppet trustworthy rather than merely possible —
waiting on facts instead of sleeping, reusing the parked-sender machinery 04 built. 06 and
07 are driven by
[example-app](../../example-app) specifically. 08 is numbered last but not ranked last: it only
needs 03, and it is what decides whether waypuppet is pleasant to reach for or a thing you
have to remember how to set up.

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
