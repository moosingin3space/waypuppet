//! Selecting a node from the accessibility tree by query, and resolving it to
//! the point an input verb should land on.
//!
//! [`resolve`] is pure: it takes a fetched [`Node`] and a [`Query`] and returns
//! either the centre of the matched node's bounds or a [`Failure`] naming what
//! went wrong. No D-Bus, no bridge, no tokio — so the resolution logic is tested
//! by hand-building a tree and asserting on the outcome, and the failure
//! branches (no match, ambiguous, no bounds) are checked directly rather than
//! through a session.
//!
//! # How a query matches
//!
//! A [`Query`] is a descendant-axis path: each [`Predicate`] matches some node,
//! each later one a descendant of the previous match, and the last one's match
//! is the target. A single-element path is a flat match anywhere in the tree —
//! the common case — and more elements narrow by structure without insisting on
//! a direct-child chain. Candidates are collected in document order (the order
//! `waypuppet tree` prints), so a [`Query::index`] selects the same node a
//! human would count.
//!
//! Bounds are how a match is *clicked*, not what is *selected by*: a node with
//! no bounds — a collapsed menu, the `i32::MIN` corners `a11y` turns into
//! `None` — is matched loudly as [`Failure::MatchedNoBounds`] rather than
//! dropped silently, because handing `click --at` a point that goes nowhere is
//! the exact bug that path already guards against.
//!
//! See `plans/07-accessibility-tree.md` for the design.

use waypuppet_proto::{Failure, Match, Node, Point, Predicate, Query, Rect};

/// Resolve a query against a fetched tree to the point an input verb should
/// land on — the centre of the matched node's bounds.
///
/// # Errors
///
/// - [`Failure::BadQuery`] when the path is empty.
/// - [`Failure::NoMatch`] when nothing matched, or when a [`Query::index`] was
///   out of range.
/// - [`Failure::Ambiguous`] when more than one node matched and no index was
///   given, carrying every match so the caller can narrow the query.
/// - [`Failure::MatchedNoBounds`] when the selected node is not laid out, so
///   there is nowhere to click.
pub fn resolve(tree: &Node, query: &Query) -> Result<Point, Failure> {
    // An empty path matches nothing on purpose. Checked here rather than only
    // in the CLI parser, because an LLM can emit the `Query` structure directly
    // and bypass it — the standing reason every selector check lives server
    // side as well as at the edge.
    // `split_last` yields `(last, rest)`: the final predicate is the target,
    // and the rest are the ancestor predicates the descendant axis walks.
    let Some((last, prefix)) = query.path.split_last() else {
        return Err(Failure::BadQuery(
            "an empty path matches nothing".to_owned(),
        ));
    };

    // Every candidate as its target and ancestor chain, in document order. The
    // two are kept distinct so the target — the node whose bounds supply the
    // point — is a plain `&Node`, not something fished out of a path with an
    // invariant to prove.
    let mut candidates: Vec<(&Node, Vec<&Node>)> = Vec::new();
    let mut ancestors: Vec<&Node> = Vec::new();
    collect(tree, &mut ancestors, prefix, last, &mut candidates);

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

/// Depth-first walk collecting `(target, ancestors)` pairs that match the
/// query.
///
/// A node is a candidate when it matches `last` and `prefix` is a subsequence
/// of its ancestor chain — i.e. each earlier predicate matches some ancestor,
/// in order, with the last predicate on the node itself. That is the
/// descendant axis: a single-element `prefix`-free path matches anywhere, and
/// more elements narrow by structure without requiring a direct-child chain.
fn collect<'a>(
    node: &'a Node,
    ancestors: &mut Vec<&'a Node>,
    prefix: &[Predicate],
    last: &Predicate,
    out: &mut Vec<(&'a Node, Vec<&'a Node>)>,
) {
    if matches_predicate(node, last) && is_subsequence(ancestors, prefix) {
        out.push((node, ancestors.clone()));
    }

    ancestors.push(node);
    for child in &node.children {
        collect(child, ancestors, prefix, last, out);
    }
    ancestors.pop();
}

