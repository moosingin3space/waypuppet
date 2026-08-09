# Using waypuppet in your CI

How to run your GUI application's tests on a machine with no screen, no desktop session and
— usually — no GPU.

> **Status:** partly real. `doctor`, `run`, `ps`, `shot`, `logs`, `kill` and `sessions`
> work today, so a pipeline that starts an application, screenshots it and uploads the
> result is possible now. The input verbs and `wait` are still to come
> ([plans 04–05](../plans/)), so the example below does not run end to end yet. The
> constraints and the advice are real regardless — they come from how the pieces fit
> rather than from the code.

## What CI has to provide

Almost nothing:

- A container. waypuppet brings its own compositor, so there is no Xvfb to start, no
  `dbus-launch`, no window manager, and nothing to clean up afterwards.
- Optionally `/dev/dri`. If it is there, waypuppet renders on the GPU. If it is not — and
  on GitHub-hosted runners it is not — waypuppet falls back to software rendering and
  everything still works.

Check what you got before debugging anything else:

```yaml
- run: waypuppet doctor
```

It prints the devices it found, one per line, and fails with an explanation if there are
none.

## The shape of a test

```bash
waypuppet run --detach --size 1280x800 -- ./target/debug/my-app
waypuppet wait --windows 1 --timeout 30000
waypuppet type 'hello'
waypuppet key Return
waypuppet shot -o artifacts/after-enter.png
waypuppet kill
```

No sleeps. `wait` blocks on facts the compositor observed, and the input verbs do not
return until the application has drawn the result, so `key` followed by `shot` captures the
frame the key press caused.

## GitHub Actions

```yaml
jobs:
  gui:
    runs-on: ubuntu-latest
    container:
      image: ghcr.io/your-org/your-app-ci:latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo build
      - run: waypuppet doctor
      - run: ./scripts/gui-tests.sh
      - uses: actions/upload-artifact@v4
        if: always()
        with:
          name: screenshots
          path: artifacts/
```

`if: always()` matters more than it looks. A GUI test that fails without a screenshot tells
you almost nothing; one that fails *with* the screenshot and the application's stderr is
usually diagnosed on sight.

## Software rendering is the normal case

GitHub-hosted runners have no render node, so CI will use llvmpipe. Two consequences:

**It is slower** — several times slower for a large window. Timeouts tuned on a laptop with
a GPU will be tight or flaky in CI. Prefer waiting on conditions over raising timeouts:
`wait --frames 1` after an action is both faster and more reliable than a longer deadline.

**It produces different pixels.** Antialiasing and filtering differ between llvmpipe and any
GPU driver, so a screenshot taken on your machine will not be byte-identical to one taken in
CI. This is the single biggest trap in image-based GUI testing.

## Golden screenshots

If you compare captures against stored images, the comparison is only meaningful when
everything that affects rasterisation is pinned:

- **Pin the size and scale.** `--size 1280x800 --scale 1` on every run, including when you
  regenerate the goldens.
- **Pin the renderer.** Generate goldens the same way CI will. Since CI is almost certainly
  software, pass `--software` everywhere — on your machine when you regenerate them, and in
  CI when you check them. Goldens made on a GPU and checked under llvmpipe will fail, and
  the diff will look like noise.

  ```
  waypuppet run --software --size 1280x800 --scale 1 -- ./my-app
  ```

  `--software` is a filter, not a preference: a machine with no llvmpipe fails to start
  rather than quietly falling back to the GPU, because a silent fallback is exactly the
  thing that would make every golden wrong without anyone noticing. It applies to `run` and
  `serve`, and `run --detach` forwards it to the session it starts.
- **Pin your fonts.** waypuppet does not draw your text; your toolkit does, with whatever
  fonts the container has. Install them explicitly rather than inheriting them.
- **Compare with a tolerance.** Exact equality is achievable within one renderer but brittle
  across driver updates. A small per-pixel tolerance with a failure threshold on the
  proportion of differing pixels is the usual compromise.

Where the accessibility tree covers what you are checking, prefer it. `waypuppet tree`
returns roles, names and bounds, and asserting that a button named "Save" exists at a
sensible position is both more robust and more legible than diffing pixels.

## Getting waypuppet into your image

Add the binary to whatever container your application already builds in. It needs the same
graphics libraries your application does — EGL, GBM, libdrm, xkbcommon — which an image that
can build a GUI application already has.

Do not run `waypuppet` and your application in *different* containers. They share a Unix
socket and graphics buffers, so they have to be in the same namespace.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | the operation failed — read stderr |
| 2 | usage error |

With one deliberate exception: **`waypuppet run` without `--detach` exits with the *child's*
status**, so a session composes with anything that already checks exit codes. A test script
that runs its application under waypuppet gets the application's answer, not waypuppet's.

`wait` timing out is a failure, not a crash, and says which condition did not hold. A
failure that names a stalled compositor loop is a waypuppet bug, not yours — see
[CLAUDE.md](../CLAUDE.md).
