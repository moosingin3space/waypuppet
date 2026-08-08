# 03 — Control socket

**Status:** mostly done — `wait` and `--follow` cancellation remain
**Depends on:** [02](02-rendering-and-capture.md)

## Goal

`waypuppet run`, `ps`, `shot`, `logs` and `kill` work end to end. After this phase the tool
is usable, if not yet trustworthy — that is [04](04-input.md) and [05](05-wait.md).

## Scope

- Serve the `Waypuppet` service from `waypuppet-proto` over a Unix socket at
  `$XDG_RUNTIME_DIR/waypuppet/<session>.sock`, using tarpc's `serde_transport` with bincode
  framing, on a tokio runtime running in its own thread.
- **The bridge.** The compositor thread owns all Wayland state and must never block on a
  request. RPC handlers send a command over an `mpsc` channel carrying a `oneshot` sender,
  and await the reply; the compositor wakes on a calloop channel source, does the work, and
  answers. Build this properly here — phases 04 and 05 both depend on it, and it is the one
  piece that is painful to retrofit.
- Stale socket handling: a socket file whose listener is gone should be replaced, not
  treated as a live session. Connect to it first and reclaim it on `ECONNREFUSED`.
- Child process spawning, with `WAYLAND_DISPLAY` set to the socket phase 01 created and
  `XDG_RUNTIME_DIR` pointing somewhere the child may write.
- stdout and stderr captured into the ring the `logs` call reads from — never inherited.
  Losing the client's last words is precisely the failure this project exists to fix.
- `logs` as a long poll: hold the call open until there is output past the caller's cursor,
  the client exits, or the deadline passes. `--follow` is then a loop resuming from
  `LogChunk::cursor`, with no polling interval to tune.
- Child exit as an event — a pidfd, or tokio's `Child::wait`. Every parked call must be
  answered when the client dies, or callers hang until their own deadlines.
- `run --detach` forks the session into the background and returns once the socket accepts.
  Without `--detach` it stays in the foreground and exits with the child's status.
- `sessions` enumerates live sockets in the session directory.

## Deadlock defences

Phase 02 produced a hang in the *test* harness — a client blocking on a compositor nobody
was ticking. The harness now makes that shape unrepresentable, but the bridge below is the
same hazard at full scale, with a real client and a real RPC server instead of two threads
in a test. Build the defences with the bridge, not after it; all three are cheap now and
awkward to retrofit.

### 1. No unbounded wait, anywhere

Every `recv`, every `.await` on a `oneshot`, every reply the CLI waits for carries a
deadline. tarpc gives this on the RPC leg; the bridge needs it on the compositor leg too.

This prevents nothing. It converts a hang into a diagnosable failure, which is the whole
point — a wedged `shot` that fails in five seconds saying why beats one that runs until CI
gives up. Prefer a helper that takes the deadline over a bare `await`, so the bounded form
is the easy one to write.

### 2. Let `Drop` do the cancelling

A dropped `oneshot::Sender` wakes its receiver with an error immediately, with no timeout
and no detection logic. Hold parked requests so that the **only** way to keep waiting is
for something to keep the sender alive, and tie that lifetime to the condition still being
possible:

- compositor thread panics → every parked sender drops → every outstanding call fails at
  once, with a real error rather than a deadline.
- the client exits → drain the parked-wait registry for conditions that can no longer hold
  ([`Condition::Frames`], settle-on-frame) → those calls answer immediately.

Get this right and "waiting for something that will never happen" mostly stops being a
thing that can happen, rather than a thing that eventually times out.

### 3. A heartbeat on the compositor loop

The loop bumps an `AtomicU64` and a timestamp every tick — one relaxed store, no
measurable cost. Consult it before choosing a timeout's error message:

- the counter advanced → the loop is fine, the client is not drawing → `NotSettled`, which
  is a **normal** outcome describing the application under test.
- the counter did not advance → the loop is stuck → a bug in waypuppet, and the error
  should say so and for how long.

This is a product feature, not test scaffolding. Without it those two cases are the same
"timed out after 5s" and a user cannot tell whether to debug their application or file a
waypuppet bug. With it, waypuppet can say *"the compositor loop has not ticked for 3.2s
while handling `shot`"*, which points straight at the culprit.

### Not worth doing

Lock-order deadlock detectors, `parking_lot`'s included. There is no lock cycle here: the
wait-for edge is "waiting on an event loop to run", which they cannot represent. The tick
counter above is the cheap thing that actually observes it.

## Done when

