# waypuppet

Run a GUI application inside a headless Wayland compositor, then look at it and drive it
from the command line.

```
$ waypuppet run --detach -- ./target/debug/example-app-app
session default started

$ waypuppet wait --windows 1
$ waypuppet type 'ls -la'
$ waypuppet key Return
$ waypuppet shot
/run/user/1000/waypuppet/default.captures/waypuppet-0000.png	1280x800
```

No sleeps, and none needed: `wait` blocks until the compositor has seen the window, and
`type` and `key` do not return until the application has drawn in response.

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
application "gtk3-widget-factory"
  frame  [0,0 1408x784]
    panel  [26,23 1356x47]
      button "Minimize"  [1262,31 34x30]
      button "Maximize"  [1302,31 34x30]
      button "Close"  [1342,31 34x30]
```

Bounds come back in the same coordinates `click --at` takes, so a widget you can name is a
widget you can click. It needs no D-Bus on your machine: waypuppet runs the session's bus
and is the accessibility registry on it, so the application registers with waypuppet rather
than announcing itself to your desktop.

And you do not have to work out how to name a widget either — `--selectors` puts the
shortest selector for each one beside it:

```
$ waypuppet tree --selectors
application "gtk3-widget-factory"  → application
  frame  [0,0 1652x952]  → frame
    panel  [26,23 1600x47]  → frame > panel --index 0
      filler  [1499,23 121x46]  → frame > panel > filler --index 0
        button "Minimize"  [1506,31 34x30]  → button[name="Minimize"]
        button "Close"  [1586,31 34x30]  → button[name="Close"]
      toggle button "Menu"  [1457,23 36x46]  → toggle-button[name="Menu"]
      radio button "Page 1"  [644,23 121x46]  → radio-button[name="Page 1"]
```

Each is the shortest selector that picks that widget and nothing else, so driving the
application is a matter of copying one rather than composing it. Where nothing can tell two
widgets apart — GTK publishes a great many unnamed containers — you get an `--index`
instead of a guess.

A selector names the widget:

```
$ waypuppet click --on 'button[name="Close"]'
$ waypuppet type 'hello' --focus-on 'entry[name*="Search"]'
```

The syntax is CSS, cut down to what an accessibility tree can answer — `>` for a direct
child, a space for any descendant, `*=` for part of a name, `:not(…)` to exclude. Role names
hyphenate, because a space already means "descendant of": `list-item`, not `list item`.

Resolution happens inside the session, so `click --on` is a single call: there is no gap
between finding the widget and clicking it for the layout to move in. `waypuppet select`
is the dry run when you want to see where a selector points before committing to it.

Anything CSS has that waypuppet does not is refused *by name*, with what to write instead —
`#id`, `.class`, `:nth-child(2)`, `+`, `,`, XPath — and a role that is not a real AT-SPI
role is a parse error rather than a query that quietly matches nothing:

```
$ waypuppet click --on 'button.primary'
waypuppet: `.primary` is a CSS class, and an accessibility tree has no classes

  button.primary
        ^^^^^^^^

  help: match the accessible name instead: `[name="…"]`, or `[name*="…"]` for part of it
```

## Where it runs

In a container: Flatpak or Podman, no VM required. The only thing waypuppet needs from the
machine underneath is a DRM render node, and it falls back to software rendering when there
is none.

You are not expected to know any of that. waypuppet works out which runtime it needs, puts
itself and your application in the same one, and figures out the mounts from the command
you gave it. `waypuppet run -- ./target/debug/my-app` is the whole interface; if you ever
find yourself writing `flatpak run` to use waypuppet, that is a bug.

`waypuppet doctor` prints what it resolved, for when you do want to look:

```
$ waypuppet doctor --sandbox flatpak
render      /dev/dri/renderD128	hardware
enclosure   host
sandbox     flatpak org.freedesktop.Platform//25.08
binary      /usr/local/bin/waypuppet
socket      /run/user/1000/waypuppet
render node passed in
mounts      /run/user/1000/waypuppet
            /home/you/project
```

If a runtime is missing it says so and offers `waypuppet setup`, which is the only thing
that will ever download one — a first run that quietly pulls a gigabyte is a bad surprise.
`--sandbox none` opts out entirely, and a `waypuppet.toml` beside your project pins a
runtime or adds a mount once instead of every caller passing flags:

