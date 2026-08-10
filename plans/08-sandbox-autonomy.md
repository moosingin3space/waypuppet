# 08 — Sandbox autonomy

**Status:** done
**Depends on:** [03](03-control-socket.md). Landed after 07, though it only ever needed 03.

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

## What landed

`waypuppet-sandbox` decides, and the `waypuppet` binary acts on the decision.

- **`--sandbox auto|flatpak|podman|none`** on `run`, and on `doctor` so the plan can be
  inspected without starting anything. `auto` runs here when this process can already
  render and finds a runtime when it cannot.
- **The four mounts are derived**, not asked for: the socket directory, the capture
  directory, the working directory, and the directories of both binaries. They are reduced
  to the shortest covering set, so a binary under the working directory does not get a
  mount of its own.
- **`org.freedesktop.Platform//25.08` is preferred over the SDK**, the opposite of what
  waypuppet's own Justfile does. The SDK adds a compiler toolchain, which is what building
  waypuppet needs and running an application does not; the Platform is already installed on
  any machine with one graphical flatpak on it.
- **`waypuppet.toml`** states a runtime, an image and extra mounts once per repository.
  Found by walking up from the working directory; relative mounts resolve against the file
  rather than against whoever ran waypuppet.
- **`waypuppet setup`** installs a missing runtime, and nothing installs one without being
  asked.
- **`doctor` prints the resolved plan** — runtime, binary, socket, render node, mount list
  — and its exit code follows the plan rather than the device list, because "nothing to
  render with here, and a runtime that supplies it" is a working machine.

Verified end to end: `gtk3-widget-factory` hosted inside a podman container and driven from
the host — `wait --windows`, `ps`, `tree`, `click --on 'button[name="Minimize"]'` and a
capture of the real widget factory — with the caller passing nothing but `--image`.

## Done when

- ~~On a machine with nothing but flatpak and waypuppet installed,
  `waypuppet run -- ./target/debug/example-app-app` produces a capture.~~ Done, against
  `gtk3-widget-factory` in both runtimes. Pointing it at example-app is the next thing to try.
- ~~`waypuppet doctor` prints the resolved plan.~~ Done.
- ~~Failures name the missing piece and the exact command that fixes it.~~ Done, and
  checked over every `Obstacle` variant by an exhaustive match.
- ~~Someone who has never used Flatpak can follow the README and get a screenshot.~~ Done
  — `waypuppet setup` is the one command, and only when a runtime is actually missing.

## What it taught us

**Every real bug here was a path meaning two different things, and none of them had a
symptom.** That is the theme, and it is why the guard rails are shaped the way they are.

- **`/home` is a symlink on an image-based system, and does not exist inside the sandbox at
  all.** A mount derived from `/home/you/project` is a mount of nothing, and the failure
  surfaces as an application that cannot find its own files. Every path is now resolved
  through `settle` before it is used for anything.
- **`XDG_RUNTIME_DIR` has the same value inside a flatpak sandbox and names a different
  directory.** Two processes computing a socket path from it agree on the string and
  disagree on the file, so the session comes up and nothing ever finds it. `WAYPUPPET_SOCKET_DIR`
  names it outright; the agreement is stated rather than inherited.
- **`/tmp` was the worst possible default for captures**, and it took two failures to see
  it. Every container has its own, so a capture written there vanishes and the path handed
  back names nothing — and podman *refuses to SELinux-relabel `/tmp`* at all, while
  mounting it unrelabelled produces a container that cannot read it. Captures now go to
  the session's own directory beside its socket, which also fixes two sessions overwriting
  each other's `waypuppet-0000.png`.
- **An image's `XDG_RUNTIME_DIR` is typically created by root at mode 700**, which
  `--userns=keep-id` then maps to somebody else. The compositor fails at startup with
  "Could not write to XDG_RUNTIME_DIR", which reads as a waypuppet bug. It is set to the
  session directory, the one place known to be both mounted and ours.
- **A capture aimed outside the sandbox is refused rather than written.** This is the
  `NotSettled`/`LoopStalled` rule again: the write would *succeed*, into a private
  filesystem, and the reported path would name nothing. `Failure::Unshared` names the
  directories that would work and the flag that adds another.

Two structural notes worth carrying forward:

- **`serve` has no `--sandbox`, and that is load-bearing.** It is the verb that means
  "here, now", and it is what `run` re-executes inside a runtime. Keeping it incapable of
  entering one makes recursion unrepresentable rather than relying on the detection to
  notice — the same move as `bridge::Watching` splitting conditions so neither half can be
  handed one it cannot see.
- **Separating the decision from the detection is what made it testable.** `resolve` is
  pure: facts in, plan out. A host with no flatpak, a container with no render node, a
  runtime installed at the wrong branch — each is three lines of test instead of a machine
  nobody will arrange. Twenty-two of them, none needing a container.

## Still to do

- **Point it at [example-app](../../example-app).** That was the reason for the project, and it is
  now unblocked rather than done.
- **Podman refuses to relabel a system directory**, so `--sandbox-mount /tmp` fails with
  podman's own message rather than one of waypuppet's. It no longer happens by default,
  which is why it was left; the fix is to refuse such a mount at plan time with a named
  alternative.
- **The re-executed binary must suit the runtime.** waypuppet re-executes *itself* inside,
  so a host-built binary meets the runtime's glibc. It works in both directions here, and
  the failure is reported with whatever the loader said, but shipping waypuppet as a
  flatpak — the alternative priced below — would remove the question rather than diagnose
  it.

## Alternative worth pricing

Ship waypuppet *as* a Flatpak with the SDK as its runtime. That removes re-execution
entirely and makes installation a single `flatpak install`. It costs the ability to run a
locally built waypuppet against a locally built application without packaging either, which
during development is most of the time — so it may be the right answer for distribution and
the wrong one for the inner loop. Both can be true, and nothing here forecloses it.
