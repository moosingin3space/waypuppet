---
name: waypuppet
description: Look at and drive a GUI application through waypuppet — screenshots, synthetic input, accessibility tree — without a desktop. Use after changing UI code, when a change needs to be seen rather than reasoned about, when a GUI test fails and the reason is not in the logs, or when a bug report arrives as a screenshot.
when_to_use: Trigger on "does this render", "show me the app", "screenshot the UI", "click the button and see", "why does the window look wrong", "the GUI is blank", or any request to verify a visual change in a desktop application.
allowed-tools: Bash(waypuppet doctor:*) Bash(waypuppet ps:*) Bash(waypuppet shot:*) Bash(waypuppet logs:*) Bash(waypuppet wait:*) Bash(waypuppet tree:*) Bash(waypuppet select:*) Bash(waypuppet sessions:*) Bash(waypuppet fingerprint:*)
---

waypuppet runs a GUI application inside a headless compositor and exposes it as PNGs, an
accessibility tree, and synthetic input. It is a compositor, so what it reports is what the
client actually did — a capture is a committed frame, not a race with a screenshot tool.

`run` and `kill` are deliberately not pre-approved: `run` executes an arbitrary command.

## Check the machine first

    waypuppet doctor

Reports what there is to render with and where a session would run. Do this before
concluding anything is broken. `waypuppet setup` installs a runtime if one is needed.

## The loop

    waypuppet run --detach --size 900x600 -- ./my-app
    waypuppet wait --windows 1
    waypuppet shot -o /tmp/app.png

Then **read the PNG** — that is the whole point. `shot` prints the path it wrote.

Finish with `waypuppet kill`. Sessions outlive the shell.

## Waiting

Wait on facts, never `sleep`.

| | |
|---|---|
| `wait --windows N` | the dependable one; use after `run --detach` |
| `wait --title RE` | also dependable; anchor it (`^Files$`) |
| `wait --idle MS` | nothing drew for MS. Good after input, in an app with no animation |
| `wait --frames N` | "it drew again", not "the effect is visible" |
| `wait --exit` | the child process ended |

Exit 3 means the condition did not hold. That is an outcome, not a failure — distinct from
1, which means waypuppet could not answer.

## Driving it

    waypuppet type 'hello'
    waypuppet key ctrl+c Return F5
    waypuppet click --at 450,300
    waypuppet click --on 'button[name="Save"]'
    waypuppet move 450 300
    waypuppet scroll --dy -5
    waypuppet resize 600x400

These return once the client has drawn in response, so no sleep is needed between them.

**Four things that waste time if you rediscover them:**

- `--at` takes `X,Y` **with a comma**. `--at 450 300` is not an error, it just does not
  click — and the symptom is later keystrokes going nowhere, because the click never gave
  the element focus.
- Send a `move` before the first `scroll`. waypuppet parks the pointer at the window centre
  without telling the client, so a toolkit still believes it is at `0,0` — over the
  titlebar, not the content. Clicks are unaffected; they carry coordinates.
- Do not click and immediately type. Focus is applied on the toolkit's next pass and
  nothing on the wire announces it. Put a `wait --idle 200` between, or have the
  application focus its main widget on open, which is better.
- A verb reporting "the client did not draw within the settle timeout" is often correct and
  not a failure: `move` in an app with no hover state, or a scroll already at the end of
  the history, genuinely change no pixels.

## Finding widgets instead of coordinates

    waypuppet tree                # roles, names, bounds
    waypuppet tree --selectors    # each widget with the shortest selector that picks it
    waypuppet select 'button[name*="Save" i]'   # dry run: where a click would land

Selectors are a CSS subset: `dialog > button`, `[name^="Ch 3"]`, `:not([name="Cancel"])`.
Role names hyphenate — `list-item`, not `list item`, because a space is the descendant
combinator. `--index N` picks among matches.

Prefer `--on` over `--at`: it survives layout changes, and `select` tells you what it
matched before you touch anything.

## When something looks wrong

- **Blank window, no error.** Almost always no font resolved. Check that nothing has put
  another `fontconfig` or `freetype` ahead of the system's — `LD_LIBRARY_PATH` and `PATH`
  both leak into the application, because it is a descendant of whatever launched
  waypuppet. A toolchain is not an environment an application should inherit.
- **The application died on startup.** `waypuppet logs` has its stdout and stderr;
  `waypuppet ps` has its exit status and whether it ever mapped a window.
- **Nothing happens on input.** Check focus. `tree` shows whether the widget exists; a
  click that missed is the usual cause.

## Golden images

Captures are byte-reproducible when the renderer is pinned, so pixel comparison works with
no tolerance — and a tolerance large enough to feel safe hides real regressions.

    waypuppet run --detach --software -- ./my-app     # llvmpipe, not the GPU
    waypuppet fingerprint --record                    # the line to keep beside the images
    waypuppet fingerprint --expect "$(cat render.txt)" # exit 3, naming what moved

Pin `--software`, and give the session no render node — a toolkit takes the GPU whenever it
can see one, and `--software` binds only waypuppet's own compositing. A capture taken with
a card visible is one no other machine reproduces.

Record the canonical line, not the digest: a digest can only say that something changed,
never what. A `wp1u:` prefix means nothing content-addressed is behind the capture, so it
is not portable.

**Never bless a blank capture.** A blank image matches a blank image, so one recorded blank
scene passes forever and the suite reports success while testing nothing. Check that
anything was drawn before recording.
