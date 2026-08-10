//! Checking a query that did not come from [`parse`].
//!
//! The CLI compiles text into a [`Query`], and everything the grammar knows —
//! above all that a role has to be a real AT-SPI role — is checked there. But
//! text is not the only door: the structure can arrive as JSON, from a caller
//! that would rather emit the type than spell the syntax, and an MCP server
//! would hand it over the same way. That path bypasses the parser entirely.
//!
//! So the semantic checks are made again on what arrives, and the same
//! generosity is extended: [`validate`] returns a *canonicalised* query rather
//! than a verdict, so `{"role":{"matches":{"value":"List Item",…}}}` is
//! accepted and stored as `list item`, exactly as `list-item` would have been
//! through the grammar. Two doors into one representation, or the wire slowly
//! grows a second dialect.
//!
//! [`parse`]: crate::parse

use waypuppet_proto::{Failure, MatchOp, Matcher, Predicate, Query, Step};

use crate::error::{ErrorKind, unknown_role};
use crate::roles;

/// Check a query and return it with every role spelled canonically.
///
/// # Errors
///
/// [`Failure::BadQuery`] naming the role that is not one, with the closest real
/// roles — the same catalogue the text parser draws its diagnostics from, so a
/// caller gets the same advice whichever door they came through.
pub fn validate(query: &Query) -> Result<Query, Failure> {
    Ok(Query {
        first: predicate(&query.first)?,
        rest: query
            .rest
            .iter()
            .map(|step| {
                Ok(Step {
                    combinator: step.combinator,
                    predicate: predicate(&step.predicate)?,
                })
            })
            .collect::<Result<Vec<_>, Failure>>()?,
        index: query.index,
    })
}

/// One predicate, and everything it excludes.
fn predicate(predicate: &Predicate) -> Result<Predicate, Failure> {
    Ok(Predicate {
        role: predicate.role.as_ref().map(role).transpose()?,
        name: predicate.name.clone(),
        not: predicate
            .not
            .iter()
            .map(self::predicate)
            .collect::<Result<Vec<_>, Failure>>()?,
    })
}

/// A role matcher: canonicalised if exact, checked against the vocabulary if
/// not.
fn role(matcher: &Matcher) -> Result<Matcher, Failure> {
    let Matcher::Matches { op, value, case } = matcher else {
        return Ok(Matcher::Present);
    };

    if *op == MatchOp::Exact {
        let Some(canonical) = roles::canonical(value) else {
            return Err(refuse(&unknown_role(value)));
        };
        return Ok(Matcher::Matches {
            op: *op,
            value: canonical.to_owned(),
            case: *case,
        });
    }

    if roles::satisfiable(matcher) {
        Ok(matcher.clone())
    } else {
        Err(refuse(&ErrorKind::UnsatisfiableRole {
            op: *op,
            value: value.clone(),
        }))
    }
}

/// Render a rejection without a caret: there is no text to point at, but there
/// is still a fix to name, and the fix is the half that matters.
fn refuse(kind: &ErrorKind) -> Failure {
    Failure::BadQuery(format!("{}\n\n  help: {}", kind.message(), kind.help()))
}
