# 07 — Accessibility tree

**Status:** done, except selectors — see "Still to do"
**Depends on:** [06](06-clipboard-and-resize.md), which has landed

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

## What the spike found

Done, against a real AccessKit application: a headless `accesskit_unix::Adapter` built for
the purpose, driven under `dbus-run-session` in the CI image. **The approach works.** A
throwaway registry of about ninety lines got an AccessKit application to activate and then
read its whole tree back:

```
registry: EMBED from :1.3 at /org/a11y/atspi/accessible/root
app: ACTIVATED — accesskit asked for the initial tree
registry: --- tree from :1.3 ---
role=75 "app"
  role=23 "spike window"
    role=43 "Reconnect"
```

That is `proto::Node` in all but name.

**Answers to the three questions:**

1. **Yes.** An AccessKit application comes up against a bus carrying nothing but the names
   waypuppet would own. Nothing else has to be on it.

2. **It looks up three things, in this order**, and the middle one is skippable:

   - `org.a11y.Status.IsEnabled`, read as a property from `org.a11y.Bus` at `/org/a11y/bus`.
     **This is the gate**: the adapter stays inactive until it reads true, and nothing else
     is attempted.
   - the a11y bus address — from `AT_SPI_BUS_ADDRESS` when that is set and non-empty, and
     only otherwise from `org.a11y.Bus.GetAddress`. Setting the environment variable lets
     one bus be both, which removes a whole layer.
   - `org.a11y.atspi.Socket.Embed` on `org.a11y.atspi.Registry` at
     `/org/a11y/atspi/accessible/root`, passing the application's unique bus name and root
     path. That argument is the prize: the entire tree is reachable from it.

   **With no session bus at all it degrades silently** — `accesskit_unix` wraps the whole
   thing in `if let Ok(session_bus) = Builder::session()` and gives up quietly. No panic,
   no block, no log. That is why an AccessKit application runs happily under waypuppet
   today and simply publishes nothing, and it means adding this cannot break what works.

3. **Yes, easily.** One property and two methods. The spike implemented all of them.

**One landmine, and it is the kind this project cares about.** The `Embed` reply must carry
a **unique** bus name (`:1.3`), not a well-known one: `atspi` deserialises that field with
`UniqueName::try_from`, and a well-known name fails to decode. The failure is then
swallowed as "recoverable", so the application embeds successfully, publishes nothing, and
logs nothing. A wrong answer with no symptom — the first version of the spike hit it, and
the only way to tell it from "AccessKit does not work this way" was to read the source.
Whatever gets built here needs a test that fails when the tree comes back empty, not one
that only checks the handshake happened.

**A correction to "The problem" above.** That section is right about the Flatpak SDK and
Platform, and it is *wrong about CI*: the CI image has both `dbus-daemon` and
`dbus-run-session`, though it has no `at-spi-bus-launcher` and no registry. Verified in
both images. So the missing piece is only ever the registry — which waypuppet becomes — and
a bus, which is missing only in the Flatpak sandbox.

## The decision this leaves

Where the bus comes from, and it is worth deciding deliberately rather than by whichever is
easiest to reach for:

- **[`busd`](https://lib.rs/crates/busd) in-process.** What this plan proposed. Confirmed
  to exist as `bin+lib`, version 0.5.0. **Its own README says "Alpha. It's not ready for
  production use yet."** Taking it means waypuppet's accessibility support inherits that,
  and it is a dependency on a bus implementation rather than on a protocol.
- **Spawn a `dbus-daemon` when there is one.** Present in the CI image and in most
  distribution images, absent from the Flatpak SDK. Cheap where it works, and a fallback
  is still needed for where it does not.
- **Speak D-Bus without a broker.** The far end of the same reasoning as "waypuppet is
  itself a compositor": there is exactly one client, so the bus is a point-to-point
  connection with name registration bolted on. That is a real protocol implementation, but
  a small one against a single known peer, and it is the only option with no runtime
  dependency at all.

The spike does not settle this, and should not — it settles that the *registry* half works,
which was the uncertain part. All three options sit behind the same registry.

## What it turned out to be

`crates/waypuppet-compositor/src/a11y.rs`, and `waypuppet tree`. Against
`gtk3-widget-factory` it reads 261 nodes with roles, names and bounds:

```
application "gtk3-widget-factory"
  frame  [0,0 1408x784]
    panel  [26,23 1356x47]
      button "Minimize"  [1262,31 34x30]
```

**The bus is a daemon where there is one and `busd` where there is not**, which is the
decision the spike left open, taken deliberately. `dbus-daemon` is the reference
implementation and is present in most images that can run a GUI application, so using it
means waypuppet is not also on the hook for being a correct message bus; `busd` describes
itself as alpha, so it is the fallback rather than the default — and it is what makes this
work inside the Flatpak SDK, which ships no bus of any kind. Both paths are exercised: CI
takes the daemon, `just test` in the SDK takes the embedded one.

`dbus-daemon` is started with a configuration file of waypuppet's own rather than
`--session`. The stock session configuration is a separate package from the binary and is
missing from plenty of images that have one; writing three lines is cheaper than depending
on somebody else's file, and it lets the policy say what is actually true here — one
client, everything allowed. `--print-address` doubles as the readiness signal, so there is
no polling and no sleep.

**The landmine the spike found is now a test that fails on it.** `tests/tree.rs`
deserialises the `Embed` reply as `atspi::ObjectRefOwned` — the same type a real client
uses — because that is the only thing that catches a well-known name coming back. This was
verified by reintroducing the bug: with a looser `(String, OwnedObjectPath)` the test
passed against it, which is precisely the trap, and with the faithful type it fails. Worth
noting that the first version of the test had exactly the bug its own module docs claimed
to guard against.

**`tree` never touches the compositor.** It is answered entirely on the control thread, the
same split as `wait --exit`: the tree is the application describing itself over a bus this
side owns, and the compositor has never heard of D-Bus. No bridge command was needed.

**Nodes that are nowhere report nowhere.** GTK answers `i32::MIN` corners with a nominal
size for anything not laid out — every collapsed menu item, of which there are many.
Passing that through as a rectangle would be worse than passing nothing, because
`click --at` would accept it and it points nowhere.

**Both new failure variants are about telling apart two things that look alike.**
`NoTree` means nothing registered, which is the ordinary answer for a toolkit that
publishes no tree; `Tree` means there was one and reading it went wrong. A session whose
bus could not start serves the whole protocol and says so at startup, which is the bargain
the renderer already makes.

## Still to do

**Selectors.** `click --on 'button[name="Reconnect"]'` does not exist. This plan said to
add them "only once the tree is real, and shape the selector syntax around what the tree
actually contains" — that condition is now met, and the tree contains role, name and
bounds, so a selector over those three is the obvious shape. Deliberately not guessed at
here.

The other thing left is checking this against `example-app-app` rather than GTK, which
[08](08-sandbox-autonomy.md) gates.

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
