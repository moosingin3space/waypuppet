//! Properties, over generated selectors and generated trees.
//!
//! One binary, with the generators at the bottom, because a shared `tests/`
//! module goes dead in whichever binary does not use all of it and this crate
//! denies warnings.
//!
//! What is being defended here is not the happy path — `grammar.rs` has the
//! examples — but the claims made *about* the selector language, each of which
//! is the kind of thing that is true for every case somebody thought of and
//! false for one they did not:
//!
//! - printing a query and parsing it back is the identity, so every selector a
//!   diagnostic hands a caller means what it appears to mean;
//! - the text door and the JSON door land on the same structure, so the wire
//!   does not grow a second dialect;
//! - a one-character typo in a role still names the role it meant, which is the
//!   entire argument for a closed vocabulary;
//! - parsing is total, and every rejection points inside the input and on a
//!   character boundary;
//! - resolution is total, and the selector an ambiguous result hands back
//!   really does pick the node it came from.
//!
//! Two generator decisions do the heavy lifting. Roles come from the real
//! vocabulary — a tree of invented role names would make every property pass
//! vacuously, since nothing would parse. Names are hostile — quotes,
//! backslashes, newlines, arbitrary unicode — because that is where a quoting
//! bug lives, and an accessible name is whatever the application says it is.

use arbtest::arbitrary::{self, Unstructured};
use arbtest::arbtest;
use waypuppet_proto::{
    Case, Combinator, Failure, MatchOp, Matcher, Node, Point, Predicate, Query, Rect, Size, Step,
};
use waypuppet_select::{ErrorKind, candidates, narrowed, parse, print, resolve, roles, validate};

#[test]
fn printing_a_query_and_parsing_it_back_is_the_identity() {
    arbtest(|u| {
        let query = any_query(u)?;
        let text = print(&query);
        match parse(&text) {
            Ok(reparsed) => assert_eq!(
                reparsed, query,
                "printed as {text:?}, which parsed to something else"
            ),
            Err(error) => panic!("printed {text:?}, which does not parse:\n{error}"),
        }
        Ok(())
    })
    .budget_ms(2_000);
}

#[test]
fn printing_is_canonical() {
    // Weaker than the round trip and worth having anyway: it catches a printer
    // with two spellings for one query, which round-tripping alone would not
    // notice because both would parse back correctly.
    arbtest(|u| {
        let query = any_query(u)?;
        let once = print(&query);
        let twice = print(&parse(&once).expect("a printed selector parses"));
        assert_eq!(once, twice);
        Ok(())
    })
    .budget_ms(1_000);
}

#[test]
fn the_text_door_and_the_json_door_agree() {
    // `validate` is what checks a query that arrived as structure and never met
    // the parser. Anything the parser produces must already be a fixed point of
    // it, or the two doors accept different things and the wire has grown a
    // second dialect.
    arbtest(|u| {
        let parsed = parse(&print(&any_query(u)?)).expect("a printed selector parses");
        assert_eq!(validate(&parsed).expect("a parsed query is valid"), parsed);
        Ok(())
    })
    .budget_ms(1_000);
}

#[test]
fn a_typo_in_a_role_still_names_the_role_it_meant() {
    // The property the closed vocabulary buys, and the whole reason an unknown
    // role is a parse error rather than an empty result. One edit to a real
    // role: either it still resolves — spaces and hyphens are levelled, so
    // several edits are no-ops — or the rejection names the original.
    arbtest(|u| {
        let role: &str = u.choose(roles::ROLES)?;
        let typo = mutate(u, role)?;
        let selector = format!("[role=\"{typo}\"]");

        match parse(&selector) {
            Ok(_) => assert!(
                roles::canonical(&typo).is_some(),
                "{selector:?} parsed but {typo:?} is not a role"
            ),
            Err(error) => {
                assert!(
                    matches!(error.kind, ErrorKind::UnknownRole { .. }),
                    "{selector:?}: {:?}",
                    error.kind
                );
                let help = error.kind.help();
                assert!(
                    help.contains(&role.replace(' ', "-")),
                    "a one-character typo of {role:?} was refused without naming it: {help}"
                );
            }
        }
        Ok(())
    })
    .budget_ms(2_000);
}

