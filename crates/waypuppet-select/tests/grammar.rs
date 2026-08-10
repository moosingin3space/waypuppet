//! The grammar and its diagnostics, by example.
//!
//! The generated properties live in `properties.rs`; these are the cases worth
//! naming. Most of them are rejections, which is deliberate: a selector language
//! is judged on what it does with the things a caller writes that it does not
//! support, and every one of those has its own right answer here.

use waypuppet_proto::{Case, Combinator, Failure, MatchOp, Matcher, Node, Point, Rect, Size};
use waypuppet_select::{ErrorKind, narrowed, parse, print, resolve};

/// One representative input per [`ErrorKind`], and the label the exhaustive
/// match below gives it.
///
/// Doubles as documentation: the catalogue of everything a caller can write
/// that waypuppet refuses, beside the shortest way to write it.
const SAMPLES: &[(&str, &str)] = &[
    ("", "empty"),
    ("textbox", "unknown-role"),
    ("list item", "spaced-role"),
    (r#"[role*="Button"]"#, "unsatisfiable-role"),
    (r#"[text="Save"]"#, "unknown-attribute"),
    (r#"button[name="a"][name="b"]"#, "repeated-attribute"),
    ("button.primary", "class"),
    ("button#save", "id"),
    ("button::before", "pseudo-element"),
    ("button:nth-child(2)", "pseudo-class"),
    ("button + button", "sibling-combinator"),
    ("button, frame", "selector-list"),
    ("> button", "leading-combinator"),
    ("dialog >", "dangling-combinator"),
    ("//button[@name='Save']", "looks-like-xpath"),
    (r#"[name~="Save"]"#, "unsupported-operator"),
    ("[name=]", "expected-value"),
    (r#"[name="Save]"#, "unterminated-string"),
    (r#"[name="Save""#, "unclosed"),
    (r#"[name="save" q]"#, "bad-case-flag"),
    ("[]", "empty-step"),
    ("button%", "unexpected"),
];

/// A stable name per variant. The match is exhaustive on purpose: a new
/// [`ErrorKind`] cannot be added without coming here, and once here it needs a
/// sample above or `every_rejection_is_represented` fails.
fn label(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Empty => "empty",
        ErrorKind::UnknownRole { .. } => "unknown-role",
        ErrorKind::SpacedRole { .. } => "spaced-role",
        ErrorKind::UnsatisfiableRole { .. } => "unsatisfiable-role",
        ErrorKind::UnknownAttribute { .. } => "unknown-attribute",
        ErrorKind::RepeatedAttribute { .. } => "repeated-attribute",
        ErrorKind::Class { .. } => "class",
        ErrorKind::Id { .. } => "id",
        ErrorKind::PseudoElement { .. } => "pseudo-element",
        ErrorKind::PseudoClass { .. } => "pseudo-class",
        ErrorKind::SiblingCombinator { .. } => "sibling-combinator",
        ErrorKind::SelectorList => "selector-list",
        ErrorKind::LeadingCombinator { .. } => "leading-combinator",
        ErrorKind::DanglingCombinator { .. } => "dangling-combinator",
        ErrorKind::LooksLikeXPath => "looks-like-xpath",
        ErrorKind::UnsupportedOperator { .. } => "unsupported-operator",
        ErrorKind::ExpectedValue => "expected-value",
        ErrorKind::UnterminatedString { .. } => "unterminated-string",
        ErrorKind::Unclosed { .. } => "unclosed",
        ErrorKind::BadCaseFlag { .. } => "bad-case-flag",
        ErrorKind::EmptyStep => "empty-step",
        ErrorKind::Unexpected { .. } => "unexpected",
    }
}

#[test]
fn each_sample_is_refused_the_way_it_was_written() {
    // Not "is refused" — refusing everything with one message would pass that.
    // The claim is that the *specific* mistake is identified, because each of
    // these has a different right answer and the message is where it lives.
    for (input, expected) in SAMPLES {
        let error = parse(input).unwrap_err();
        assert_eq!(
            label(&error.kind),
            *expected,
            "{input:?} was refused as {:?}",
            error.kind
        );
    }
}

#[test]
fn every_rejection_is_represented() {
    let covered: std::collections::BTreeSet<&str> = SAMPLES
        .iter()
        .map(|(input, _)| label(&parse(input).unwrap_err().kind))
        .collect();
    let expected: std::collections::BTreeSet<&str> =
        SAMPLES.iter().map(|(_, label)| *label).collect();
    assert_eq!(covered, expected);
}

#[test]
fn every_rejection_names_a_fix() {
    // A diagnostic that only says "no" leaves a caller to guess and a model to
    // guess repeatedly. `help` may not be empty, and may not be the message
    // again wearing a different hat.
    for (input, _) in SAMPLES {
        let error = parse(input).unwrap_err();
        let message = error.kind.message();
        let help = error.kind.help();
        assert!(!message.is_empty(), "{input:?}");
        assert!(!help.is_empty(), "{input:?} says nothing about what to do");
        assert_ne!(help, message, "{input:?} repeats itself instead of helping");

        let rendered = error.to_string();
        assert!(rendered.contains("help:"), "{input:?}: {rendered}");
        assert!(rendered.contains('^'), "{input:?}: {rendered}");
    }
}

#[test]
fn the_help_for_a_css_ism_names_what_to_write_instead() {
    // Each of these is something fluent CSS produces on the first try. Refusing
    // them is table stakes; carrying the caller across to the equivalent is the
    // job.
    let cases = [
        ("button.primary", "[name="),
        ("button#save", "[name="),
        ("button:nth-child(2)", "--index"),
        ("button + button", ">"),
        ("button, frame", "--index"),
        ("//button[@name='Save']", r#"button[name="Save"]"#),
        (r#"[name~="Save"]"#, "*="),
        (r#"[name|="Save"]"#, "^="),
        (r#"[name=~"Sav.*"]"#, "*="),
        (r#"button:contains("Save")"#, "[name*="),
        ("button:checked", "state"),
        ("button:hover", "role and name"),
        ("textbox", "did you mean"),
    ];
    for (input, wanted) in cases {
        let help = parse(input).unwrap_err().kind.help();
        assert!(
            help.contains(wanted),
            "{input:?} should point at {wanted:?}, said: {help}"
        );
    }
}

#[test]
fn a_name_may_contain_anything_including_other_syntax() {
    // An accessible name is whatever the application says it is, and quoting is
    // what makes that safe. The regression: the XPath check used to search the
    // whole input for `[@`, so a widget whose name contained it could not be
    // selected at all — refused as a dialect it was not written in. Found by a
    // property, on a seed that had not come up locally.
    for name in [r"[@", "//button", "a > b", ".class", "#id", ":not("] {
        let selector = format!("tree-item[name=\"{name}\"]");
        let query = parse(&selector).unwrap_or_else(|error| panic!("{selector}: {error}"));
        assert_eq!(query.first.name, Some(exact(name)), "{selector}");
    }

    // Which does not soften the detection: XPath is still XPath where it is
    // actually written, whether or not it leads with a slash.
    for xpath in ["//button[@name='Save']", "button[@name='Save']", "/dialog"] {
        let error = parse(xpath).unwrap_err();
        assert!(
            matches!(error.kind, ErrorKind::LooksLikeXPath),
            "{xpath}: {:?}",
            error.kind
        );
    }
}

#[test]
fn the_diagnostic_points_at_the_offending_text() {
    let error = parse("dialog button.primary").unwrap_err();
    assert_eq!(error.found(), ".primary");
    let rendered = error.to_string();
    assert!(
        rendered.contains("             ^^^^^^^^"),
        "caret is not under `.primary`:\n{rendered}"
    );
}

#[test]
fn a_selector_compiles_to_the_structure_it_looks_like() {
    let query = parse(r#"dialog > button[name*="Save" i]:not([name="Save As…"])"#).unwrap();

    assert_eq!(query.first.role, Some(exact("dialog")));
    assert_eq!(query.rest.len(), 1);
    assert_eq!(query.rest[0].combinator, Combinator::Child);

    let target = &query.rest[0].predicate;
    assert_eq!(target.role, Some(exact("button")));
    assert_eq!(
        target.name,
        Some(Matcher::Matches {
            op: MatchOp::Contains,
            value: "Save".to_owned(),
            case: Case::Insensitive,
        })
    );
    assert_eq!(target.not.len(), 1);
    assert_eq!(target.not[0].name, Some(exact("Save As…")));
}

#[test]
fn a_space_is_the_descendant_axis_and_a_hyphen_is_part_of_a_role() {
    // The one ambiguity CSS's syntax has against AT-SPI's vocabulary, and the
    // whole reason roles are hyphenated in a type selector: `list item` would
    // otherwise be two steps.
    let one_step = parse("list-item").unwrap();
    assert!(one_step.rest.is_empty());
    assert_eq!(one_step.first.role, Some(exact("list item")));

    let two_steps = parse("list button").unwrap();
    assert_eq!(two_steps.rest.len(), 1);
    assert_eq!(two_steps.first.role, Some(exact("list")));
    assert_eq!(two_steps.rest[0].combinator, Combinator::Descendant);
}

#[test]
fn a_role_written_with_its_spaces_is_told_to_hyphenate() {
    // The collision this grammar has to live with, and the reason it gets a
    // diagnostic of its own. Without it, `list item` complains that `item` is
    // not a role and offers `icon` — true, and pointing away from the answer
    // the caller is already holding.
    let error = parse("list item").unwrap_err();
    assert!(
        matches!(error.kind, ErrorKind::SpacedRole { .. }),
        "{error}"
    );
    assert!(error.kind.help().contains("list-item"), "{error}");
    assert_eq!(error.found(), "list item");

    // Longest wins: `page tab list` contains `page tab`, and sending a caller
    // to the shorter one would hand them a selector that parses and matches
    // something else — worse than the error they started with.
    let error = parse("dialog page tab list").unwrap_err();
    assert!(error.kind.help().contains("page-tab-list"), "{error}");
    assert_eq!(error.found(), "page tab list");
}

#[test]
fn a_partial_role_is_allowed_when_some_role_could_satisfy_it() {
    // `[role*="button"]` is the useful case: AT-SPI has four kinds of button
    // and a caller usually does not care which.
    let query = parse(r#"[role*="button"]"#).unwrap();
    assert_eq!(
        query.first.role,
        Some(Matcher::Matches {
            op: MatchOp::Contains,
            value: "button".to_owned(),
            case: Case::Sensitive,
        })
    );

    // And the near-miss it protects: role names are lower case, so this one can
    // never match anything and says so now rather than by returning nothing.
    let error = parse(r#"[role*="Button"]"#).unwrap_err();
    assert!(matches!(error.kind, ErrorKind::UnsatisfiableRole { .. }));
}

// --- resolution -------------------------------------------------------------

/// The three-node fixture from `tests/tree.rs` in the compositor: an
/// application, one frame, one button inside it.
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

#[test]
fn a_selector_resolves_to_the_centre_of_what_it_matched() {
    let tree = fixture(rect(10, 20, 100, 30));
    let query = parse(r#"button[name="Reconnect"]"#).unwrap();
    assert_eq!(resolve(&tree, &query).unwrap(), Point { x: 60, y: 35 });
}

#[test]
fn moving_the_bounds_moves_the_resolved_point() {
    // The defence worth breaking: a resolver that returned a coordinate fixed
    // at parse time would pass every other test in this file. Selecting a
    // widget rather than a point is only worth anything if the point follows
    // the widget.
    let query = parse(r#"button[name="Reconnect"]"#).unwrap();

    let tree = fixture(rect(10, 20, 100, 30));
    assert_eq!(resolve(&tree, &query).unwrap(), Point { x: 60, y: 35 });

    let mut moved = tree;
    moved.children[0].children[0].bounds = Some(rect(100, 200, 40, 60));
    assert_eq!(resolve(&moved, &query).unwrap(), Point { x: 120, y: 230 });
}

#[test]
fn a_child_combinator_means_child_and_a_space_does_not() {
    // app > frame > panel > button. `frame > button` must not reach through
    // the panel; `frame button` must.
    let tree = node(
        "application",
        None,
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
                    vec![],
                )],
            )],
        )],
    );

    assert_eq!(
        resolve(&tree, &parse("frame button").unwrap()).unwrap(),
        Point { x: 10, y: 10 }
    );
    assert!(matches!(
        resolve(&tree, &parse("frame > button").unwrap()),
        Err(Failure::NoMatch)
    ));
    assert_eq!(
        resolve(&tree, &parse("panel > button").unwrap()).unwrap(),
        Point { x: 10, y: 10 }
    );
}

#[test]
fn a_descendant_step_backtracks_past_a_wrong_guess() {
    // `panel > button` under an outer panel that also has a button, but not as
    // a direct child. Taking the first `panel` that fits and giving up would
    // report no match; the right answer is the inner one.
    let tree = node(
        "application",
        None,
        None,
        vec![node(
            "panel",
            Some("outer"),
            None,
            vec![node(
                "filler",
                None,
                None,
                vec![node(
                    "panel",
                    Some("inner"),
                    None,
                    vec![node("button", Some("go"), Some(rect(4, 6, 10, 10)), vec![])],
                )],
            )],
        )],
    );

    let query = parse(r#"panel[name="outer"] panel > button"#).unwrap();
    assert_eq!(resolve(&tree, &query).unwrap(), Point { x: 9, y: 11 });
}

#[test]
fn a_node_with_no_bounds_is_matched_loudly() {
    // The collapsed-menu case: matched, but there is nowhere to click. Silently
    // skipping it would report `NoMatch` for a widget the caller can see in
    // `waypuppet tree`.
    let mut tree = fixture(rect(10, 20, 100, 30));
    tree.children[0].children[0].bounds = None;
    let query = parse("button").unwrap();
    assert!(matches!(
        resolve(&tree, &query),
        Err(Failure::MatchedNoBounds)
    ));
}

#[test]
fn an_ambiguous_query_hands_back_selectors_that_are_not_ambiguous() {
    // The constructive half of `Ambiguous`, and the reason it carries ancestor
    // chains rather than a count: each match renders as a selector that picks
    // it alone, so the fix is to copy one rather than to invent one.
    let tree = node(
        "application",
        Some("app"),
        None,
        vec![
            node("button", Some("Save"), Some(rect(0, 0, 10, 10)), vec![]),
            node("button", Some("Open"), Some(rect(20, 0, 10, 10)), vec![]),
        ],
    );

    let Err(Failure::Ambiguous { count, matches }) = resolve(&tree, &parse("button").unwrap())
    else {
        panic!("two buttons and no index should be ambiguous");
    };
    assert_eq!(count, 2);

    for matched in &matches {
        let selector = narrowed(matched);
        let narrowed_query = parse(&selector).unwrap_or_else(|e| panic!("{selector}: {e}"));
        let point = resolve(&tree, &narrowed_query)
            .unwrap_or_else(|e| panic!("{selector} did not resolve: {e}"));
        let bounds = matched.bounds.unwrap();
        assert_eq!(
            point,
            Point {
                x: bounds.origin.x + 5,
                y: bounds.origin.y + 5
            },
            "{selector} resolved somewhere else"
        );
    }

    assert_eq!(
        narrowed(&matches[0]),
        r#"application[name="app"] > button[name="Save"]"#
    );
}

#[test]
fn an_index_selects_in_document_order() {
    let tree = node(
        "application",
        None,
        None,
        vec![
            node("button", Some("Save"), Some(rect(0, 0, 10, 10)), vec![]),
            node("button", Some("Open"), Some(rect(20, 0, 10, 10)), vec![]),
        ],
    );

    let mut query = parse("button").unwrap();
    query.index = Some(1);
    assert_eq!(resolve(&tree, &query).unwrap(), Point { x: 25, y: 5 });

    query.index = Some(7);
    assert!(matches!(resolve(&tree, &query), Err(Failure::NoMatch)));
}

#[test]
fn not_excludes_without_widening() {
    let tree = node(
        "application",
        None,
        None,
        vec![
            node("button", Some("Save"), Some(rect(0, 0, 10, 10)), vec![]),
            node("button", Some("Cancel"), Some(rect(20, 0, 10, 10)), vec![]),
        ],
    );

    let query = parse(r#"button:not([name="Cancel"])"#).unwrap();
    assert_eq!(resolve(&tree, &query).unwrap(), Point { x: 5, y: 5 });
}

#[test]
fn a_present_matcher_asks_only_whether_there_is_one() {
    let tree = node(
        "application",
        None,
        None,
        vec![
            node("button", None, Some(rect(0, 0, 10, 10)), vec![]),
            node("button", Some(""), Some(rect(20, 0, 10, 10)), vec![]),
        ],
    );

    // The empty name is a name; the absent one is not. `[name]` tells them
    // apart, which `[name=""]` cannot.
    let query = parse("button[name]").unwrap();
    assert_eq!(resolve(&tree, &query).unwrap(), Point { x: 25, y: 5 });
    assert_eq!(print(&query), "button[name]");
}

fn exact(value: &str) -> Matcher {
    Matcher::Matches {
        op: MatchOp::Exact,
        value: value.to_owned(),
        case: Case::Sensitive,
    }
}

fn node(role: &str, name: Option<&str>, bounds: Option<Rect>, children: Vec<Node>) -> Node {
    Node {
        role: role.to_owned(),
        name: name.map(str::to_owned),
        bounds,
        children,
    }
}

fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
    Rect {
        origin: Point { x, y },
        size: Size { width, height },
    }
}
