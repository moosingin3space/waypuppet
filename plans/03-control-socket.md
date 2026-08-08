# 03 — Control socket

**Status:** not started
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

## Done when

- `waypuppet run --detach -- ./shm-fixture` followed by `ps`, `shot` and `kill` behaves,
  with no sleeps anywhere in the test.
- A client that panics on startup produces the diagnostic from the README: the exit status,
  how long it lasted, whether it ever mapped a window, and its captured stderr.
- Two sessions with different `--session` names run at once without interfering.
- Killing the CLI mid-call leaves no parked work behind on the server.

## Notes

- Deadlines are the client's to set. Give every CLI verb a sensible default and a flag to
  override, because a default tuned on hardware rendering will be wrong under llvmpipe.
- The CLI is a short-lived process making one call. A current-thread tokio runtime is
  plenty; do not reach for the multi-threaded scheduler out of habit.
- Resist putting Wayland state behind a mutex so handlers can touch it directly. The
  channel is not ceremony — it is what keeps "who may touch compositor state" answerable.
