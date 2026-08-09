# waypuppet's own CI

What CI has to check for this repository, and the one decision that shapes all of it.

> **Status:** built. [`.github/workflows/check.yml`](../.github/workflows/check.yml) runs
> it, [`ci/Containerfile`](../ci/Containerfile) is the image, and `just ci` runs the same
> thing locally so "will CI agree?" does not require pushing to find out.

## What has to pass

The same thing `just check` runs locally: `cargo clippy --workspace --all-targets`,
`cargo fmt --all --check`, and `cargo test --workspace`. The workspace denies warnings,
`missing_docs` and `unsafe_code`, so clippy failing is a build failing.

## The container question

Locally, every recipe runs inside `org.freedesktop.Sdk//25.08` via `flatpak run`. That
exists to remove a variable: contributors are on different distributions and should not
each have to work out which `-devel` packages produce a linkable `libwayland-server.so`.

CI does not have that problem. A CI runner is already a fresh container and can install
whatever it likes. Reproducing the Flatpak layer there buys nothing and costs a lot:
installing the SDK is over a gigabyte per run, flatpak inside a container needs
privileges it should not have, and the nested-sandbox mounting in the Justfile is fragile
in an environment that is already sandboxed.

**So: build a CI image from the same libraries, and run cargo in it directly.** The
Flatpak path stays the local developer story; the image is the CI story. What must not
happen is the two drifting — if CI installs a different libwayland than the SDK ships, CI
stops predicting anything.

[`ci/Containerfile`](../ci/Containerfile) is that image. It installs exactly the libraries
CLAUDE.md's Toolchain section names, and gets its compiler from the same `mise.toml` the
Justfile uses, so the toolchain has one source and cannot drift at all. The libraries can
still drift — Debian's libwayland is not the SDK's — which is why the list is in one file
with a comment saying what it is mirroring.

It is **built in the job rather than pushed to a registry**, which the earlier sketch had
the other way round. A pushed image is faster, but it cannot be written by a pull request
from a fork, and a fresh checkout's first run would have nothing to pull. The buildx layer
cache recovers most of the speed and neither problem exists. Revisit if image build time
ever dominates.

That cache only pays off if the run step looks at the same image the build step produced,
and it nearly did not. buildx loads into **docker**, while `ci/run.sh` prefers **podman** —
right on a contributor's machine, wrong here, because podman cannot see a docker image and
so rebuilt all sixteen layers from scratch before every job. The workflow sets
`WAYPUPPET_CONTAINER_ENGINE: docker` to name the engine rather than let the two steps
disagree. One consequence worth knowing: docker's daemon is root, so the container is root
too, which is why the dmabuf step can open a device node that a rootless engine could not.

## No render node

GitHub-hosted runners have no `/dev/dri`. waypuppet has to work anyway, which is why
[`probe`](../crates/waypuppet-compositor/src/lib.rs) reports software devices alongside
hardware ones and the compositor is expected to fall back.

**This is a feature to test, not a limitation to work around.** CI is the environment where
the software path gets exercised on every commit, and it is the environment most of
waypuppet's users will run in. If the software path breaks, CI should be the thing that
notices.

This has a sharper edge than "the software path gets exercised", and it is worth being
precise about which half needs what.

The **compositor** needs only EGL, and llvmpipe supplies that anywhere — it advertises
dmabuf formats and imports them perfectly well with no GPU. The **client** side of
[`tests/dmabuf.rs`](../crates/waypuppet-compositor/tests/dmabuf.rs) has to *produce* a
dmabuf, and that is the part a runner cannot obviously do.

It can, though, via [`udmabuf(7)`](https://docs.kernel.org/driver-api/dma-buf.html): a
sealed `memfd` becomes a dma-buf with no GPU involved. The fixture tries a render node
first, so a development machine exercises the path a real toolkit takes, and falls back to
udmabuf otherwise. **Verified with no `/dev/dri` present at all** — llvmpipe imports a
udmabuf-backed buffer and composites it correctly, and the dmabuf-versus-shm comparison
passes too.

So the tests stay `#[ignore]` — a machine can still have neither source — and the
`dmabuf import` step tries `modprobe udmabuf` and runs them when it succeeds. **Whether a
hosted runner has the module is the one thing here not verified locally.** When it does
not, the step emits a warning annotation rather than a green tick, because "we did not test
this" and "this passed" must not look the same.

A GPU job on a self-hosted runner is still worth having: udmabuf covers the import, but
only for a linear buffer that a real driver never produced. Tiled modifiers and format
negotiation against an actual GPU are what it would add. Treat it as a second job that is
allowed to be slower and rarer, never as the one that gates merges.

## What runs

Two jobs, both on `ubuntu-latest`, both in the image.

**`check`** is `just check` plus a probe: `cargo fmt --all --check`, `cargo clippy
--workspace --all-targets`, `cargo test --workspace`, then `waypuppet doctor` — which must
report a software device and must *not* report a hardware one. That last assertion is not
decoration. Everything else on this page assumes a hosted runner has no render node, and if
that ever changes it should be noticed on purpose rather than by a test behaving oddly.

**`toolkit`** hosts a real GTK 3 application and drives it — see below. It uploads its
captures as artifacts whatever the outcome, because a screenshot of the moment it went
wrong diagnoses this class of failure on sight.

Both run through [`ci/run.sh`](../ci/run.sh), the same script `just ci` uses. CI has no
private incantation; if it fails and your machine does not, the difference is the image,
not the commands.

## The fixture is not a toolkit

The compositor's own tests drive it with a `wayland-client` fixture — the compositor on the
test's thread, the client on another — which covers the protocol but not real toolkits. This
is not a hypothetical gap. Two bugs got past the entire unit suite and were found only by
running GTK: a missing `wl_data_device_manager`, without which GTK creates no seat and
every keystroke goes nowhere, and frame callbacks that were sent only from `capture`, so a
client that paces itself drew one frame and then looked hung.

Both would have been caught by [`ci/toolkit-smoke.sh`](../ci/toolkit-smoke.sh), which is
why it is a gating job rather than a nicety. It hosts `gtk3-widget-factory`, waits for a
window, clicks, types, and requires the capture to *change*. Without a seat or without
frame callbacks, that fails.

Two known weaknesses, both deliberate:

- **It asserts change, not content.** There is no OCR, so it cannot check that `hello`
  specifically appeared — only that driving the application did something. The captures are
  uploaded so a human can look.
- **It polls `ps --json` for a mapped window** rather than blocking on a condition, because
  `waypuppet wait` is [plan 05](../plans/05-wait.md). It becomes `wait --windows 1` when
  that lands. Polling a protocol fact is still better than a fixed sleep.

Running [example-app](../../example-app) itself remains the test that would most directly justify
the project, and needs example-app built in the image. GTK is the cheap stand-in that catches
the same class of failure today.

## Releasing

Not urgent, but the shape:

- The CLI as a static-ish binary per platform, since the point is to drop it into somebody
  else's container.
- Optionally a Flatpak bundle, which is the packaging alternative discussed in
  [plan 08](../plans/08-sandbox-autonomy.md) — attractive for distribution, wrong for the
  inner loop.
- `Cargo.lock` is committed. Keep it that way: this is a binary, and a reproducible build
  is worth more than automatic dependency currency.
