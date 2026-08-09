# Using waypuppet in your CI

How to run your GUI application's tests on a machine with no screen, no desktop session and
— usually — no GPU.

> **Status:** real, apart from `tree`. `doctor`, `run`, `ps`, `shot`, `logs`, `kill`,
> `sessions`, `wait`, `clipboard` and every input verb — `key`, `type`, `click`, `move`,
> `scroll`, `resize` — work today, and are exercised against a real GTK application on
> every push by [`ci/toolkit-smoke.sh`](../ci/toolkit-smoke.sh), which runs the example
> below almost verbatim. The accessibility tree is
> [plan 07](../plans/07-accessibility-tree.md).

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

### Which condition to wait on

| | Reliable? | |
|---|---|---|
| `--windows N` | yes | at least N windows are mapped. What to use after `run --detach`. |
| `--title RE` | yes | a *mapped* window's title matches a regular expression. Anchor it — `'^Files$'` — when the application has more than one window. |
| `--frames N` | with care | the focused window drew N more times. Frames arrive whether or not you caused them, so this means "it drew again", not "your change is on screen". Fails immediately if no window exists yet. |
| `--idle MS` | rarely | nothing has drawn for MS milliseconds. A heuristic, and a trap: anything with a spinner, an animation or a blinking cursor is *never* idle, so this never fires. |
| `--exit` | yes | the spawned client has exited. |

Reach for `--windows` and `--title`. They are facts about what the compositor observed;
the other two are facts about what it has not seen lately, which is a weaker thing.

The condition is evaluated when the request arrives, so `wait` for something that has
already happened returns immediately rather than blocking until it next changes. That is
what makes `run --detach` followed by `wait --windows 1` safe: it does not matter which of
the two wins the race.

### Testing a paste

`waypuppet type` and a paste are different code paths in most applications — a terminal
uses bracketed paste precisely so that pasted text is not treated as keystrokes — so if
your application handles both, test both:

```bash
waypuppet clipboard set 'text to paste'
waypuppet key ctrl+v
waypuppet shot -o artifacts/after-paste.png
```

It reads the other way too, which is how to assert that a copy did what it should:

```bash
waypuppet key ctrl+a
waypuppet key ctrl+c
test "$(waypuppet clipboard get)" = 'what should have been copied'
```

Both take `--primary` for the X11-style selection. `clipboard get` prints nothing and exits
0 when nothing owns the selection — an empty clipboard is not an error — and
`clipboard set --stdin` reads the text from a pipe when it is long or awkward to quote.

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
a GPU will be tight or flaky in CI. Prefer waiting on conditions over raising timeouts —
though note that the mutating verbs already settle on the client's next frame before they
return, so the common case needs no wait at all. `wait --windows` and `wait --title` are
the tools for the rest.

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

  Worth knowing what the failure looks like, because it is silent: every glyph renders as
  an empty box, the layout is otherwise perfect, and nothing appears in any log. It is also
  not always a missing font — the same symptom came from a `PATH` the toolkit inherited
  from the process that launched it, with the identical binary rendering correctly when
  launched from a clean shell. If your captures come back full of boxes, compare the
  environment your application is spawned into, not just the fonts installed.
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
| 3 | `wait` only: the condition did not hold before the timeout |

With one deliberate exception: **`waypuppet run` without `--detach` exits with the *child's*
status**, so a session composes with anything that already checks exit codes. A test script
that runs its application under waypuppet gets the application's answer, not waypuppet's.

3 is not a failure, which is why it is not 1. "The condition did not hold" is an outcome a
script may well be expecting — `waypuppet wait --exit --timeout 100` is a perfectly good way
to ask whether the application is still running — and it has to be distinguishable from
waypuppet being unable to answer at all:

```bash
waypuppet wait --title '^Save changes' --timeout 2000
case $? in
    0) waypuppet key Return ;;       # the dialog appeared
    3) echo "no dialog, carrying on" ;;
    *) exit 1 ;;                     # something is actually wrong
esac
```

A failure that names a stalled compositor loop is a waypuppet bug, not yours — see
[CLAUDE.md](../CLAUDE.md).
