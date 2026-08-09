# 05 — Wait

**Status:** not started
**Depends on:** [04](04-input.md), which landed the parked-sender pattern this needs in bulk
— see `State::settling`, `settle_on` and `abandon_settled`

## Goal

`waypuppet wait` blocks on facts the compositor observed, so no script driving waypuppet
ever needs a `sleep`.

## Scope

- The conditions already declared in `waypuppet_proto::Condition`: `Windows`,
  `TitleMatches`, `Frames`, `Idle`, `Exit`.
- Each is answered from bookkeeping the earlier phases already keep — the commit counter
  from 01, the title and `app_id` from 01, the child's exit from 03. If a condition needs
  new state, add it at the point of truth rather than polling for it.
- Parked calls, over the bridge phase 03 built. `Commands::ask` is the existing shape: the
  handler sends a `Command` carrying a `oneshot::Sender` and awaits with a deadline. What is
  new is that the compositor cannot answer immediately, so `handle_command` has to keep the
  sender in a registry and answer it when the condition fires. A parked `wait` then costs a
  suspended task rather than a stalled compositor.
- **The registry is where defence 2 is either honoured or lost.** A sender must be released
  the moment its condition becomes impossible — the window closed, the client exited — so
  the call fails at once rather than on its deadline. Plan 04 built exactly this for
  settle-on-frame and learned one refinement: dropping the sender *works*, but the caller
  then sees `ShuttingDown`, which is wrong for a window that merely closed. Answer
  explicitly with `ClientGone` and keep the drop as the backstop for a dead compositor
  thread. `abandon_settled` and the per-tick `prune_settling` sweep are the shape to copy.
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
- No sleep is used for *synchronisation* anywhere in waypuppet's own test suite. Deliberate
  fault injection is a different thing — `tests/stall.rs` sleeps because sleeping is the
  fault under test — but a sleep standing in for a condition is a bug.

## Notes

- `Idle` is the one condition that is a heuristic rather than a fact, and it is a trap for
  clients that repaint on a timer: they are never idle, so `--idle` never fires. Say so in
  the CLI help, and prefer `--frames` where the caller knows what they caused.

  This stopped being hypothetical in plan 04. `gtk3-widget-factory` has spinners and
  repaints continuously, so it is *never* idle — and `--frames` is barely better for it,
  since frames arrive whether or not the caller caused them. Settle-on-frame has the same
  weakness and it is written down there too. For such an application neither condition
  means "the effect is visible", and the honest answer may be that `TitleMatches` and
  `Windows` are the reliable ones while `Frames` and `Idle` come with a stated caveat.
  Worth deciding deliberately rather than discovering in a flaky test.
- `TitleMatches` needs a regex crate; it is the only place one is required, so weigh that
  against matching on substrings.
- Watch for the case where a client dies while a `wait` is parked. Every parked request
  must be answered on client exit, or the caller hangs until its own timeout. The unit test
  `a_dropped_reply_fails_at_once_rather_than_on_the_deadline` in `bridge.rs` is the shape to
  copy: assert it fails *quickly*, against a long deadline, or the test passes for the wrong
  reason.