- `waypuppet run --detach -- ./shm-fixture` followed by `ps`, `shot` and `kill` behaves,
  with no sleeps anywhere in the test.
- A client that panics on startup produces the diagnostic from the README: the exit status,
  how long it lasted, whether it ever mapped a window, and its captured stderr.
- Two sessions with different `--session` names run at once without interfering.
- Killing the CLI mid-call leaves no parked work behind on the server.
- Killing the *compositor* thread mid-call fails every outstanding call promptly, by
  dropped sender rather than by deadline.
- A deliberately stalled loop produces an error naming the stall, not a generic timeout.

## Notes

- Deadlines are the client's to set. Give every CLI verb a sensible default and a flag to
  override, because a default tuned on hardware rendering will be wrong under llvmpipe.
- The CLI is a short-lived process making one call. A current-thread tokio runtime is
  plenty; do not reach for the multi-threaded scheduler out of habit.
- Resist putting Wayland state behind a mutex so handlers can touch it directly. The
  channel is not ceremony — it is what keeps "who may touch compositor state" answerable.

## What landed

`bridge.rs` — `Command`, `Commands`, `Heartbeat`. `Commands::ask` is the only way to reach
the compositor and it always takes a deadline, so defence 1 is structural rather than a
habit. `session.rs` — the compositor thread, the child process with captured output, the
tarpc server, stale-socket reclaim. The CLI gained `serve`, `run` (with `--detach`), `ps`,
`shot`, `logs`, `kill` and `sessions`.

Verified by hand, end to end:

```
$ waypuppet run --size 400x300 -- ./fail.sh
session: client exited 3 after 2ms without mapping a window
--- stdout ---
out-line
--- stderr ---
err-line
$ echo $?
3
```

That is the diagnostic from the README, and the failure this whole project was started
over. The detached path works too: `run --detach`, `sessions`, `ps`, `shot` (a real PNG at
the reported path), `kill`, and the socket is gone afterwards.

`tests/control.rs` covers the same ground over the wire, which is the half that can
deadlock.

## Still to do

- **`wait`** — [plan 05](05-wait.md), which is where parked calls actually appear. The
  bridge is built for them but nothing parks yet.
- **`logs --follow` cancellation.** A follower that is killed leaves its long poll to
  expire rather than being cancelled by the dropped connection.
- Input verbs return `Failure::Unimplemented`, pointing at `plans/`.

## Defence 3 is tested now

`Command::Stall` is fault injection: it blocks the compositor thread on purpose, because
the only way to know whether a stall detector detects stalls is to cause one. Nothing
reachable from the CLI or the control protocol sends it.

`tests/stall.rs` wedges a real compositor and asserts a call made during the wedge comes
back as `LoopStalled`, then that the session recovers once the stall clears. Four unit
tests in `bridge.rs` cover the branches: stalled, healthy-but-timed-out, dropped reply, and
dead channel.

**Verified by mutation, not by passing.** Inverting the comparison in `diagnose` makes
three of them fail with the messages they were written to produce. That check is worth
repeating on anything in this family — the whole failure mode here is silent, because a
`diagnose` that always answered `TimedOut` would still fail every call correctly and only
name the wrong culprit.

## What we learned

- **`Option<Option<i32>>` is a smell clippy catches, and it was right.** "No child" and
  "child exited by signal" are different states, and squashing them into nested options
  made `client_state` hard to read. A three-variant `Life` enum says what it means.
- **`logs` needed "no more output is coming", not "the child exited".** Treating only the
  latter as done made a childless session block for the whole five-second poll window
  before answering emptily — the control test took 5.07s. It now takes 0.07s. A long poll's
  terminating condition is easy to get subtly wrong in the direction of "works, but slowly",
  which no assertion catches.
- **A test that cannot fail is worse than no test.** The first integration test for the
  healthy-loop branch passed against a deliberately broken `diagnose`: every command a real
  compositor can answer, it answers in microseconds, so there is no way to make one time out
  without also blocking the loop — which is the other case. It was deleted rather than kept
  as decoration, and the branch is covered in `bridge.rs` where the heartbeat can be beaten
  with no compositor attached.
- **`deny(warnings)` blocks the laziest mutations.** Stubbing the condition to `if false`
  would not compile, because the parameter goes unused. Inverting the comparison is both
  compilable and a more realistic bug, which made it the better probe anyway.
- **The compositor thread ending is the cancellation.** Falling out of `run()` drops every
  `oneshot::Sender` it held, so outstanding calls fail immediately rather than on their
  deadlines. Defence 2 needed no code at all beyond structuring it that way.
