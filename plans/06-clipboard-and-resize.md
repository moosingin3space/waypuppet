# 06 — Clipboard and resize

**Status:** not started
**Depends on:** [05](05-wait.md)

## Goal

The interactions a terminal multiplexer actually exercises: copy, paste, and a window that
changes size.

## Already done, and not by this phase

`wl_data_device_manager` is **already advertised**, with smithay's default handlers and no
selection support. That was not a head start on this phase — it was forced by
[plan 04](04-input.md): GTK defers creating a seat until the data device manager appears,
so without the global there is no keyboard at all and every keystroke goes nowhere. What is
left here is the part that was actually about the clipboard: reading and writing
selections, and the primary selection protocol.

## Scope

- `wl_data_device` for the clipboard — the global exists, the *selections* do not — and
  `zwp_primary_selection` for the X11-style middle click selection that terminal users
  expect to work.
- Resize by reconfiguring the `xdg_toplevel` — send a configure with the new size and let
  the client redraw, exactly as a real compositor would. Not by scaling the capture, which
  would test nothing.
- The output can stay fixed while the window resizes within it; a window smaller than the
  output also proves the compositing path draws a background.

## Protocol additions

`resize` is not just present on the trait, it is **implemented and settling** — plan 04
delivered it, since it is a mutating verb like any other. What this phase adds to the
window half is a client that resizes to something smaller than the output, which is the
case that proves the compositing path draws a background.

The trait still has **nothing for the clipboard**. This phase
adds methods to read and write both selections, and the naming should be settled against
how it reads at the command line: `waypuppet clipboard set 'text'` and `waypuppet clipboard
get` are the likely verbs.

Adding a method means adding a `Command` variant and a handler in `handle_command` too.
Anything that cannot be answered on the spot — a clipboard read waits on the client to
write down a pipe — must park its sender rather than block, under the rule in
[plan 03](03-control-socket.md).

Setting a selection means waypuppet offers a data source to the client, so it needs a MIME
type list. `text/plain;charset=utf-8` covers the terminal case; do not over-generalise
before something needs it.

## Done when

- Text set with the clipboard verb pastes into `example-app-app` and appears in the next
  capture.
- Text selected in the app can be read back out, for both the clipboard and primary
  selections.
- `resize 800x600` produces a capture whose window content reflows, rather than a scaled
  image of the old one — for a terminal, the row and column count should visibly change.

## Notes

- Clipboard transfers are fd-based and asynchronous: the receiving side gets a pipe and
  reads from it. That read belongs in the event loop like everything else, and it is the
  most likely place to accidentally block.
- Terminals distinguish paste from typing — bracketed paste exists precisely so a paste is
  not treated as keystrokes. Setting the selection and letting the app paste is testing
  something genuinely different from `waypuppet type`, and both are worth having.