```toml
[sandbox]
runtime = "org.freedesktop.Sdk//25.08"
mounts = ["./assets"]
```

## Interface

A CLI, not a daemon protocol you have to speak. Underneath it is a [tarpc] service over a
Unix socket, so anything else — an MCP server, a test harness, another Rust program — can
drive a session too, but the command line is the product.

| | |
|---|---|
| `waypuppet doctor` | what can we render with, and where a session would run |
| `waypuppet setup` | install the runtime, if one is missing |
| `waypuppet run -- CMD` | start a session, spawn a client |
| `waypuppet ps` | list windows: id, title, app id, geometry, frames |
| `waypuppet shot` | capture to PNG, print the path |
| `waypuppet key` / `type` | keyboard input |
| `waypuppet click` / `move` / `scroll` | pointer input |
| `waypuppet resize` | reconfigure a window, as a compositor would |
| `waypuppet wait` | block on a protocol-level condition |
| `waypuppet clipboard get` / `set` | read or write either selection |
| `waypuppet tree` | the accessibility tree |
| `waypuppet logs` | what the client printed |
| `waypuppet kill` | stop it |

Mutating verbs settle before they return: `click` waits for the frame the click caused, so
`click` followed by `shot` is correct without a sleep between them. `wait` covers the rest —
`--windows`, `--title`, `--frames`, `--idle` and `--exit`, each answered from something the
compositor observed rather than from a timer.

## Status

Early, and honest about which half is which.

**Works today.** `waypuppet run` starts a session and hosts a process against a real
compositor. `ps`, `shot`, `logs`, `kill` and `sessions` answer over the control socket, and
`shot` produces a real PNG of what the client drew. A client that dies during startup is
reported with its exit status, how long it lasted, whether it ever mapped a window, and
everything it wrote — which is the failure this project was started over. Input works:
`key`, `type`, `click`, `move`, `scroll` and `resize` deliver real events through the seat,
and each one returns only once the client has drawn in response.

`wait` blocks on facts rather than sleeps: `--windows`, `--title`, `--frames`, `--idle` and
`--exit`. A condition that is already true returns immediately, so `run --detach` followed
by `wait --windows 1` is safe whichever of the two wins the race. `--windows` and `--title`
are the dependable ones; the other two come with caveats stated in `--help`, because frames
arrive whether or not you caused them and an application with a spinner is never idle.

The clipboard works both ways and covers both selections — the ordinary one and the
X11-style primary selection a terminal user expects. `clipboard set` offers text the
application can paste, which is a genuinely different test from `type`, since a terminal
distinguishes a paste from keystrokes. `clipboard get` reads back what the application
itself copied.

Clients may draw through `wl_shm` or `zwp_linux_dmabuf_v1`, so anything Skia-backed —
which renders through EGL — has somewhere to put its buffers.

A real toolkit works end to end: GTK 3's `gtk3-widget-factory` is hosted, waited for,
clicked into, typed at, and the text appears in the next capture with no sleep anywhere.

Every push is checked in a container with no GPU, which is the environment most users will
run waypuppet in, and one job drives that GTK application end to end.

`waypuppet tree` prints the accessibility tree — roles, names and bounds, in the same
coordinates `click --at` takes. waypuppet runs a private D-Bus session and is the AT-SPI
registry on it, so the application registers with waypuppet rather than announcing itself
to your desktop, and none of it needs a bus on the host. It reads 261 nodes out of
`gtk3-widget-factory`.

Selectors turn that tree into something to aim with: `click --on 'button[name="Save"]'`,
`type --focus-on`, `select` to try one first, and `tree --selectors` to be handed one per
widget. The grammar is a subset of CSS, and what it does not support it refuses by name
with the equivalent spelled out, rather than matching nothing quietly.

The container is hidden. `waypuppet run -- ./my-app` decides whether a runtime is needed,
finds one, derives which directories have to be visible from inside it, and puts waypuppet
and your application in there together — see [plan 08](plans/08-sandbox-autonomy.md).
`gtk3-widget-factory` has been hosted inside a container and driven entirely from outside
it: waited for, inspected, clicked by selector, and captured.

**Not yet.** Pointing all of this at [example-app] rather than at GTK, which is what the
project was started for and is now unblocked rather than done.

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