#[test]
fn arbitrary_input_is_refused_rather_than_survived() {
    // Totality, and the two things a diagnostic has to get right before its
    // wording matters: it must point inside the input, and on a character
    // boundary — reporting slices the input by that span, so a span that split
    // a multi-byte character would panic in the act of explaining itself.
    arbtest(|u| {
        let input: String = u.arbitrary()?;
        if let Err(error) = parse(&input) {
            assert!(
                error.span.start <= error.span.end && error.span.end <= input.len(),
                "{input:?}: span {:?} is outside the input",
                error.span
            );
            assert!(
                input.is_char_boundary(error.span.start) && input.is_char_boundary(error.span.end),
                "{input:?}: span {:?} splits a character",
                error.span
            );
            assert_eq!(error.input, input);
            let _ = error.found();
            let rendered = error.to_string();
            assert!(rendered.contains("help:"), "{input:?}: {rendered}");
            assert!(!error.kind.help().is_empty(), "{input:?}");
        }
        Ok(())
    })
    .budget_ms(2_000);
}

#[test]
fn anything_accepted_can_be_written_down_again() {
    // The other half of totality, over input the query generator cannot reach:
    // whatever the parser accepts, the printer has to be able to express, and
    // the parser has to read back the same way.
    arbtest(|u| {
        let input: String = u.arbitrary()?;
        if let Ok(query) = parse(&input) {
            let printed = print(&query);
            assert_eq!(
                parse(&printed).as_ref(),
                Ok(&query),
                "{input:?} parsed, printed as {printed:?}, and did not survive that"
            );
        }
        Ok(())
    })
    .budget_ms(2_000);
}

#[test]
fn resolution_is_total_and_lands_inside_what_it_matched() {
    arbtest(|u| {
        let tree = any_tree(u, 3)?;
        let mut query = any_query(u)?;
        if u.arbitrary()? {
            query.index = Some(u.int_in_range(0..=4)?);
        }

        let found = candidates(&tree, &query);
        match resolve(&tree, &query) {
            Ok(point) => {
                let node = if let Some(index) = query.index {
                    found[index].0
                } else {
                    assert_eq!(
                        found.len(),
                        1,
                        "a unique answer from {} matches",
                        found.len()
                    );
                    found[0].0
                };
                let bounds = node.bounds.expect("a resolved node has bounds");
                assert_eq!(point, centre(bounds), "{}", print(&query));
            }
            Err(Failure::NoMatch) => assert!(
                found.is_empty() || query.index.is_some_and(|index| index >= found.len()),
                "{} matched {} nodes and reported none",
                print(&query),
                found.len()
            ),
            Err(Failure::Ambiguous { count, matches }) => {
                assert_eq!(count, matches.len());
                assert_eq!(count, found.len());
                assert!(query.index.is_none());
            }
            Err(Failure::MatchedNoBounds) => {}
            Err(other) => panic!("resolution invented a failure: {other:?}"),
        }
        Ok(())
    })
    .budget_ms(3_000);
}

#[test]
fn a_node_is_always_selected_by_its_own_position() {
    // The claim behind every ambiguity diagnostic: the ancestor chain of a
    // match, turned into a query, finds that match again. If this can fail,
    // then "narrow it with one of these" is advice that sometimes leads
    // nowhere — and `NoMatch` in particular is unreachable here, because the
    // node the query was built from satisfies it by construction.
    arbtest(|u| {
        let tree = any_tree(u, 3)?;
        let mut every = Vec::new();
        walk(&tree, &mut Vec::new(), &mut every);
        let chosen = u.choose(&every)?;
        let (node, ancestors) = (chosen.0, &chosen.1);

        let query = anchored(node, ancestors);
        match resolve(&tree, &query) {
            Ok(point) => {
                assert_eq!(point, centre(node.bounds.expect("resolved, so laid out")));
            }
            Err(Failure::Ambiguous { matches, .. }) => {
                // Legitimate: two siblings with the same role and name are the
                // same position as far as a selector can tell.
                assert!(matches.iter().any(|found| found.role == node.role
                    && found.name == node.name
                    && found.bounds == node.bounds));
            }
            Err(Failure::MatchedNoBounds) => assert!(node.bounds.is_none()),
            Err(other) => panic!("a node did not match its own position: {other:?}"),
        }
        Ok(())
    })
    .budget_ms(3_000);
}

