# 04 — Input

**Status:** landed
**Depends on:** [03](03-control-socket.md), which landed the bridge these verbs travel over

## Goal

`key`, `type`, `click`, `move` and `scroll` deliver real events to the client, and return
only once the client has drawn the result.

## Scope

- An xkb keymap on the seat via `XkbConfig`, sent to the client on focus. The layout has to
  be pinned and stated, because chord parsing resolves against it.
- Keyboard input through `KeyboardHandle::input`, with keyboard focus set to the mapped
  window.
- Chord parsing: `ctrl+shift+t`, `Return`, `F5`. Modifier names, then a key name resolved
  to a keysym.
- Keysym to keycode is the fiddly part. The protocol carries *keycodes*, so the keymap has
  to be searched for a code that produces the wanted keysym, along with the modifier state
  needed to get there. Build the reverse index once at startup.
- `type` is the same machinery per character, including working out when shift is required.
  Decide explicitly what happens for characters the layout cannot produce — erroring is
  better than silently typing something else.
- Pointer through `PointerHandle`: `motion`, `button`, `axis`. Each batch ends with
  `frame()`. Serials must come from the seat's counter, not be invented.
- Coordinates resolve against `Target`: window-relative by default, output-relative for
  `Target::Output`.
- **Settle-on-frame.** After delivering events, wait for the client to commit a frame, up
  to a timeout, before replying. This is the property that makes `click` followed by `shot`
  correct with nothing in between, and it belongs here rather than in every caller.

  This is the first command that cannot be answered inline. `handle_command` currently
  answers everything on the spot; a settling verb has to hold its `oneshot::Sender` until
  the commit counter moves, which is the parked-sender pattern [plan 05](05-wait.md) needs
  in bulk. Build it once, here, and let `wait` reuse it.

  Note that "the client did not draw" is `Failure::NotSettled` — a normal outcome about the
  application — while "the loop stopped" is `Failure::LoopStalled`. `Commands::ask` already
  tells them apart via the heartbeat; do not collapse them into one timeout.

## Done when

- A test client that echoes received input confirms the right keysyms, modifiers and
  button codes arrive, in order.
- `type 'hello'` into a text field in `example-app-app` shows `hello` in the next capture, with
  no sleep in the test.
- A settle timeout on a client that draws nothing returns `NotSettled` rather than hanging,
  and is not confused with a stalled loop.
- Every parked sender is dropped when its window goes away, so a client that exits
  mid-settle fails the call at once rather than on its deadline.

## Notes

- Settling has a failure mode worth designing for: a client that repaints continuously
  never goes idle, so "wait for the next frame" must mean the next commit after the event,
  not the loop going quiet.
- Key repeat is the client's business — it gets the repeat rate from the keymap and runs
  its own timer. Do not synthesise repeats.
- Focus follows the single window for now. It becomes a real question only if multiple
  windows ever land.

## What it landed

`crates/waypuppet-compositor/src/input.rs` is the keymap and the chord parser;
`State::deliver_*` sends the events; `State::settle_on` parks the reply;
`Commands::settle` is the control-thread half. The CLI verbs `key`, `type`, `click`,
`move`, `scroll` and `resize` all work over the socket.

## What it taught us

- **Asking the xkb *state*, not the keymap's levels, is what makes the reverse index
  exact.** The tempting build is "level 0 is unshifted, level 1 is shifted", which is true
  of `us` and not a promise. Setting the shift mask on an `xkb::State` and asking what each
  keycode produces is the same computation the client will run against the same keymap, so
  the two cannot disagree. It is also no more code.

- **The pointer needs a starting position, or half the surface silently does nothing.**
  `scroll`, and `click` with no coordinates, go to whatever the pointer is over. With no
  initial position there is no pointer focus, the events go nowhere, and the call fails as
  `NotSettled` — a completely misleading diagnosis. The pointer is now parked at the centre
  of a window when it maps, alongside keyboard focus. Fixed rather than clever, because
  determinism is the constraint.

- **Parse before you look up the window.** `key hyper+z` against a session with no window
  first reported `no such window`, which is true but useless — the caller's actual mistake
  was the chord. Resolving the whole request before touching state costs nothing and makes
  the error name the real problem. The same ordering is what makes `type` all-or-nothing.

- **`Commands::settle` is `ask` with one substitution, and that substitution is the
  interesting part.** A healthy loop that ran out of time means the client did not draw
  (`NotSettled`); a loop that stopped ticking still means waypuppet is broken
  (`LoopStalled`). Writing it as a wrapper rather than a second wait keeps the heartbeat as
  the single place that decides whose fault a timeout is. Both branches are tested, and
  both were checked by mutating the code — laundering `LoopStalled` into `NotSettled` fails
  exactly one test, which is the one written for it.

- **A parked sender wants an explicit answer, not just a drop.** Dropping does wake the
  receiver, but the caller then sees `ShuttingDown`, which is wrong for a window that
  merely closed. `toplevel_destroyed` answers its settlers with `ClientGone`; the drop
  remains the backstop for a compositor thread that dies. Abandoned senders — a caller that
  hit its own timeout — are swept each tick using `Sender::is_closed`.

## Verified against a real toolkit

`gtk3-widget-factory` — GTK 3, from the Flatpak SDK — was hosted, clicked into and typed
at, and the next capture shows `hello` in the text field with the caret after it. No
sleeps: `click` and `type` each returned once GTK had drawn. That closes the last item in
"Done when", with a real toolkit rather than `example-app-app`, which does not exist yet.

Two defects had to be fixed first, and neither was visible from the test suite, because the
`wl_shm` fixture does not behave like a toolkit.

- **No `wl_data_device_manager`, so GTK created no seat at all.** The clipboard global was
  filed under [plan 06](06-clipboard-and-resize.md) as a separate concern. It is not: GTK
  defers seat creation until the data device manager appears, so without it there is no
  `wl_seat` bind, no keyboard, and every keystroke goes nowhere. The only symptom was
  `gdk_seat_get_keyboard: assertion 'GDK_IS_SEAT (seat)' failed` in the client's stderr —
  which waypuppet captured and handed back, which is the one part of this that went right.
  The global is now advertised with the default handlers; reading and writing selections is
  still plan 06.

- **Frame callbacks were only sent from `capture`.** A toolkit requests a
  `wl_surface.frame` callback before committing and does not draw again until it fires, so
  GTK drew exactly one frame and then stopped — every verb after that failed as
  `NotSettled`, blaming an application that was doing precisely what the protocol says. The
  callbacks are now paced from the event loop at the 60Hz the output advertises.

  Worth being clear about what this does *not* change: rendering is still on demand, and
  that is what determinism rests on. Pacing a client and drawing a frame are different
  jobs, and only the second one is a capture.

`tests/frames.rs` covers the second, and the fixture's required-globals check covers the
first. Both were confirmed by breaking them.
