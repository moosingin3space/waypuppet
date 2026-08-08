# 04 — Input

**Status:** not started
**Depends on:** [03](03-control-socket.md)

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

## Done when

- A test client that echoes received input confirms the right keysyms, modifiers and
  button codes arrive, in order.
- `type 'hello'` into a text field in `example-app-app` shows `hello` in the next capture, with
  no sleep in the test.
- A settle timeout on a client that draws nothing returns a clear timeout rather than
  hanging.

## Notes

- Settling has a failure mode worth designing for: a client that repaints continuously
  never goes idle, so "wait for the next frame" must mean the next commit after the event,
  not the loop going quiet.
- Key repeat is the client's business — it gets the repeat rate from the keymap and runs
  its own timer. Do not synthesise repeats.
- Focus follows the single window for now. It becomes a real question only if multiple
  windows ever land.
