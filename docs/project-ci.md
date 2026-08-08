# waypuppet's own CI

What CI has to check for this repository, and the one decision that shapes all of it.

> **Status:** planned. No pipeline exists yet. Written down now because the container
> question below changes what the Justfile should look like, and that is cheaper to settle
> early.

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

A `Containerfile` in this repository, built and pushed on change, is the way to keep that
honest. It should install exactly the libraries listed in CLAUDE.md's Toolchain section and
the mise-pinned Rust toolchain, so the versions have one source.

## No render node

GitHub-hosted runners have no `/dev/dri`. waypuppet has to work anyway, which is why
[`probe`](../crates/waypuppet-compositor/src/lib.rs) reports software devices alongside
hardware ones and the compositor is expected to fall back.

**This is a feature to test, not a limitation to work around.** CI is the environment where
the software path gets exercised on every commit, and it is the environment most of
waypuppet's users will run in. If the software path breaks, CI should be the thing that
notices.

A GPU job is still worth having eventually, on a self-hosted runner with a render node, to
catch dmabuf and format problems that llvmpipe hides. Treat it as a second job that is
allowed to be slower and rarer, never as the one that gates merges.

## Sketch

```yaml
name: check
on: [push, pull_request]

jobs:
  check:
    runs-on: ubuntu-latest
    container:
      image: ghcr.io/moosingin3space/waypuppet-ci:latest
    steps:
      - uses: actions/checkout@v4
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets
      - run: cargo test --workspace
      - run: cargo run -p waypuppet -- doctor   # expect software-only here
```

## What CI cannot check, and what to do about it

The compositor's own tests drive it with a `wayland-client` fixture — the compositor on the
test's thread, the client on another — which covers the protocol but not real toolkits. The
failures that matter most, a toolkit that hangs because it never got a configure or refuses
to start because a global is missing, only show up against a real application.

So the suite should grow a job that runs [example-app](../../example-app) under waypuppet and
asserts it maps a window and draws something. It is the reason waypuppet exists, and it is
the only test that would have caught the problem it was built for.

That job needs example-app built, which means either a second image or building it in the same
one. Worth the cost; without it, waypuppet can pass every test and still not work.

## Releasing

Not urgent, but the shape:

- The CLI as a static-ish binary per platform, since the point is to drop it into somebody
  else's container.
- Optionally a Flatpak bundle, which is the packaging alternative discussed in
  [plan 08](../plans/08-sandbox-autonomy.md) — attractive for distribution, wrong for the
  inner loop.
- `Cargo.lock` is committed. Keep it that way: this is a binary, and a reproducible build
  is worth more than automatic dependency currency.
