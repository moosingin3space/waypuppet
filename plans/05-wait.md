# 05 — Wait

**Status:** not started
**Depends on:** [04](04-input.md)

## Goal

`waypuppet wait` blocks on facts the compositor observed, so no script driving waypuppet
ever needs a `sleep`.

## Scope

- The conditions already declared in `waypuppet_proto::Condition`: `Windows`,
  `TitleMatches`, `Frames`, `Idle`, `Exit`.
- Each is answered from bookkeeping the earlier phases already keep — the commit counter
  from 01, the title and `app_id` from 01, the child's exit from 03. If a condition needs
  new state, add it at the point of truth rather than polling for it.
- Parked calls, using the bridge from phase 03: the handler registers its `oneshot` against
  the condition and awaits, so a parked `wait` costs a suspended task rather than a stalled
  compositor. A tokio timer covers the timeout; tarpc's deadline is the backstop behind it.
- Conditions are evaluated once when the request arrives, so a `wait` for something already
  true returns immediately rather than waiting for the next change.
- Timeout is a distinguishable outcome, not an error indistinguishable from a broken
  socket. A caller needs to tell "it did not happen" from "waypuppet fell over".

## Done when

- Every variant of `Condition` has a test that passes for the true case and times out
  cleanly for the false one.
- `run --detach` immediately followed by `wait --windows 1` is reliable under repetition —
  run it a few hundred times — including the race where the window maps before the `wait`
  arrives.
- No sleep appears anywhere in waypuppet's own test suite.

## Notes

- `Idle` is the one condition that is a heuristic rather than a fact, and it is a trap for
  clients that repaint on a timer: they are never idle, so `--idle` never fires. Say so in
  the CLI help, and prefer `--frames` where the caller knows what they caused.
- `TitleMatches` needs a regex crate; it is the only place one is required, so weigh that
  against matching on substrings.
- Watch for the case where a client dies while a `wait` is parked. Every parked request
  must be answered on client exit, or the caller hangs until its own timeout.
