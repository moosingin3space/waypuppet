# 05 — Wait

**Status:** done
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

## What it turned out to be

Landed as `State::waiting`, a second parked-sender registry alongside `settling`, plus
`Commands::until` on the bridge and `waypuppet wait` on the CLI. Everything is in
`tests/wait.rs`; `ci/toolkit-smoke.sh` now uses `wait` instead of polling `ps`, which
removed the last synchronisation sleep in the project.

**The conditions split in two, and the split is a type.** Four of the five are facts the
compositor observes. `Exit` is not: a process can exit without ever having connected, so
there is no Wayland event behind it, and only the control thread — which spawned it — can
see it. That is `bridge::Watching`, an enum with a `Compositor(Watched)` arm and an `Exit`
arm, so neither half is ever handed a condition it cannot answer. The first draft passed
all five to the compositor and needed a "cannot happen" arm in the match; an arm that
cannot happen is an arm that answers wrongly when it does.

**Conditions are resolved on arrival, not stored as asked.** `Frames(2)` becomes an
absolute commit count against a named surface, and a pattern becomes a compiled `Regex`.
Both fall out of the same idea: resolve once, against the state the request arrived in, so
re-evaluating cannot drift and a bad pattern is reported to the caller who wrote it rather
than silently matching nothing until the timeout. That needed `Failure::BadPattern`.

**Two new `Failure` variants, both because reusing one would have lied.** `BadPattern` as
above, and `NoClient` for `wait --exit` against a session that never spawned anything.
`ClientGone` was the tempting reuse and its message is "the client has exited" — telling
somebody whose application never started that it had died is the worst available answer.

**Event-driven, with one deliberate exception.** The registry is woken from `commit`, from
`title_changed`, and from `park_wait` itself — never from a sweep on the loop. Polling
every tick would have been fewer lines and correct to within 16ms, and it was the first
design; it was dropped because it makes the arrival check unfalsifiable. With per-tick
evaluation, deleting that check still passes every test, one tick later. Without it, four
tests fail. `Idle` is the exception and gets `check_idle` on the loop, because going quiet
is the *absence* of commits and there is no event to hang it off.

**`title_changed` is load-bearing and was nearly missed.** `xdg_toplevel.set_title` is not
double buffered — smithay applies it on the request — so a client may rename its window and
then never draw again. A registry woken only by frames leaves that wait parked forever.
`a_renamed_window_is_noticed_without_being_redrawn` is the test, and it needed a `set_title`
command on the fixture client to exist at all.

**Four defences were verified by breaking them**, as the project asks of anything whose
failure mode is a wrong answer: removing the arrival check, the commit hook, or the
`title_changed` hook each fails tests (1, 5 and 1 respectively), and replacing the explicit
`ClientGone` in `abandon_waits` with a dropped sender fails the abandonment test.

**On the notes above:** `regex` earned its place — three crates, all but one already in the
tree via `tracing-subscriber`. Substring matching would have made `^Untitled` look for a
literal caret and quietly never fire, which is the exact class of flake `wait` exists to
remove. And the `Idle` warning was right: it is documented in the CLI help as a heuristic
that never fires for anything with a spinner, with `--windows` and `--title` named as the
dependable ones.

**`--frames` with no window fails rather than parking.** `Failure::NoSuchWindow`,
immediately. Waiting for frames from a window that does not exist is failure mode B wearing
a condition's clothes, and the caller who wrote that wanted `--windows`.

**Acceptance:** all five variants have a true case and a clean-timeout case;
`run --detach` followed by `wait --windows 1` ran 300 times against a real GTK application
with no failures, in 43 seconds; and `waiting_for_a_window_is_reliable_however_the_race_falls`
does 200 rounds alternating the two orderings deterministically, which is a stronger check
than repetition alone — a version missing either wake path passes exactly half the time.

**Still to do:** nothing blocking. `Settled::TimedOut` maps to CLI exit 3, not the obvious 2,
because clap already spends 2 on usage errors and a script cannot otherwise tell "the
condition did not hold" from "you misspelled a flag".
