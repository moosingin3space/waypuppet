# waypuppet

waypuppet runs a Wayland application without a desktop. It provides a command-line
interface to capture images, inspect the accessibility tree, and send input. Use it to
inspect an application or automate GUI tests.

## Install

Download the x86-64 Linux archive from the [releases page]. Extract it, install the
binary, and check the available runtime:

```sh
tar xzf waypuppet-*-x86_64-linux.tar.gz
install -m755 waypuppet ~/.local/bin/
waypuppet doctor
```

The release binary needs `libwayland-server` and
`libxkbcommon`. If `doctor` reports that a runtime is missing, run `waypuppet setup`.

## Start a session

```sh
waypuppet run --detach -- ./my-app
waypuppet wait --windows 1
waypuppet shot
waypuppet kill
```

`shot` writes a PNG file and prints its path. A detached session continues until you stop
it. For a step-by-step example, see the [tutorial](docs/tutorial.md).

## Command-line API

Run `waypuppet --help` for all commands and `waypuppet COMMAND --help` for their options.
Commands use the `default` session unless you set `--session NAME` or the
`WAYPUPPET_SESSION` environment variable. Use `--json` for machine-readable responses
where supported.

| Command | Purpose |
| --- | --- |
| `doctor`, `setup` | Check the runtime or install one. |
| `run`, `sessions`, `ps`, `kill` | Start, inspect, and stop sessions. |
| `shot`, `logs` | Capture a PNG or read the application's output. |
| `tree`, `select` | Inspect the accessibility tree and find a widget. |
| `click`, `move`, `scroll` | Send pointer input. |
| `key`, `type` | Send key chords or type text. |
| `resize` | Change a window's size. |
| `clipboard get`, `clipboard set` | Read or offer clipboard text. |
| `wait` | Wait for windows, a title, frames, idle time, or process exit. |
| `fingerprint` | Record or check the rendering environment for image tests. |

For example, `waypuppet tree --selectors` shows selectors for accessible widgets.
Use `waypuppet select 'button[name="Save"]'` to check a selector, then use
`waypuppet click --on 'button[name="Save"]'` to click it. Selector syntax is a subset
of CSS; run `waypuppet select --help` for the supported forms.

Input commands wait for a client frame after they send input. If the application does not
draw a frame before the settle timeout, the command reports an error. `wait` exits with
status 0 when its condition is met, 3 when it times out, and 1 if it cannot answer.

waypuppet selects a runtime when it needs one. Use `waypuppet doctor` to inspect that
choice. See the [CI guide](docs/ci.md) for software rendering and image comparisons.
The CLI communicates with a session over a Unix socket. The Rust service types are in
[`waypuppet-proto`](crates/waypuppet-proto).

## More documentation

- [Tutorial](docs/tutorial.md): start and inspect an application.
- [CI guide](docs/ci.md): run tests and compare captures in CI.
- [Documentation index](docs/README.md): project and development documentation.
- [Agent skill](skills/waypuppet/SKILL.md): instructions for coding agents.

[releases page]: https://github.com/moosingin3space/waypuppet/releases
