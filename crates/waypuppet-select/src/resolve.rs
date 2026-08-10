//! Matching a [`Query`] against a fetched tree, and resolving what it matched
//! to the point an input verb should land on.
//!
//! [`resolve`] is pure: a [`Node`] and a [`Query`] in, a [`Point`] or a
//! [`Failure`] out. No D-Bus, no bridge, no tokio — so the branches that matter
//! (no match, ambiguous, no bounds) are tested by hand-building a tree, and
//! `tests/` can check over generated trees that it is total.
//!
//! # How a query matches
//!
//! [`Query::first`] matches any node at or below the root; each [`Step`] then
//! relates to the previous match by its [`Combinator`]. Matching runs
//! right-to-left from a candidate up its ancestor chain, which is how CSS
//! engines do it and for the same reason: the rightmost step is the one that
//! selects, and everything left of it only has to be satisfiable somewhere
//! above.
//!
//! Bounds are how a match is *clicked*, not what is *selected by*: a node with
//! no bounds — a collapsed menu, the `i32::MIN` corners `a11y` turns into
//! `None` — is matched loudly as [`Failure::MatchedNoBounds`] rather than
//! dropped silently, because handing `click --at` a point that goes nowhere is
//! the exact bug that path already guards against.

use waypuppet_proto::{
    Case, Combinator, Failure, Match, MatchOp, Matcher, Node, Point, Predicate, Query, Rect, Step,
};

use crate::roles;

/// Resolve a query against a fetched tree to the point an input verb should
/// land on — the centre of the matched node's bounds.
///
/// # Errors
///
/// - [`Failure::NoMatch`] when nothing matched, or when a [`Query::index`] was
///   out of range.
/// - [`Failure::Ambiguous`] when more than one node matched and no index was
///   given, carrying every match so the caller can narrow the query.
/// - [`Failure::MatchedNoBounds`] when the selected node is not laid out, so
///   there is nowhere to click.
pub fn resolve(tree: &Node, query: &Query) -> Result<Point, Failure> {
    let candidates = candidates(tree, query);

    let selected: &Node = match query.index {
        None => {
            if candidates.is_empty() {
                return Err(Failure::NoMatch);
            }
            if candidates.len() > 1 {
                return Err(Failure::Ambiguous {
                    count: candidates.len(),
                    matches: candidates
                        .iter()
                        .map(|(target, ancestors)| to_match(target, ancestors))
                        .collect(),
                });
            }
            candidates[0].0
        }
        Some(index) => {
            // Out of range is `NoMatch`, the same failure as "nothing matched":
            // from the caller's point of view there is no i-th match, and
            // distinguishing the two would ask them to handle two cases that
            // call for the same fix — narrow the query.
            let Some((target, _)) = candidates.get(index) else {
                return Err(Failure::NoMatch);
            };
            target
        }
    };

    let Some(bounds) = selected.bounds.as_ref() else {
        return Err(Failure::MatchedNoBounds);
    };
    Ok(centre(bounds))
}

/// Every node the query matches, with its ancestor chain, in document order —
/// the order `waypuppet tree` prints, so [`Query::index`] selects the node a
/// human would count.
#[must_use]
pub fn candidates<'a>(tree: &'a Node, query: &Query) -> Vec<(&'a Node, Vec<&'a Node>)> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    walk(tree, &mut path, query, &mut out);
    out
}

/// Depth-first walk collecting matches.
fn walk<'a>(
    node: &'a Node,
    path: &mut Vec<&'a Node>,
    query: &Query,
    out: &mut Vec<(&'a Node, Vec<&'a Node>)>,
) {
    path.push(node);
    if matches_suffix(path, &query.first, &query.rest) {
        let mut ancestors = path.clone();
        ancestors.pop();
        out.push((node, ancestors));
    }
    for child in &node.children {
        walk(child, path, query, out);
    }
    path.pop();
}

