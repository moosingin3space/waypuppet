# waypuppet

Run a GUI application inside a headless Wayland compositor, then look at it and drive it
from the command line.

```
$ waypuppet run --detach -- ./target/debug/example-app-app
session default: client connected, window 1 mapped "example-app", 2 frames

$ waypuppet type 'ls -la'
$ waypuppet key Return
$ waypuppet wait --idle 200ms
$ waypuppet shot -o /tmp/after-ls.png
/tmp/after-ls.png  1280x800
```

## The problem

A coding agent can build a GUI application and it can run one, but it cannot see one. It
launches the binary, the window opens on somebody's desk, the process exits, and all that
comes back is `exit 0`. Whether anything rendered, whether the layout is right, whether
the button does what the button should do — none of that survives the trip.

The usual workarounds all leak. Screenshotting the developer's own desktop is invasive and
unrepeatable, and impossible in CI. Synthetic input through `uinput` needs privileges and
races the application. Sleeping and hoping is how GUI test suites become flaky.

waypuppet closes the loop: the application runs in a compositor that exists only to host
it, and that compositor answers questions.

## How it works

waypuppet **is** a Wayland compositor — a small one, built on [Smithay], that renders
offscreen and never touches a screen. It starts your program as its only client, and
exposes a control socket. Screenshots come out as PNG files. Keyboard, pointer and touch
events go in. So does the question "is the window mapped yet?"

Everything follows from owning the compositor rather than scripting somebody else's:

- **Input injection is the ordinary path, not a trick.** A key press is a call into the
  seat, delivered on the same protocol objects a real keyboard would use. There is no
  `uinput` device, no privilege, and no ambiguity about whether the client received it.
- **Captures cannot race repaints.** The compositor knows when a client commits a buffer,
  because the client tells it. A capture is a specific committed frame, not a snapshot of
  whatever happened to be on a screen at the time.
- **Waiting is precise.** "Two windows are mapped", "the title matches `/example-app/`", "two
  more frames have been committed", "nothing has been drawn for 200ms" are facts observed
  on the wire. None of them is a `sleep`.
- **It is isolated and reproducible.** A fixed output size, a fixed scale, one client and
  nothing else on the desktop. The same run produces the same pixels, which is what makes
  screenshots worth comparing against a golden image.
- **Failures explain themselves.** When the client dies on startup, waypuppet reports what
  it saw and hands back the output the process wrote before it went:

  ```
  $ waypuppet run -- ./target/debug/example-app-app
  session default: client exited 0 after 340ms without mapping a window
  --- stderr ---
  thread 'main' panicked at crates/example-app-app/src/main.rs:42
  ```

## Beyond pixels

An image tells an agent what a window looks like, but not what is in it. Toolkits built on
[AccessKit] — [freya] among them — already publish an accessibility tree over AT-SPI
describing every widget's role, name and bounds. waypuppet runs the session's bus, so it
can read that tree and hand it back:

```
$ waypuppet tree
window "example-app"
  ├─ terminal "session 1"  0,0 1280x760
  └─ statusbar             0,760 1280x40
     └─ button "Reconnect" 1180,768 88x24
```

Which turns `click --at 1224,780` — a magic number that breaks the moment the layout
changes — into something that says what it means.

## Where it runs

In a container: Flatpak or Podman, no VM required. The only thing waypuppet needs from the
machine underneath is a DRM render node, and it falls back to software rendering when there
is none.

You are not expected to know any of that. waypuppet works out which runtime it needs, puts
itself and your application in the same one, and figures out the mounts from the command
you gave it. `waypuppet run -- ./target/debug/my-app` is the whole interface; if you ever
find yourself writing `flatpak run` to use waypuppet, that is a bug. `waypuppet doctor`
prints what it resolved, for when you do want to look.

## Interface

A CLI, not a daemon protocol you have to speak. Underneath it is a [tarpc] service over a
Unix socket, so anything else — an MCP server, a test harness, another Rust program — can
drive a session too, but the command line is the product.

| | |
|---|---|
| `waypuppet doctor` | what can we render with |
| `waypuppet run -- CMD` | start a session, spawn a client |
| `waypuppet ps` | list windows: id, title, app id, geometry, frames |
| `waypuppet shot` | capture to PNG, print the path |
| `waypuppet key` / `type` | keyboard input |
| `waypuppet click` / `move` / `scroll` | pointer input |
| `waypuppet resize` | reconfigure a window, as a compositor would |
| `waypuppet wait` | block on a protocol-level condition |
| `waypuppet tree` | the accessibility tree |
| `waypuppet logs` | what the client printed |
| `waypuppet kill` | stop it |

Mutating verbs settle before they return: `click` waits for the frame the click caused, so
`click` followed by `shot` is correct without a sleep between them.

## Status

Early, and honest about which half is which.

**Works today.** `waypuppet run` starts a session and hosts a process against a real
compositor. `ps`, `shot`, `logs`, `kill` and `sessions` answer over the control socket, and
`shot` produces a real PNG of what the client drew. A client that dies during startup is
reported with its exit status, how long it lasted, whether it ever mapped a window, and
everything it wrote — which is the failure this project was started over.

**Not yet.** Input (`key`, `type`, `click`), `wait`, the clipboard, and the accessibility
tree. And `zwp_linux_dmabuf_v1`, which matters more than it sounds: anything Skia-backed
renders through EGL and will not draw without it, so no real toolkit has been hosted yet.
The transcript at the top of this file is the design, not a recording.

[CLAUDE.md] has the architecture, [plans/](plans/) has one file per phase with its status
and what it taught us, and [docs/](docs/) covers using waypuppet in CI.

Built to test [example-app], which is why the next things to work are the ones a terminal
application needs: text input, key repeat, clipboard and resize.

## Name

It puppets a Wayland client. It also holds the strings.

[Smithay]: https://smithay.github.io/
[tarpc]: https://lib.rs/crates/tarpc
[AccessKit]: https://accesskit.dev/
[freya]: https://freyaui.dev/
[example-app]: ../example-app
[CLAUDE.md]: CLAUDE.md
