//! Structure back to text: a [`Query`] out as the selector that would parse to
//! it.
//!
//! This exists for three jobs, and the third is the one that justifies it:
//!
//! - `--help` and documentation can show a real selector rather than a
//!   hand-written approximation that drifts.
//! - [`Failure::Ambiguous`] carries every match with its ancestor chain, and a
//!   caller reading that wants a *selector to paste*, not a struct to translate.
//!   [`narrowed`] turns a match into exactly that.
//! - `parse(print(q)) == q` is a property, checked over generated queries in
//!   `tests/roundtrip.rs`. A printer that agrees with the parser everywhere is
//!   evidence the grammar has no corner where the two disagree — which is where
//!   a selector that reads as one thing and means another would live.
//!
//! [`Failure::Ambiguous`]: waypuppet_proto::Failure::Ambiguous

use waypuppet_proto::{Case, Combinator, Match, MatchOp, Matcher, Predicate, Query};

/// Render a query as the selector that parses to it.
#[must_use]
pub fn print(query: &Query) -> String {
    let mut out = predicate(&query.first);
    for step in &query.rest {
        match step.combinator {
            Combinator::Descendant => out.push(' '),
            Combinator::Child => out.push_str(" > "),
        }
        out.push_str(&predicate(&step.predicate));
    }
    out
}

/// The selector that would select one [`Match`] and nothing else: its full
/// ancestor chain as direct-child steps, then the node itself.
///
/// This is what makes [`Failure::Ambiguous`] constructive rather than a count.
/// A caller — or a model — that gets several matches back does not have to work
/// out how to narrow the query; it picks one of these.
///
/// [`Failure::Ambiguous`]: waypuppet_proto::Failure::Ambiguous
#[must_use]
pub fn narrowed(matched: &Match) -> String {
    let mut steps: Vec<String> = matched.path.iter().map(predicate).collect();
    steps.push(predicate(&Predicate {
        role: Some(Matcher::Matches {
            op: MatchOp::Exact,
            value: matched.role.clone(),
            case: Case::Sensitive,
        }),
        name: matched.name.as_ref().map(|name| Matcher::Matches {
            op: MatchOp::Exact,
            value: name.clone(),
            case: Case::Sensitive,
        }),
        not: Vec::new(),
    }));
    steps.join(" > ")
}

/// One step.
fn predicate(predicate: &Predicate) -> String {
    let mut out = String::new();

    match &predicate.role {
        // The common case, and the only one that can be written as CSS's type
        // selector: an exact, case-sensitive role. Spaces become hyphens
        // because a space is the descendant combinator.
        Some(Matcher::Matches {
            op: MatchOp::Exact,
            value,
            case: Case::Sensitive,
        }) => out.push_str(&value.replace(' ', "-")),
        Some(matcher) => out.push_str(&attribute("role", matcher)),
        None => {}
    }
    if let Some(matcher) = &predicate.name {
        out.push_str(&attribute("name", matcher));
    }
    for excluded in &predicate.not {
        out.push_str(":not(");
        out.push_str(&self::predicate(excluded));
        out.push(')');
    }

    // A predicate that constrains nothing is CSS's universal selector. It has
    // to be written: an empty string is not a step.
    if out.is_empty() {
        out.push('*');
    }
    out
}

/// One `[…]` qualifier.
fn attribute(name: &str, matcher: &Matcher) -> String {
    match matcher {
        Matcher::Present => format!("[{name}]"),
        Matcher::Matches { op, value, case } => {
            let op = match op {
                MatchOp::Exact => "=",
                MatchOp::Contains => "*=",
                MatchOp::Prefix => "^=",
                MatchOp::Suffix => "$=",
            };
            let flag = match case {
                Case::Sensitive => "",
                Case::Insensitive => " i",
            };
            format!("[{name}{op}{}{flag}]", quoted(value))
        }
    }
}

/// A value in double quotes, with the two characters that would end it early
/// escaped. Everything else — including newlines — is passed through, because
/// the parser reads it back the same way and an accessible name is whatever the
/// application says it is.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}