/// Whether `node` satisfies `predicate`: role and name, where `None` in the
/// predicate matches anything (including an absent name on the node).
fn matches_predicate(node: &Node, predicate: &Predicate) -> bool {
    let role_ok = predicate
        .role
        .as_deref()
        .is_none_or(|role| role == node.role);
    let name_ok = predicate
        .name
        .as_deref()
        .is_none_or(|name| node.name.as_deref() == Some(name));
    role_ok && name_ok
}

/// Whether `prefix` matches as a subsequence of `ancestors`.
///
/// Greedy earliest is correct here without backtracking: matching a predicate
/// against an earlier ancestor never hurts, because a later predicate can
/// always use any ancestor after it. An empty `prefix` matches trivially — the
/// single-element-path case, where the target can be anywhere.
fn is_subsequence(ancestors: &[&Node], prefix: &[Predicate]) -> bool {
    let mut wanted = prefix.iter();
    let Some(mut current) = wanted.next() else {
        return true;
    };
    for node in ancestors {
        if matches_predicate(node, current) {
            current = match wanted.next() {
                Some(next) => next,
                None => return true,
            };
        }
    }
    // `current` was never satisfied: either prefix outlasted the ancestors, or
    // the last wanted predicate found no match.
    let _ = current;
    false
}

/// Build the [`Match`] a caller sees on an ambiguous query: the target's
/// identity and bounds, plus the *full* ancestor chain as predicates — not just
/// the matched subsequence — so appending the target's own role/name yields a
/// [`Query`] that selects this position alone.
fn to_match(target: &Node, ancestors: &[&Node]) -> Match {
    Match {
        role: target.role.clone(),
        name: target.name.clone(),
        bounds: target.bounds,
        path: ancestors
            .iter()
            .map(|node| Predicate {
                role: Some(node.role.clone()),
                name: node.name.clone(),
            })
            .collect(),
    }
}

/// The centre of a rectangle, as a click point.
///
/// Floor division, so the centre of a 100×30 rect at (10, 20) is (60, 35). The
/// `a11y` walk only produces bounds with positive size — anything not laid out
/// is `None` — but a hand-built tree in a test could carry any `Rect`; the
/// `try_from` keeps this total rather than panicking on a width that overflows
/// `i32`.
fn centre(bounds: &Rect) -> Point {
    Point {
        x: bounds.origin.x + i32::try_from(bounds.size.width / 2).unwrap_or(i32::MAX),
        y: bounds.origin.y + i32::try_from(bounds.size.height / 2).unwrap_or(i32::MAX),
    }
}

#[cfg(test)]
mod tests {
    //! The resolution logic, checked against hand-built trees. Every failure
    //! branch is exercised directly, and the one defence worth breaking — that
    //! the point follows the node's bounds rather than being a cached coordinate
    //! — is too: a selector that only "matched the Reconnect node" reads as
    //! coverage and is worse than none.

    use super::*;
    use waypuppet_proto::{Rect, Size};

    /// A node with role, optional name, optional bounds, and children.
    fn node(role: &str, name: Option<&str>, bounds: Option<Rect>, children: Vec<Node>) -> Node {
        Node {
            role: role.to_owned(),
            name: name.map(str::to_owned),
            bounds,
            children,
        }
    }