#[test]
fn the_selectors_an_ambiguous_result_hands_back_parse_and_resolve() {
    // End to end on the constructive diagnostic: whatever `Ambiguous` offers
    // has to be something a caller can paste back in. A `Match` whose printed
    // form did not parse would make the advice worse than silence.
    arbtest(|u| {
        let tree = any_tree(u, 3)?;
        let query = any_query(u)?;
        if let Err(Failure::Ambiguous { matches, .. }) = resolve(&tree, &query) {
            for found in &matches {
                let selector = narrowed(found);
                let narrowed_query =
                    parse(&selector).unwrap_or_else(|e| panic!("{selector:?}: {e}"));
                match resolve(&tree, &narrowed_query) {
                    // Unique, or still ambiguous between indistinguishable
                    // twins, or not laid out — but never "no such node".
                    Ok(_) | Err(Failure::Ambiguous { .. } | Failure::MatchedNoBounds) => {}
                    Err(other) => panic!("{selector:?} was offered and then failed: {other:?}"),
                }
            }
        }
        Ok(())
    })
    .budget_ms(3_000);
}

// --- generators -------------------------------------------------------------

/// Names that look like something a toolkit would publish, mixed with the
/// arbitrary ones so the corpus is not all line noise.
const REALISTIC: &[&str] = &[
    "Save",
    "Save As…",
    "Cancel",
    "Reconnect",
    "_Open",
    "OK",
    "",
    "Ch 3 — Notes",
    "quote\"inside",
    "back\\slash",
];

/// A query the grammar can express: real roles, satisfiable role fragments, and
/// no index — position is not part of the text form.
fn any_query(u: &mut Unstructured<'_>) -> arbitrary::Result<Query> {
    let first = any_predicate(u, 1)?;
    let steps = u.int_in_range(0..=2)?;
    let mut rest = Vec::with_capacity(steps);
    for _ in 0..steps {
        rest.push(Step {
            combinator: if u.arbitrary()? {
                Combinator::Descendant
            } else {
                Combinator::Child
            },
            predicate: any_predicate(u, 1)?,
        });
    }
    Ok(Query {
        first,
        rest,
        index: None,
    })
}

/// One step. `depth` bounds `:not(…)` nesting, which the grammar allows to
/// recurse but which nothing real needs more than a level of.
fn any_predicate(u: &mut Unstructured<'_>, depth: usize) -> arbitrary::Result<Predicate> {
    let role = if u.arbitrary()? {
        Some(any_role_matcher(u)?)
    } else {
        None
    };
    let name = if u.arbitrary()? {
        Some(any_name_matcher(u)?)
    } else {
        None
    };
    let excluded = if depth == 0 {
        0
    } else {
        u.int_in_range(0..=1)?
    };
    let mut not = Vec::with_capacity(excluded);
    for _ in 0..excluded {
        not.push(any_predicate(u, depth - 1)?);
    }
    Ok(Predicate { role, name, not })
}

/// A role matcher some real role satisfies — which is what the parser insists
/// on, so generating anything else would only re-test the rejection path.
fn any_role_matcher(u: &mut Unstructured<'_>) -> arbitrary::Result<Matcher> {
    let role: &str = u.choose(roles::ROLES)?;
    let case = if u.arbitrary()? {
        Case::Sensitive
    } else {
        Case::Insensitive
    };
    Ok(match u.int_in_range(0..=4)? {
        0 => Matcher::Present,
        1 => Matcher::Matches {
            op: MatchOp::Exact,
            value: role.to_owned(),
            case,
        },
        2 => Matcher::Matches {
            op: MatchOp::Prefix,
            value: fragment(u, role, true, false)?,
            case,
        },
        3 => Matcher::Matches {
            op: MatchOp::Suffix,
            value: fragment(u, role, false, true)?,
            case,
        },
        _ => Matcher::Matches {
            op: MatchOp::Contains,
            value: fragment(u, role, false, false)?,
            case,
        },
    })
}

/// A slice of a real role name, so the matcher is satisfiable.
fn fragment(
    u: &mut Unstructured<'_>,
    role: &str,
    from_start: bool,
    to_end: bool,
) -> arbitrary::Result<String> {
    let chars: Vec<char> = role.chars().collect();
    let start = if from_start {
        0
    } else {
        u.int_in_range(0..=chars.len() - 1)?
    };
    let end = if to_end {
        chars.len()
    } else {
        u.int_in_range(start + 1..=chars.len())?
    };
    Ok(chars[start..end].iter().collect())
}

