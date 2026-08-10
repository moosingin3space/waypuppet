# 07 — Accessibility tree

**Status:** done, selectors included
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

## Selectors: the grammar, and why it is CSS

`waypuppet-select` is a crate of its own: `parse` (text → `Query`), `print` (`Query` →
text), `resolve` (`Query` + tree → point), `validate` (the semantic checks, for structure
that arrived as JSON), and `roles` (the vocabulary). Both ends need it and neither owns it
— the CLI parses what a caller wrote, the control thread resolves it — and it is the part
of waypuppet with the least I/O and the most branches, which is what lets its tests be
property tests.

**The syntax is a subset of CSS.** The decisive argument is not familiarity, though that
matters: it is that **CSS's default combinator is the descendant axis**, which is exactly
what a tree query wants. XPath's default is the child axis, so every XPath a caller wrote
would be silently *widened* — a wrong answer with no symptom, which is the failure this
project spends its effort avoiding. XPath also does not dodge the problem it looks like it
solves: its element names are NCNames, so `list item` needs `*[@role="list item"]` there
just as it needs `list-item` here. And Datalog was considered and dropped for the opposite
reason — its whole value is joins across relations, there is one relation here, so the
syntax would advertise far more than the engine does, and its failure mode is the empty
result.

**No CSS library.** [`selectors`](https://lib.rs/crates/selectors) is the real engine and
it costs 10 associated types plus 26 required `Element` methods, MPL-2.0, and a
Servo-paced release cadence — to buy the half that is already written, since matching
happens server side against a `Query` and the wire carries structure rather than a
selector string. Worse, it fights the design: it would parse
`button#save > .primary:nth-child(2)` happily and then match nothing.
[`simplecss`](https://docs.rs/simplecss) is the light alternative (5 methods, one
dependency) and is CSS 2.1, so `:not()` and the Selectors-4 `i` flag are already outside
it. The parser is ~350 lines by hand, and the thing no library provides — the rejection
catalogue — is where the value is.

**What CSS has and this does not is refused by name.** That catalogue *is* the interface:
`.class`, `#id`, `::before`, `+`, `~`, `,`, `:nth-child()`, `:checked`, `:hover`,
`:contains()`, `~=`, `|=`, `=~`, and XPath itself each get their own diagnostic naming the
equivalent — `:nth-child(2)` says position is `--index N` and that it is zero-based;
`~=` says CSS means word-list containment by it and points at `*=`. Collapsing them into
one syntax error would be true and useless, since each has a different right answer.

**Roles are a closed vocabulary, so an unknown one is a parse error.** `textbox` is not a
selector that matches nothing; it is a selector that *cannot* match anything, and reporting
it as `NoMatch` is the same look-alike collapse as reporting a stalled loop as a timeout.
The 130 names are checked at parse and the closest real ones offered. Two signals, because
edit distance alone was not enough: `pushbutton` is four edits from both `button` and
`push button menu`, which reads as noise and is in fact exactly right, so containment
counts too.

**The collision that shaped everything else has its own diagnostic.** AT-SPI role names
contain spaces and CSS's descendant combinator *is* a space. Roles are therefore hyphenated
in a type selector — and `list item` is refused with "write `list-item`", found by scanning
the words around the unknown token for a run that spells a role, longest first. Without it
the caller is told `item` is not a role and offered `icon`: true, useless, and pointing away
from the answer they are already holding.

**Two doors, one representation.** The text form is one way in; the structure can arrive as
JSON, which is what makes an MCP server thin — the tool is a wrapper over `Query` with no
hand-maintained schema. So `validate` re-runs the semantic checks and *canonicalises*, and
the property `parse(print(q)) == q` plus "a parsed query is a fixed point of `validate`"
are what keep the two from drifting into separate dialects. `deny_unknown_fields` is on the
query types for the same reason: a caller writing `{"text":"Save"}` is told, rather than
having the field dropped and the query quietly widened to every node in the tree.

## What the wiring turned out to be

`click --on`, `key`/`type --focus-on`, `--index`, and `select`. Against
`gtk3-widget-factory`: `button[name="Minimize"]` resolves to `1523,46`, which is exactly the
centre of the bounds the application itself published.

**`select` earns its place by making the smoke test assertable.** `click --on` is one call
on purpose — resolve and click, with no gap for the layout to move — which leaves no way to
see *where* it decided to click, and clicking almost anywhere in a widget factory does
something. So the check is: read a widget's bounds out of `tree`, ask `select` for the same
widget, and assert the point matches the centre. Verified by breaking it — a resolver
returning a third of the width instead of half fails with both numbers printed. A test that
only asserted "the click worked" would have passed.

**The focus click is confirmed, not assumed.** The plan left open whether a left click is
the right lever for moving a toolkit's internal focus. It is, and `tests/input.rs` watches
the fixture receive motion, button and keys in that order — because a click delivered
*after* the keys produces every event a weaker test would look for while focusing the widget
just in time to miss the text. Verified by reordering the two and watching it fail. The
all-or-nothing rule extends to it: chords are parsed before the focus click, so a rejected
chord does not move the application's focus on its way out.

**`Ambiguous` is rendered by the CLI, not by `Display`.** The failure carries every match
with its ancestor chain precisely so a caller can see what matched and pick, and
`Failure`'s own `Display` can only produce a count. So the CLI prints each match as a
selector that would select it alone. Against the real tree that is 21 buttons, each with a
pasteable selector and its bounds — which is the constructive diagnostic finally reaching
somebody.

**`tree --selectors` is the guidance, and it changes what generating a selector *is*.**
Every line carries the shortest selector picking that widget and nothing else, so the
common case stops being composition and becomes copying — no working out whether a name is
unique, whether an ancestor step is needed, or how a role with a space in it is spelled.

The rule that produces it is one sentence: **the shortest rung that is as precise as any.**
The ladder is role, then role and name, then ancestors as `>` steps; each rung only ever
matches fewer nodes, so the answer is the first to reach the lowest count the ladder
reaches. Where that is one, it reads as "the shortest unique selector". Where it is not —
GTK publishes a great many unnamed `filler`s — the `--index` does the last of the work and a
longer path in front of it would be *no more precise* while being harder to read. That
unification came from looking at the real output: the first version always climbed to the
full path before falling back to an index, which turned 156 of 261 lines into 70-character
paths ending in `--index 0`. `application[name="gtk3-widget-factory"] > frame > panel
--index 0` became `frame > panel --index 0`, and nothing was lost.

Two properties defend it, and both were broken to check. That the suggestion *selects the
node it was built from* — printed, re-parsed as text, resolved back — because a suggestion
picking a different widget is worse than none: the caller has no reason to doubt it and the
click lands somewhere plausible. And that it is *as short as it can be*, because correct
alone is satisfied by the fully anchored path, which nobody would read.

`--json --selectors` emits a flat array rather than the nested tree, because that is the
shape the question has once selectors are what is wanted: "what is here, and how do I name
it?" is a list. `depth` keeps the structure for anything that still wants it.

**Deferred, and named in the error rather than silently missing.** `#id` maps to AT-SPI's
accessible id and `:checked`/`:disabled`/`:focus` map almost one-to-one onto AT-SPI states
— both are the *right* CSS spellings and both need `Node` to carry data `a11y` does not
fetch yet. That is the order to add them in: the vocabulary is already agreed, so the work
is in `a11y.rs`, not in the grammar. `+`, `~`, `:nth-child()` and `:has()` are all
implementable against `Node` and none are needed yet.

The shape the investigation settled into, which the implementation follows:

- **Resolution happens on the server, not in the CLI.** A selector travels over the wire
  as a structured query — a new protocol type — and the control thread resolves it
  against the tree it already owns before the click is dispatched. Resolving in the CLI
  (read the tree, find the match, call `click --at` with the centre) would be a
  read-then-act round trip with a window for the layout to move in between, which is the
  race the whole project is built against. Keeping it one verb collapses that round trip
  and lets the settle semantics apply to the resolved click.

- **The CLI parses the grammar; the wire carries the structure.** The text form
  (`button[name="…"]`) is parsed in the CLI into the structured query, so the protocol
  carries a real type rather than a string — consistent with "the protocol is a Rust
  trait" and bincode's inability to round-trip a `Value`. A side effect worth keeping:
  an LLM generating waypuppet calls could emit the structured query directly, never
  spelling the grammar.

- **The grammar is CSS, and settled** — see above. The one substantive thing shaping it
  against the real tree changed the answer: multi-word role names collide with CSS's
  descendant combinator, which is why roles hyphenate. Regex was dropped rather than
  spelled: CSS has no regex operator, `~=` means something else, and `*=`/`^=`/`$=` cover
  what a pattern was wanted for here — the ellipsis in `Save As…` and the mnemonic in
  `_Open`. `Matcher` keeps a shape that can grow one if the JSON door ever needs it.

- **Failure variants are distinct, not one `Selector(String)`.** Following `BadPattern`'s
  precedent, a malformed selector reports once, at parse, rather than being retried at
  match time. A selector that matches nothing, one that matches several when the caller
  wanted one, and one that matches a node with no bounds — a collapsed menu, the
  `i32::MIN` corners `a11y` already turns into `None` — are each their own `Failure`,
  because a caller reacts to each differently. Collapsing them would hide "you wrote a
  selector that points nowhere" behind "no match", which is the kind of look-alike this
  project refuses.

- **The click point is the centre of the matched bounds.** Matches the "pointer always
  has a position, parked at the centre" decision. Bounds are `Option<Rect>`, so a node
  with no bounds is the failure above, not a fallback point — `click --at` must never
  receive a coordinate that goes nowhere.

- **`--on` for `click`, `--focus-on` for `key`/`type`.** `--on` and `--at` are mutually
  exclusive (one names a widget, one a point): `conflicts_with`. `key` and `type` are
  about focus rather than position, so they get `--focus-on`, which resolves the selector
  to a node and focuses it before the events go out — the resolve-before-sending rule
  applied to focus rather than coordinates. `move` and `scroll` are the natural next
  extension; out of the first cut.

- **Multi-window scoping is deferred.** `Waypuppet::tree`'s `target` is ignored
  server-side today because a session hosts one window, and a selector implicitly scopes
  to that one. Whether `--on` wants a `--window` too is a real question once there is more
  than one, and is left for then. The ignored `_target` is a wart worth revisiting at the
  same time.

**Testing.** `tests/tree.rs`'s `register` builds a 3-node app with `button "Reconnect"`
at `(10,20 100x30)`, so `button[name="Reconnect"]` resolving to centre ≈ `(60,35)` is a
one-test addition, and the no-bounds fixture already exists for the matched-nowhere
failure. The defence worth breaking, per the project's rule: move the button's bounds in
the fixture and confirm the selector resolves to the *new* centre. A test that only
checks "matched the Reconnect node" reads as coverage and is worse than none — exactly
the trap the tree test's own module docs warn about.

The other thing left is checking this against `example-app-app` rather than GTK, which
[08](08-sandbox-autonomy.md) gates.

## Protocol additions — the selector mechanism

`Waypuppet::tree` and `proto::Node` exist. Selectors do not: `Target` names a window, and
`click --on` needs something that names an element within one. That something is a
structured query — a new protocol type the CLI parses the grammar into, not a string on
the wire (bincode cannot round-trip a free-form value, and a real type is checked at both
ends). The text form is the CLI's concern; the wire carries the structure, which is what
lets an LLM emit the query directly without spelling the grammar. Shape the grammar
around what the tree actually contains now that it is real — see "Still to do".

### Wire types

Landed in `waypuppet-proto`. `Locator` is the one the verbs take; the rest are what a
locator carries when it is not a point. Sketched below with the reasoning; the shipped
definitions and their full docs are in the crate.

```rust
enum Locator { At(Point), On(Query) }

struct Query { first: Predicate, rest: Vec<Step>, index: Option<usize> }
struct Step { combinator: Combinator, predicate: Predicate }
enum Combinator { Descendant, Child }

struct Predicate { role: Option<Matcher>, name: Option<Matcher>, not: Vec<Predicate> }
enum Matcher { Present, Matches { op: MatchOp, value: String, case: Case } }
enum MatchOp { Exact, Contains, Prefix, Suffix }
enum Case { Sensitive, Insensitive }

struct Match { role: String, name: Option<String>, bounds: Option<Rect>, path: Vec<Predicate> }
```

Three things here are not what the first draft said, and each was bought:

- **`first` is a `Predicate`, not the head of a `Vec`.** An empty path is now
  unrepresentable, so "a selector must select something" is a fact about the type rather
  than a check to remember — and there is nowhere to put a leading combinator, so `> button`
  cannot be represented and ignored. `Failure::BadQuery` survives for the JSON door.
- **A step carries a combinator**, because `>` is core CSS and refusing it would be a worse
  compatibility story than supporting it. It is also nearly free: `Node` has ordered
  children.
- **`role` is a `Matcher`, not a `String`.** CSS's type selector and its attribute selector
  become one mechanism — `button` and `[role="button"]` parse identically — and
  `[role*="button"]`, every kind of button of which AT-SPI has four, falls out. An exact
  role is stored canonically, so the resolver compares against `Node::role` literally.

### Trait changes

`click` takes a `Locator` where it took a point; `key` and `write` gain a `focus` locator
that, when set, focuses the resolved node before the events go out. `None` everywhere
preserves today's behaviour.

```rust
async fn click(locator: Option<Locator>, button: Button, target: Target) -> Result<(), Failure>;
async fn key(chords: Vec<String>, focus: Option<Locator>) -> Result<(), Failure>;
async fn write(text: String, focus: Option<Locator>) -> Result<(), Failure>;
```

### Resolution: control thread, before the bridge

The tree lives on the control thread; the seat lives on the compositor thread. So
resolution happens on the control thread, in the RPC handler, and the bridge command
that crosses over carries an ordinary `Option<Point>` — the compositor never hears of
selectors. This is what keeps `click --on` one verb with one settle rather than a
read-tree-then-`--at` round trip with a window for the layout to move in between.

```rust
impl Accessibility {
    /// Resolve a locator to a point against the published tree.
    async fn locate(&self, locator: &Locator) -> Result<Point, Failure>;
}
```

`At(p)` passes through unchanged. `On(q)` calls `tree()` and then the **pure** resolver in
`waypuppet-select`:

```rust
/// Resolve a query against a fetched tree to the click point.
pub fn resolve(tree: &Node, query: &Query) -> Result<Point, Failure>;
```

The algorithm matches right to left, which is how CSS engines do it and for the same
reason: the rightmost step is the one that selects, and everything left of it only has to
be satisfiable somewhere above.

1. A query cannot be empty — `first` is a `Predicate`, not a `Vec` — so that check is gone.
   `validate` still runs the *semantic* checks server side, because a caller may send the
   structure as JSON and bypass the parser.
2. A node is a candidate when the last step matches it and what remains matches up its
   ancestor chain: the immediate parent for `Child`, any ancestor for `Descendant`. The
   descendant case tries each ancestor and **backtracks**, which becomes necessary once
   `>` exists — `dialog panel > button` cannot be settled by taking the first `panel` that
   fits, because the `>` constrains which one was meant.
3. No candidates → `NoMatch`.
4. `index` is `None` and there is more than one candidate → `Ambiguous`, carrying
   every candidate as a `Match` (role, name, bounds, ancestor path) so the caller can
   see what matched and narrow the query. The resolver already walked to each candidate,
   so the path and identity are free; the count is `matches.len()`.
5. `index` is `Some(i)` and `i` is out of range → `NoMatch`.
6. The selected node's `bounds` is `None` → `MatchedNoBounds` — the collapsed-menu case,
   matched loudly rather than dropped silently. A node with bounds is never excluded from
   the candidate set, so this failure specifically describes "you selected something that
   points nowhere."
7. Otherwise the click point is the centre of the matched bounds, matching the
   "pointer parked at the centre" decision.

`click`'s handler resolves the locator to `Option<Point>` and sends today's
`Command::Click { at, button, target, reply }` unchanged — no new bridge command for the
click path. `--focus-on` is the one that needs new machinery (below).

### `--focus-on`: the focus mechanism

Keyboard focus within a window is the toolkit's internal state, set by input, not by the
seat — the seat owns *which surface* has focus, not which widget within it. The reliable
lever is a click. So `--focus-on` resolves the locator to a point and the compositor does
move → left-click → deliver chords/text as one settled sequence, not as a click that
settles and then a separate `key` call (that would be the round trip selectors exist to
remove).

The bridge commands gain a resolved focus point — `Query` never crosses the bridge:

```rust
Key { chords: Vec<String>, focus: Option<Point>, reply }
Write { text: String, focus: Option<Point>, reply }
```

**Open sub-decision, to confirm at implementation:** that a left click is the right focus
mechanism, against a real toolkit (GTK, then example-app-app). It is the pragmatic choice;
if a toolkit focuses on pointer-enter without a click, a click still works, and a click
that does nothing is harmless. Confirm rather than assume, the way the `wl_shm` fixture's
"good manners" were confirmed to hide bugs.

### New `Failure` variants

Each is a distinct kind a caller reacts to differently, following `BadPattern`'s
"report once, at the point it becomes known" precedent. They are not collapsed into one
`Selector(String)` — telling "you wrote a selector that points nowhere" from "no match"
is the same class of distinction as `NoTree` from `Tree`.

```rust
/// A selector could not be understood. Carries the *rendered* diagnostic —
/// caret, `help:` line and all — because the caller who needs it is the one
/// who wrote the selector, and there is no second chance to ask for detail.
#[error("malformed selector: {0}")]
BadQuery(String),
/// A selector matched no node.
#[error("selector matched no node")]
NoMatch,
/// A selector matched several nodes where one was expected.
///
/// Carries every match, trimmed to its disambiguating facts — role, name,
/// bounds, and the ancestor path that locates it — so a caller or an agent can
/// see what was found and narrow the query. The count is `matches.len()`; no
/// silent cap, because truncating the list would hide matches a caller needs to
/// see, and a broad selector against a real tree can match dozens honestly.
#[error("selector matched {count} nodes; disambiguate with an index")]
Ambiguous {
    /// How many nodes matched. Redundant with `matches.len()` but kept for the
    /// error message and for callers that want the count without the detail.
    count: usize,
    /// Every match, trimmed to what disambiguates it.
    matches: Vec<Match>,
},
/// A selector matched a node with no bounds — it is not laid out, so there is
/// nowhere to click.
#[error("selector matched a node with no bounds — it is not laid out")]
MatchedNoBounds,
```

`NoTree` and `Tree` already exist and cover "nothing registered" and "the walk broke" —
`On(_)` against an unregistered session is `NoTree`, reusing the existing path.

### The testability seam

Nothing in `waypuppet-select` needs a compositor: a tree is a value and a selector is a
string. That is what made the properties properties, and `arbtest`'s first use in this
project. What is defended:

- `parse(print(q)) == q` over generated queries, with **hostile names** — quotes,
  backslashes, newlines, arbitrary unicode — because that is where a quoting bug lives.
  It matters beyond tidiness: every selector a diagnostic hands back is *printed*, so a
  printer the parser reads differently would turn advice into a wrong answer.
- A parsed query is a fixed point of `validate`, so the text door and the JSON door cannot
  drift apart.
- **A one-character typo in a role still names the role it meant.** Generated from the
  vocabulary, one random edit applied: either it still resolves — spaces and hyphens are
  levelled, so several edits are no-ops — or the rejection names the original. This is the
  property the closed-vocabulary argument rests on, and it is what "an agent can correct
  itself in one round trip" actually means.
- Parsing is total over arbitrary strings, every span lies inside the input and on a
  character boundary, and every `ErrorKind` has a non-empty `help` distinct from its
  message. Coverage of *every* variant is enforced by an exhaustive match in the test:
  a new variant does not compile until it has a sample.
- Resolution is total, and **a node is always selected by its own position** — the ancestor
  chain of a match, turned into a query, finds that match again. `NoMatch` is unreachable
  there by construction, which is what makes `Ambiguous`'s "narrow it with one of these"
  advice that always leads somewhere. Checked end to end: the selectors an ambiguous result
  hands back are parsed and resolved.

Five defences were broken to confirm they fail: dropping the printer's case flag (two
properties), treating `Child` as `Descendant` (the example test — the properties do not
catch a *widening*, which is what examples are for), silencing suggestions, and deleting a
role from the vocabulary, which the compositor's cross-check against `atspi` catches by
name and number.

That cross-check is worth its own note. The role table is a *copy*, because the CLI must
refuse an unknown role without linking a D-Bus stack, and a copy drifts silently: a role
added upstream simply becomes unselectable, reported as "not an AT-SPI role" to somebody
looking straight at it in `waypuppet tree`. So `a11y`'s tests, in the one crate that has
both, walk every `atspi::Role` and assert it round-trips.

### Deferred, and the warts that come with it

- **`#id` and states.** The CSS spellings are already agreed and already refused by name;
  what is missing is the data. `a11y` does not read AT-SPI's accessible id or state set, so
  `#save` and `:checked` cannot be honoured — and an accessible id would be the most robust
  selector available, stable across layout *and* locale, unlike a name.
- **Multi-window.** `Waypuppet::tree`'s `target` is ignored server-side because a session
  hosts one window, so `On(_)` resolves against the focused window's tree and `click`'s
  `target` is effectively `Focused`. A `--window` alongside `--on` is the real question
  once there is more than one; the ignored `_target` on `tree` is the wart to revisit then.

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
