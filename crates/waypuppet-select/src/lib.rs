//! Selectors: naming a widget in the accessibility tree instead of guessing a
//! coordinate off a screenshot.
//!
//! ```text
//! button[name="Reconnect"]        a button with that exact name, anywhere
//! dialog > button[name*="Save"]   a direct child of a dialog, part of the name
//! list-item:not([name="Cancel"])  hyphens stand in for the spaces in a role
//! ```
//!
//! # Why this is a crate
//!
//! Both ends need it and neither owns it. The CLI parses the text a caller
//! wrote; the control thread resolves the structure against the tree it fetched,
//! and re-checks anything that arrived as JSON without passing through the
//! parser. Putting all three in `waypuppet-compositor` would make the CLI depend
//! on the compositor to read a string, and putting them in `waypuppet-proto`
//! would give the wire types an opinion about syntax.
//!
//! It is also the part of waypuppet with the least I/O and the most branches, so
//! standing it up alone is what lets the property tests be property tests: a
//! tree is a value, a selector is a string, and nothing here needs a compositor
//! to be running.
//!
//! # The design, in one paragraph
//!
//! The syntax is a subset of CSS, because a selector is read and written far
//! more often by people and models fluent in CSS than in anything waypuppet
//! could invent — and CSS's default combinator is the descendant axis, which is
//! exactly what a tree query wants. What CSS has and this does not — `.class`,
//! `#id`, `+`, `,`, `:nth-child()` — is **refused by name**, with the
//! equivalent spelled out, rather than parsed and silently ignored. That
//! catalogue in [`error`] is the reason the grammar is hand-written instead of
//! delegated to a CSS engine: `selectors` would parse every one of those happily
//! and then match nothing, which is the silent wrong answer this project spends
//! its effort avoiding.
//!
//! Roles are a closed vocabulary of 130 AT-SPI names, so an unknown one is a
//! parse error with the closest real roles named, not a query that quietly
//! matches nothing. See [`roles`].
//!
//! # Layout
//!
//! - [`parse`] — text in, [`Query`] out. The grammar.
//! - [`print`] — [`Query`] in, text out. Also [`narrowed`], which turns a
//!   [`Match`] into the selector that would pick it alone (what makes an
//!   ambiguous result constructive), and [`identify`], which finds the shortest
//!   selector for a node in a tree (what makes `tree --selectors` possible).
//! - [`resolve`] — a [`Query`] and a tree in, a click point or a named failure
//!   out.
//! - [`validate`] — the semantic checks, for structure that arrived as JSON and
//!   never met the parser.
//! - [`roles`] — the vocabulary, and what to suggest when a caller misses it.
//! - [`error`] — the diagnostics, which are the interface as much as the
//!   grammar is.
//!
//! [`Query`]: waypuppet_proto::Query
//! [`Match`]: waypuppet_proto::Match

pub mod error;
pub mod print;
pub mod roles;

mod parse;
mod resolve;
mod validate;

pub use error::{ErrorKind, ParseError};
pub use parse::parse;
pub use print::{Unique, identify, narrowed, print};
pub use resolve::{candidates, resolve};
pub use validate::validate;