/// Whether the node at the end of `path` satisfies `first` followed by `rest`.
///
/// Right to left: the last step must match the node itself, and what remains
/// must match somewhere above it — the immediate parent for
/// [`Combinator::Child`], any ancestor for [`Combinator::Descendant`]. The
/// descendant case tries each ancestor and backtracks, which is necessary once
/// `>` exists: `dialog panel > button` cannot be decided by taking the first
/// `panel` that fits, because the `>` constrains which one was meant.
fn matches_suffix(path: &[&Node], first: &Predicate, rest: &[Step]) -> bool {
    let Some((last, earlier)) = rest.split_last() else {
        // Nothing but the leftmost step, which matches anywhere at or below the
        // root — so it only has to match this node.
        return path.last().is_some_and(|node| matches(node, first));
    };
    let Some((node, ancestors)) = path.split_last() else {
        return false;
    };
    if !matches(node, &last.predicate) {
        return false;
    }
    match last.combinator {
        Combinator::Child => matches_suffix(ancestors, first, earlier),
        Combinator::Descendant => (1..=ancestors.len())
            .rev()
            .any(|depth| matches_suffix(&ancestors[..depth], first, earlier)),
    }
}

/// Whether a node satisfies one predicate. A `None` field does not constrain.
fn matches(node: &Node, predicate: &Predicate) -> bool {
    let role = predicate
        .role
        .as_ref()
        .is_none_or(|matcher| attribute(Some(node.role.as_str()), matcher));
    let name = predicate
        .name
        .as_ref()
        .is_none_or(|matcher| attribute(node.name.as_deref(), matcher));
    role && name
        && predicate
            .not
            .iter()
            .all(|excluded| !matches(node, excluded))
}

/// Whether an attribute — which may be absent — satisfies a matcher.
///
/// [`Matcher::Present`] is the only one an absent attribute can never satisfy
/// and the only one an empty value can: `[name]` asks whether there is a name,
/// not what it says.
fn attribute(value: Option<&str>, matcher: &Matcher) -> bool {
    match matcher {
        Matcher::Present => value.is_some(),
        Matcher::Matches { .. } => value.is_some_and(|value| roles::matches(value, matcher)),
    }
}

/// Build the [`Match`] a caller sees on an ambiguous query: the target's
/// identity and bounds, plus the *full* ancestor chain as predicates — not just
/// the part some step matched — so [`crate::print::narrowed`] can render a
/// selector that picks this position alone.
fn to_match(target: &Node, ancestors: &[&Node]) -> Match {
    Match {
        role: target.role.clone(),
        name: target.name.clone(),
        bounds: target.bounds,
        path: ancestors.iter().map(|node| identity(node)).collect(),
    }
}

/// A node as the predicate that matches exactly it.
pub(crate) fn identity(node: &Node) -> Predicate {
    Predicate {
        role: Some(Matcher::Matches {
            op: MatchOp::Exact,
            value: node.role.clone(),
            case: Case::Sensitive,
        }),
        name: node.name.as_ref().map(|name| Matcher::Matches {
            op: MatchOp::Exact,
            value: name.clone(),
            case: Case::Sensitive,
        }),
        not: Vec::new(),
    }
}

/// The centre of a rectangle, as a click point.
///
/// Floor division, so the centre of a 100×30 rect at (10, 20) is (60, 35). The
/// `a11y` walk only produces bounds with positive size — anything not laid out
/// is `None` — but a hand-built tree in a test could carry any [`Rect`]; the
/// `try_from` keeps this total rather than panicking on a width that overflows
/// `i32`.
fn centre(bounds: &Rect) -> Point {
    Point {
        x: bounds
            .origin
            .x
            .saturating_add(i32::try_from(bounds.size.width / 2).unwrap_or(i32::MAX)),
        y: bounds
            .origin
            .y
            .saturating_add(i32::try_from(bounds.size.height / 2).unwrap_or(i32::MAX)),
    }
}
