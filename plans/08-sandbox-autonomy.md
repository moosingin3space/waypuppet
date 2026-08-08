# 08 — Sandbox autonomy

**Status:** not started
**Depends on:** [03](03-control-socket.md). Can land any time after; pull it forward if
using waypuppet on a real project starts costing more explanation than it saves.

## Goal

```
waypuppet run -- ./target/debug/my-app
```

works, and the person who typed it never learns that a container was involved. No
`flatpak run` incantation, no `--filesystem` list, no `--device=dri`, no wondering why the
compositor can see the binary but the binary cannot see its libraries.

## Why this is a real problem

waypuppet needs system libraries and a render node. So, usually, does the application under
test — that is why example-app builds inside the SDK at all. Both of them have to end up
somewhere that has those things, **and in the same place as each other**, because they talk
over a Unix socket and share graphics buffers.

Left to the caller, that is a `flatpak run` line with six flags on it, most of which are
about paths. It is exactly the kind of mechanics that makes a tool not worth reaching for.

## Approach to try first

**One sandbox holding both.** waypuppet detects that it needs a runtime it is not currently
in, re-executes itself inside one, and spawns the child there too. Compositor and client
land in the same namespace and everything downstream is ordinary.

- `--sandbox auto|flatpak|podman|none`, defaulting to `auto`. `none` when the current
  process already has what it needs — detect via `/.flatpak-info` and
  `/run/.containerenv`, and confirm with the same EGL probe `doctor` uses.
- Derive the mounts rather than asking for them: the directory of the command being run,
  the working directory, the capture output directory, and the session socket directory.
  These four cover the ordinary case.
- Pass the environment through, minus the variables waypuppet is authoritative for —
  `WAYLAND_DISPLAY`, `DISPLAY`, `XDG_RUNTIME_DIR`, the D-Bus addresses. Silently inheriting
  a `WAYLAND_DISPLAY` that points at the user's real desktop is the worst available
  failure, because it looks like it worked.
- Optional `waypuppet.toml` in the project, so a repository can state its runtime and extra
  mounts once instead of every caller passing flags. Optional is the point: the defaults
  have to work without it.

## Alternative worth pricing

Ship waypuppet *as* a Flatpak with the SDK as its runtime. That removes re-execution
entirely and makes installation a single `flatpak install`. It costs the ability to run a
locally built waypuppet against a locally built application without packaging either, which
during development is most of the time — so it may be the right answer for distribution and
the wrong one for the inner loop. Both can be true.

## Done when

- On a machine with nothing but flatpak and waypuppet installed,
  `waypuppet run -- ./target/debug/example-app-app` produces a capture.
- `waypuppet doctor` prints the resolved plan: which sandbox it would use, which runtime,
  what it would mount, and which render node it found. Getting a path wrong is the most
  likely failure and the hardest to guess at, so it must be inspectable without a `--debug`
  flag.
- Failures name the missing piece and the exact command that fixes it, rather than
  surfacing whatever the container runtime said.
- Someone who has never used Flatpak can follow the README and get a screenshot.

## Notes

- **Never install a runtime without asking.** A first run that silently pulls a gigabyte is
  a bad surprise. Detect, explain, and offer the command — or a `waypuppet setup` that does
  it deliberately.
- Nested containers are worth an explicit error rather than a confusing one. Running
  waypuppet inside a Podman container that has no render node cannot be fixed by trying
  harder, and should say so.
- Watch for path identity across the boundary: a capture written to `/tmp/x.png` inside the
  sandbox must be reported at the path the *caller* can open. If those differ, translate
  before answering, or refuse to write there.
