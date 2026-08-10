//! Structure back to text: a [`Query`] out as the selector that would parse to
//! it.
//!
//! This exists for four jobs, and the last two are the ones that justify it:
//!
//! - `--help` and documentation can show a real selector rather than a
//!   hand-written approximation that drifts.
//! - `parse(print(q)) == q` is a property, checked over generated queries in
//!   `tests/properties.rs`. A printer that agrees with the parser everywhere is
//!   evidence the grammar has no corner where the two disagree — which is where
//!   a selector that reads as one thing and means another would live.
//! - [`Failure::Ambiguous`] carries every match with its ancestor chain, and a
//!   caller reading that wants a *selector to paste*, not a struct to translate.
//!   [`narrowed`] turns a match into exactly that.
//! - [`identify`] goes further: given a tree and a node in it, the *shortest*
//!   selector that picks that node and nothing else. It is what
//!   `waypuppet tree --selectors` prints, and it changes the common case for
//!   anything generating selectors from composing one to copying one.
//!
//! [`Failure::Ambiguous`]: waypuppet_proto::Failure::Ambiguous

use waypuppet_proto::{Case, Combinator, Match, MatchOp, Matcher, Node, Predicate, Query, Step};

use crate::resolve::{candidates, identity};

/// The shortest selector that picks one node out of a tree, and the index it
/// needs if no selector can. See [`identify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unique {
    /// The selector.
    pub selector: String,
    /// The `--index N` that has to accompany it, when nothing distinguishes
    /// this node from another the selector also matches.
    ///
    /// `None` is the ordinary case and means the selector stands alone. `Some`
    /// is not a failure: two nodes with the same role, the same name and the
    /// same position are genuinely indistinguishable to a selector, and an
    /// index is the honest answer rather than a longer selector that would
    /// still be wrong.
    pub index: Option<usize>,
}

/// The shortest selector that selects `node` and nothing else in `tree`.
///
/// This is what `waypuppet tree --selectors` prints beside every widget, and it
/// is the most useful thing this crate can offer something that generates
/// selectors: the common case stops being *composition* and becomes *copying*.
/// A caller that has read the tree does not have to work out whether a name is
/// unique, whether an ancestor step is needed, or how a role with a space in it
/// is spelled — the answer is on the line.
///
/// The rungs, shortest first:
///
/// 1. the role alone — `dialog`, where a window has one;
/// 2. role and name — `button[name="Save"]`, the usual answer;
/// 3. ancestors, nearest first, as direct-child steps.
///
/// Direct children rather than descendants at each rung, because a `>` chain
/// narrows strictly and reads no longer — while a descendant step taken from an
/// arbitrary ancestor can be satisfied somewhere unintended, which is the one
/// thing a *suggested* selector must never be.
///
/// **The rung chosen is the shortest one that is as precise as any.** Each rung
/// adds a constraint, so the number of nodes it matches only ever falls; the
/// answer is the first rung to reach the lowest count the ladder gets to. Where
/// that count is one, this is simply "the shortest unique selector". Where it is
/// not — GTK publishes a great many unnamed `filler`s, and two siblings can be
/// identical in role, name and position — the index does the last of the work,
/// and a longer path in front of it would be no more precise while being harder
/// to read. Being *no more precise* is the point: the ladder stops climbing
/// once climbing stops buying anything.
#[must_use]
pub fn identify<'a>(tree: &'a Node, ancestors: &[&'a Node], node: &'a Node) -> Unique {
    // The role on its own is the shortest rung and always exists, so it is held
    // apart from the others: that makes "there is always something to fall back
    // on" a fact about the types rather than an `expect` nobody can check.
    let role_only = Query {
        first: Predicate {
            role: identity(node).role,
            name: None,
            not: Vec::new(),
        },
        rest: Vec::new(),
        index: None,
    };
    let mut ladder = Vec::with_capacity(ancestors.len() + 1);

    if node.name.is_some() {
        ladder.push(Query {
            first: identity(node),
            rest: Vec::new(),
            index: None,
        });
    }
    for depth in 1..=ancestors.len() {
        let chain = &ancestors[ancestors.len() - depth..];
        let rest: Vec<Step> = chain[1..]
            .iter()
            .chain(std::iter::once(&node))
            .map(|step| Step {
                combinator: Combinator::Child,
                predicate: identity(step),
            })
            .collect();
        ladder.push(Query {
            first: identity(chain[0]),
            rest,
            index: None,
        });
    }

    // Strictly fewer, never merely as few: keeping the *first* rung to reach
    // each new low is what makes this the shortest selector of its precision,
    // and it is also why the role rung being held apart matters — there is
    // always something to have chosen.
    let role_count = candidates(tree, &role_only).len();
    let mut best = (role_only, role_count);
    for rung in ladder {
        let count = candidates(tree, &rung).len();
        if count < best.1 {
            best = (rung, count);
        }
    }

    let found = candidates(tree, &best.0);
    Unique {
        selector: print(&best.0),
        // `node` satisfies every rung — each is built from its own identity —
        // so one candidate is necessarily this one, and no index is needed. The
        // `position` states that rather than assuming it.
        index: if found.len() == 1 {
            None
        } else {
            found
                .iter()
                .position(|(candidate, _)| std::ptr::eq(*candidate, node))
        },
    }
}

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