/// A name matcher over a hostile value.
fn any_name_matcher(u: &mut Unstructured<'_>) -> arbitrary::Result<Matcher> {
    if u.int_in_range(0..=5)? == 0 {
        return Ok(Matcher::Present);
    }
    Ok(Matcher::Matches {
        op: match u.int_in_range(0..=3)? {
            0 => MatchOp::Exact,
            1 => MatchOp::Contains,
            2 => MatchOp::Prefix,
            _ => MatchOp::Suffix,
        },
        value: any_name(u)?,
        case: if u.arbitrary()? {
            Case::Sensitive
        } else {
            Case::Insensitive
        },
    })
}

/// An accessible name: sometimes plausible, sometimes whatever the fuzzer has.
fn any_name(u: &mut Unstructured<'_>) -> arbitrary::Result<String> {
    if u.arbitrary()? {
        Ok((*u.choose(REALISTIC)?).to_owned())
    } else {
        u.arbitrary()
    }
}

/// A tree of nodes with real roles, bounded in depth and breadth.
fn any_tree(u: &mut Unstructured<'_>, depth: usize) -> arbitrary::Result<Node> {
    let role: &str = u.choose(roles::ROLES)?;
    let children = if depth == 0 {
        Vec::new()
    } else {
        let count = u.int_in_range(0..=3)?;
        let mut children = Vec::with_capacity(count);
        for _ in 0..count {
            children.push(any_tree(u, depth - 1)?);
        }
        children
    };
    Ok(Node {
        role: role.to_owned(),
        name: if u.arbitrary()? {
            Some(any_name(u)?)
        } else {
            None
        },
        bounds: if u.arbitrary()? {
            Some(any_rect(u)?)
        } else {
            None
        },
        children,
    })
}

/// Bounds small enough that arithmetic on them is uninteresting, so a failure
/// is about matching rather than about overflow.
fn any_rect(u: &mut Unstructured<'_>) -> arbitrary::Result<Rect> {
    Ok(Rect {
        origin: Point {
            x: u.int_in_range(-1000..=1000)?,
            y: u.int_in_range(-1000..=1000)?,
        },
        size: Size {
            width: u.int_in_range(0..=2000)?,
            height: u.int_in_range(0..=2000)?,
        },
    })
}

/// One edit — insert, delete or substitute — at a random position. The
/// alphabet carries a space and a hyphen so the cases that normalise away are
/// generated too.
fn mutate(u: &mut Unstructured<'_>, role: &str) -> arbitrary::Result<String> {
    const ALPHABET: &[char] = &[
        'a', 'b', 'c', 'd', 'e', 'i', 'l', 'm', 'n', 'o', 'r', 's', 't', 'u', 'x', ' ', '-',
    ];
    let mut chars: Vec<char> = role.chars().collect();
    let at = u.int_in_range(0..=chars.len() - 1)?;
    match u.int_in_range(0..=2)? {
        0 => chars.insert(at, *u.choose(ALPHABET)?),
        1 => {
            chars.remove(at);
        }
        _ => chars[at] = *u.choose(ALPHABET)?,
    }
    Ok(chars.into_iter().collect())
}

/// Every node in document order, each with its ancestor chain.
fn walk<'a>(
    node: &'a Node,
    ancestors: &mut Vec<&'a Node>,
    out: &mut Vec<(&'a Node, Vec<&'a Node>)>,
) {
    out.push((node, ancestors.clone()));
    ancestors.push(node);
    for child in &node.children {
        walk(child, ancestors, out);
    }
    ancestors.pop();
}

/// The fully anchored query naming one position: every ancestor as a
/// direct-child step, then the node itself, all by exact role and name. The
/// structural form of what [`narrowed`] prints.
fn anchored(node: &Node, ancestors: &[&Node]) -> Query {
    let mut chain = ancestors.iter().map(|node| identity(node));
    let first = chain.next().unwrap_or_else(|| identity(node));
    let mut rest: Vec<Step> = chain
        .map(|predicate| Step {
            combinator: Combinator::Child,
            predicate,
        })
        .collect();
    if !ancestors.is_empty() {
        rest.push(Step {
            combinator: Combinator::Child,
            predicate: identity(node),
        });
    }
    Query {
        first,
        rest,
        index: None,
    }
}

/// A node as the predicate matching exactly it.
fn identity(node: &Node) -> Predicate {
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

/// The centre of a rectangle, spelled out here rather than imported so the test
/// does not check the resolver against itself.
fn centre(bounds: Rect) -> Point {
    Point {
        x: bounds.origin.x + i32::try_from(bounds.size.width / 2).unwrap(),
        y: bounds.origin.y + i32::try_from(bounds.size.height / 2).unwrap(),
    }
}
