# 01 — Compositor core

**Status:** done
**Depends on:** nothing

> The third acceptance bullet — a real toolkit getting past startup — is **deliberately
> deferred**. A toolkit needs more of the protocol surface than this phase provides
> (dmabuf above all, since anything Skia-backed renders through EGL), so failing against
> one here would say nothing useful. The protocol surface is proven against a
> `wayland-client` test client; proving it against freya waits until there is enough
> compositor to be a fair test.

## Goal

A client can connect to waypuppet and map a window. Nothing is drawn and nothing is
observable from outside the process yet — this phase is about the protocol surface being
complete enough that a real toolkit does not bail out during startup.

## Scope

- A `calloop` event loop owning a `Display<State>`, with the Wayland fd registered as an
  event source. `smithay::wayland::socket::ListeningSocketSource` creates the
  `wayland-N` socket; remember which name it picked, because phase 03 has to put it in the
  child's `WAYLAND_DISPLAY`.
- The globals a toolkit needs before it will start: `wl_compositor`, `wl_subcompositor`,
  `wl_shm`, `wl_seat`, `wl_output`, `xdg_shell`. Wire them with smithay's `delegate_*`
  macros and the matching `*State` types.
- `xdg_output` too. Clients that do scale maths read the logical size from there and some
  will wait for it.
- A `desktop::Space<Window>` as the single source of truth for what exists and where.
  Placement is trivial for now: one window, at the origin, sized to the output.
- Per-surface bookkeeping the later phases need, added now because it is cheap and
  retrofitting it means touching every commit path: a monotonic commit counter, first-map
  time, and the last `xdg_toplevel` title and `app_id`.

## Done when

- An in-process test client — a socketpair-connected `wayland-client` — binds every global
  without error, creates an `xdg_toplevel`, and the test observes it become mapped.
- `WAYPUPPET_LOG=debug` shows the window mapped with its title and `app_id`.
- Running a real toolkit against the socket by hand gets past startup. `example-app-app` is the
  target, but anything Wayland-native proves the point.

## Notes

- Read smithay's `anvil` example before writing new protocol handling; it is the reference
  compositor and most of this phase is a smaller version of its state struct.
- The one non-obvious sequencing rule: a toolkit that gets no `wl_output` geometry, or an
  `xdg_surface` that is never configured, blocks forever without saying so. If a client
  hangs at startup, suspect a missing configure before suspecting anything else.
- Keep `State` a plain struct with everything in it. Smithay's delegation wants one type
  to hang the handler impls off, and splitting it early fights the framework.

## What landed

`waypuppet-compositor` with `State` (globals, seat, output, `Space`, per-window records)
and `Compositor` (event loop, socket, run loop). `waypuppet serve` starts it and prints the
socket name. `tests/handshake.rs` is the acceptance test.

The test asserts from the *client* side — globals advertised, `xdg_surface.configure`
followed by `xdg_toplevel.configure` at the output size, and a buffer commit that draws no
protocol error. Compositor-side assertions on the commit counter need a way to ask the
compositor a question from another thread, which is exactly what plan 03's bridge is for.

## What we learned

- **anvil's display dispatch needs `unsafe`, and it is avoidable.** The reference
  compositor puts `Display` inside a calloop `Generic` and reaches back in with an unsafe
  `get_mut`. Registering a *clone of the poll fd* as a wake-only source, and dispatching in
  the run loop where `display` and `state` borrow separately, is equivalent and entirely
  safe. Worth remembering when copying anvil elsewhere: several of its `unsafe` blocks are
  structural, not essential.
- **`std::env::set_var` is `unsafe` in Rust 2024.** The obvious way to point a test client
  at the compositor is to set `WAYLAND_DISPLAY`; the lint forbids it. Connecting the socket
  path directly with `UnixStream::connect` is better anyway — no process-global mutation,
  so tests can run in parallel.
- **smithay's typed units earn their keep.** `Logical` versus `Physical` sizes caught a
  real mistake at compile time: the output mode is physical, window geometry is logical, and
  conflating them would have produced a window the wrong size at any scale but 1.
- **The initial configure is the whole handshake.** A toolkit that never receives one hangs
  at startup with no error. It has to go out on the client's first buffer-less commit, not
  when the toplevel is created.