    /// A rectangle from origin and size.
    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect {
            origin: Point { x, y },
            size: Size {
                width: w,
                height: h,
            },
        }
    }

    /// The three-node fixture from `tests/tree.rs`: an application, one frame,
    /// one button inside it.
    fn fixture(button_bounds: Rect) -> Node {
        node(
            "application",
            Some("fake-app"),
            None,
            vec![node(
                "frame",
                Some("fake window"),
                Some(rect(0, 0, 320, 240)),
                vec![node(
                    "button",
                    Some("Reconnect"),
                    Some(button_bounds),
                    Vec::new(),
                )],
            )],
        )
    }

    /// `Predicate { role, name }`, spelled out for readability.
    fn pred(role: Option<&str>, name: Option<&str>) -> Predicate {
        Predicate {
            role: role.map(str::to_owned),
            name: name.map(str::to_owned),
        }
    }

    /// A query over a path of predicates, no index (the unique-match case).
    fn query(preds: &[Predicate]) -> Query {
        Query {
            path: preds.to_vec(),
            index: None,
        }
    }

    #[test]
    fn a_flat_role_match_resolves_to_the_centre() {
        let tree = fixture(rect(10, 20, 100, 30));
        let point = resolve(&tree, &query(&[pred(Some("button"), None)])).unwrap();
        assert_eq!(point, Point { x: 60, y: 35 });
    }

    #[test]
    fn a_named_match_resolves_to_the_centre() {
        let tree = fixture(rect(10, 20, 100, 30));
        let point = resolve(&tree, &query(&[pred(Some("button"), Some("Reconnect"))])).unwrap();
        assert_eq!(point, Point { x: 60, y: 35 });
    }

    #[test]
    fn a_name_only_predicate_matches_any_role() {
        let tree = fixture(rect(10, 20, 100, 30));
        // `role: None` matches anything, so the name alone finds the button.
        let point = resolve(&tree, &query(&[pred(None, Some("Reconnect"))])).unwrap();
        assert_eq!(point, Point { x: 60, y: 35 });
    }

    #[test]
    fn a_name_predicate_does_not_match_an_absent_name() {
        let tree = fixture(rect(10, 20, 100, 30));
        let err = resolve(&tree, &query(&[pred(Some("button"), Some("Save"))])).unwrap_err();
        assert!(matches!(err, Failure::NoMatch), "{err:?}");
    }

    #[test]
    fn no_match_is_reported_rather_than_panicked() {
        let tree = fixture(rect(10, 20, 100, 30));
        let err = resolve(&tree, &query(&[pred(Some("text"), None)])).unwrap_err();
        assert!(matches!(err, Failure::NoMatch), "{err:?}");
    }

    #[test]
    fn an_empty_path_is_a_bad_query() {
        let tree = fixture(rect(10, 20, 100, 30));
        let err = resolve(&tree, &query(&[])).unwrap_err();
        assert!(matches!(err, Failure::BadQuery(_)), "{err:?}");
    }

    #[test]
    fn a_node_with_no_bounds_is_matched_loudly() {
        // The collapsed-menu case: matched, but there is nowhere to click.
        let tree = fixture(rect(10, 20, 100, 30));
        let mut tree = tree;
        tree.children[0].children[0].bounds = None;
        let err = resolve(&tree, &query(&[pred(Some("button"), None)])).unwrap_err();
        assert!(matches!(err, Failure::MatchedNoBounds), "{err:?}");
    }

    #[test]
    fn an_ambiguous_query_reports_every_match_with_its_ancestor_path() {
        // Two buttons, both at the root level, so both are candidates. The
        // query is flat and carries no index, so this is ambiguous.
        let tree = node(
            "application",
            Some("app"),
            None,
            vec![
                node("button", Some("Save"), Some(rect(0, 0, 10, 10)), Vec::new()),
                node("frame", Some("f"), None, Vec::new()),
                node(
                    "button",
                    Some("Open"),
                    Some(rect(20, 0, 10, 10)),
                    Vec::new(),
                ),
            ],
        );

        let err = resolve(&tree, &query(&[pred(Some("button"), None)])).unwrap_err();
        let Failure::Ambiguous { count, matches } = err else {
            panic!("expected Ambiguous, got {err:?}");
        };
        assert_eq!(count, 2);
        assert_eq!(matches.len(), 2);

        // Document order: Save first, Open second.
        assert_eq!(matches[0].role, "button");
        assert_eq!(matches[0].name.as_deref(), Some("Save"));
        assert_eq!(matches[0].bounds, Some(rect(0, 0, 10, 10)));
        // The full ancestor chain, root to parent: just the application.
        assert_eq!(
            matches[0].path,
            vec![pred(Some("application"), Some("app"))]
        );

        assert_eq!(matches[1].name.as_deref(), Some("Open"));
        assert_eq!(matches[1].bounds, Some(rect(20, 0, 10, 10)));
        assert_eq!(
            matches[1].path,
            vec![pred(Some("application"), Some("app"))]
        );
    }

    #[test]
    fn an_index_selects_the_nth_candidate_in_document_order() {
        let tree = node(
            "application",
            None,
            None,
            vec![
                node("button", Some("Save"), Some(rect(0, 0, 10, 10)), Vec::new()),
                node(
                    "button",
                    Some("Open"),
                    Some(rect(20, 0, 10, 10)),
                    Vec::new(),
                ),
            ],
        );

        let query = Query {
            path: vec![pred(Some("button"), None)],
            index: Some(1),
        };
        let point = resolve(&tree, &query).unwrap();
        // The second button, at (20, 0, 10, 10): centre (25, 5).
        assert_eq!(point, Point { x: 25, y: 5 });
    }

    #[test]
    fn an_out_of_range_index_is_no_match() {
        let tree = node(
            "application",
            None,
            None,
            vec![node(
                "button",
                Some("Save"),
                Some(rect(0, 0, 10, 10)),
                Vec::new(),
            )],
        );

        let query = Query {
            path: vec![pred(Some("button"), None)],
            index: Some(3),
        };
        let err = resolve(&tree, &query).unwrap_err();
        assert!(matches!(err, Failure::NoMatch), "{err:?}");
    }

    #[test]
    fn a_descendant_axis_narrows_by_structure() {
        // A button under a frame, and a stray button at the root. The query
        // `[frame, button]` must match only the framed one.
        let tree = node(
            "application",
            None,
            None,
            vec![
                node(
                    "frame",
                    None,
                    None,
                    vec![node(
                        "button",
                        Some("in frame"),
                        Some(rect(5, 5, 10, 10)),
                        Vec::new(),
                    )],
                ),
                node(
                    "button",
                    Some("stray"),
                    Some(rect(50, 50, 10, 10)),
                    Vec::new(),
                ),
            ],
        );

        let point = resolve(
            &tree,
            &query(&[pred(Some("frame"), None), pred(Some("button"), None)]),
        )
        .unwrap();
        assert_eq!(point, Point { x: 10, y: 10 });
    }

    #[test]
    fn a_descendant_axis_matches_through_intervening_levels() {
        // Not a direct-child chain: app > frame > panel > button. The query
        // `[app, button]` still finds the button two levels under the app.
        let tree = node(
            "application",
            Some("app"),
            None,
            vec![node(
                "frame",
                None,
                None,
                vec![node(
                    "panel",
                    None,
                    None,
                    vec![node(
                        "button",
                        Some("deep"),
                        Some(rect(0, 0, 20, 20)),
                        Vec::new(),
                    )],
                )],
            )],
        );

        let point = resolve(
            &tree,
            &query(&[pred(Some("application"), None), pred(Some("button"), None)]),
        )
        .unwrap();
        assert_eq!(point, Point { x: 10, y: 10 });
    }

    #[test]
    fn a_descendant_axis_requires_the_ancestors_in_order() {
        // A button whose only ancestor is a panel, not a frame. The query
        // `[frame, button]` must not match it.
        let tree = node(
            "application",
            None,
            None,
            vec![node(
                "panel",
                None,
                None,
                vec![node(
                    "button",
                    Some("p"),
                    Some(rect(0, 0, 10, 10)),
                    Vec::new(),
                )],
            )],
        );

        let err = resolve(
            &tree,
            &query(&[pred(Some("frame"), None), pred(Some("button"), None)]),
        )
        .unwrap_err();
        assert!(matches!(err, Failure::NoMatch), "{err:?}");
    }

    // The defence worth breaking: a selector that resolves to a fixed
    // coordinate instead of the node's current bounds would pass every other
    // test in this file. This one moves the bounds and confirms the point
    // follows — the whole point of selecting a widget rather than a point.
    #[test]
    fn moving_the_bounds_moves_the_resolved_point() {
        let tree = fixture(rect(10, 20, 100, 30));
        let query = query(&[pred(Some("button"), Some("Reconnect"))]);

        let before = resolve(&tree, &query).unwrap();
        assert_eq!(before, Point { x: 60, y: 35 });

        let mut moved = tree;
        moved.children[0].children[0].bounds = Some(rect(100, 200, 40, 60));
        let after = resolve(&moved, &query).unwrap();
        assert_eq!(after, Point { x: 120, y: 230 });
    }
}
