# 07 — Accessibility tree

**Status:** not started, and the least certain of these plans
**Depends on:** [06](06-clipboard-and-resize.md)

## Goal

`waypuppet tree` prints the widget tree the application publishes — roles, names, bounds —
and selectors let a caller say `click --on 'button[name="Reconnect"]'` instead of guessing
coordinates that break the moment a layout changes.

## Why it is worth the trouble

A capture says what a window looks like; it does not say what is in it. Coordinates
harvested from a screenshot are the most brittle thing a GUI test can contain. The tree is
the difference between a test that describes intent and one that encodes a layout.

It is also nearly free on the application side: toolkits built on [AccessKit] publish this
without being asked. `example-app-app` already pulls `accesskit_atspi_common`, `atspi` and
`zbus` transitively through freya, so the data exists and nothing in example-app has to change.

## The problem

AT-SPI needs a D-Bus session bus and an accessibility bus, and **neither the Flatpak SDK
nor the Platform runtime ships a bus daemon or `at-spi2-registryd`.** Verified: both
runtimes carry `libdbus-1` and `libatspi` as libraries, but there is no `dbus-daemon`, no
`dbus-broker`, no `at-spi-bus-launcher` and no registry binary.

Using the *host's* session bus is the wrong answer even where it is available. It breaks
isolation, it fails in CI, and it means the application under test announces itself to the
user's real desktop.

## Approach to try first

Provide both halves in-process, in Rust:

- [`busd`](https://lib.rs/crates/busd) is a D-Bus broker as a library, so waypuppet can run
  a private bus with no system daemon and hand its address to the child through
  `DBUS_SESSION_BUS_ADDRESS`.
- **waypuppet acts as the AT-SPI registry itself.** An AccessKit application looks up
  `org.a11y.Bus` and registers against `org.a11y.atspi.Registry`. If waypuppet owns those
  names, it receives the registration directly and holds references to the application's
  accessible objects — which is all `tree` needs. There is nothing else on the bus to
  serve, so the parts of the registry that exist to multiplex between screen readers and
  applications can be skipped.

Then `zbus` walks the tree and converts it into `proto::Node`.

## Spike first

This phase should start with a timeboxed spike, not an implementation, because the approach
above is reasoned from how AccessKit is documented to behave rather than from having
watched it work. Answer these, in order:

1. Does an AccessKit application come up at all when `DBUS_SESSION_BUS_ADDRESS` points at a
   `busd` instance with nothing else on it?
2. What exactly does it look up, and what does it do when the lookup fails? A toolkit that
   degrades quietly is fine; one that blocks or panics changes the design.
3. Is the minimal registry surface small enough to be worth owning?

If the answers are bad, the fallback worth pricing is asking the application to publish its
tree directly — a debug endpoint in `example-app-app` — which is less general but a great deal
less machinery.

## Protocol additions

`Waypuppet::tree` and `proto::Node` exist. Selectors do not: `Target` names a window, and
`click --on` needs something that names an element within one. Add that only once the tree
is real, and shape the selector syntax around what the tree actually contains.

`Node` is a concrete type rather than a free-form value on purpose — bincode is not
self-describing, so a `serde_json::Value` cannot round-trip through the transport. Expect
to extend `Node` as AT-SPI turns out to carry things worth having, and treat that as the
normal case rather than a design failure.

## Done when

- `waypuppet tree` against `example-app-app` prints its widgets with roles, names and bounds.
- A selector resolves to the same coordinates a human would pick off a capture.
- It all still works with no host D-Bus session available at all — which is the situation
  in CI, and the whole point.

[AccessKit]: https://accesskit.dev/
