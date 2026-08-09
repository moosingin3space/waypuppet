# 06 — Clipboard and resize

**Status:** done
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

## What it turned out to be

`waypuppet clipboard get` and `clipboard set`, each taking `--primary`, over
`wl_data_device` and `zwp_primary_selection_v1`. Verified against GTK 3 in both directions:
text set from the command line pastes into `gtk3-widget-factory` with `ctrl+v` and changes
the capture, and text selected in GTK with `ctrl+a`/`ctrl+c` reads back out — including
into the primary selection, which GTK fills on select-all without being asked.

**The transfers are event sources, which is the whole of the difficulty.** The note above
called the fd-based read "the most likely place to accidentally block", and it was right
in both directions. A read is a pipe the *client* fills whenever it likes; a write is a
pipe the client drains whenever it likes. Neither can be done inline on the compositor
thread, so both are `calloop` sources with the payload carried across wake-ups, and both
fds are made non-blocking first. That is what `rustix` was added for — `std::io::pipe`
exists but offers no way to set `O_NONBLOCK`, and there is no acceptable amount of blocking
here.

`a_selection_larger_than_a_pipe_still_arrives_whole` is the test that matters: 300KiB,
comfortably past the 64KiB a pipe buffers, so the transfer cannot complete in one write.
A version that read once and stopped would return a tidy 64KiB prefix and look like it
worked.

**Reads are tracked, writes are not.** A read has a parked reply, so an abandoned one has
to be reachable to be swept — `prune_parked` closes its pipe and drops its loop
registration alongside the other two registries. A write owns everything it needs and ends
by itself: when the client stops reading, the write fails with `EPIPE` and unregisters. The
asymmetry is deliberate and is written down at the type.

**`focus_changed` is load-bearing**, in the same way `title_changed` was for
[05](05-wait.md). A selection is offered to the focused client and to nobody else, so
without that hook a `clipboard set` issued before the window appeared would be owned by the
seat and visible to no one. smithay re-offers the current selection when focus changes,
which is exactly when a freshly mapped window needs telling.

**Three answers, not two.** `clipboard get` returns `Option<String>`: `None` means nothing
owns the selection, which is an ordinary state of affairs. A selection that exists but is
not text is `Failure::Clipboard` instead, because "there is nothing" and "there is something
you cannot read" call for different reactions. Collapsing them into an empty string would
have been the easy mistake.

**The resize half needed a test, not code.** `resize` landed with
[04](04-input.md) — it is a mutating verb like any other. What was missing was the case in
the scope above: a window *smaller* than the output. `tests/resize.rs` asserts the client's
pixels are inside its own bounds and background is outside, which is the only test in the
suite where the compositing path draws anything of its own. Every other capture is
wall-to-wall client buffer, so a renderer that ignored the clear colour, placed the window
wrongly, or scaled it to fit would produce an identical image; here those three give three
different wrong answers. Fixing the fixture to wait for the *next* configure rather than
returning the cached one was needed to make it mean anything.

**Not done, and deliberately:** the acceptance criteria name `example-app-app`, and this was
verified against GTK 3 instead. GTK exercises the same protocols and is already in the CI
image; building a sibling project inside waypuppet's container to prove waypuppet is the
wrong dependency to take. The `example-app` check is worth doing once
[08](08-sandbox-autonomy.md) makes hosting an arbitrary application routine.
