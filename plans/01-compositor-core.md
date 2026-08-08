# 01 — Compositor core

**Status:** not started
**Depends on:** nothing

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
