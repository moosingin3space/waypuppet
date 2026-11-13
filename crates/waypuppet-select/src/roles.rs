//! The AT-SPI role vocabulary, and what to say when a selector names something
//! that is not in it.
//!
//! Roles are a *closed* set — AT-SPI sends a `u32` and `atspi` turns it into one
//! of these names — and that is the fact this module trades on. A caller who
//! writes `textbox` has not written a selector that matches nothing; they have
//! written a selector that cannot match anything, and those are different
//! failures. Reporting the second as [`Failure::NoMatch`] would be the same
//! look-alike collapse as reporting a stalled loop as a timeout: true in the
//! narrow sense, useless for fixing it.
//!
//! [`Failure::NoMatch`]: waypuppet_proto::Failure::NoMatch
//!
//! So an unknown role is refused where it is written, with the closest real
//! roles named. The vocabulary is small enough that "closest" is exact rather
//! than a guess: every role within edit distance three is considered, and every
//! role at the *minimum* distance is offered. There is no cap on that list, so a
//! suggestion is never dropped for fitting badly in a terminal — see
//! `suggest`'s own note.
//!
//! Input is normalised before lookup: case folds, and spaces, hyphens and
//! underscores are all dropped. `list item`, `list-item`, `list_item` and
//! `ListItem` are one role, which costs nothing — the 130 names stay distinct
//! under that normalisation.
//!
//! This table is a *copy* of what `atspi::Role::name` produces, because the CLI
//! has to refuse an unknown role without linking a D-Bus stack. A copy drifts,
//! and this one would drift silently — a role added upstream would simply become
//! unselectable, reported as "not an AT-SPI role" to a caller looking straight
//! at it in `waypuppet tree`. So `a11y`'s own tests, in the one crate that has
//! both, hold the two against each other.

use waypuppet_proto::{Case, MatchOp, Matcher};

/// Every AT-SPI role name, exactly as `atspi::Role::name` renders it.
///
/// This is the vocabulary a selector's type token and `[role=…]` resolve
/// against, and the canonical spelling stored in a [`Query`] — so the resolver
/// compares against [`Node::role`] literally, with no normalisation at match
/// time. Canonicalising once, at the edge where it was written, is the same
/// instinct as compiling a `wait --title` pattern when the wait is parked.
///
/// `invalid` is in the list because `atspi` can produce it; a selector naming it
/// is odd but not wrong, and trimming the table to what seems reasonable is how
/// it would drift out of step with the source of truth.
///
/// [`Query`]: waypuppet_proto::Query
/// [`Node::role`]: waypuppet_proto::Node::role
pub const ROLES: &[&str] = &[
    "invalid",
    "accelerator label",
    "alert",
    "animation",
    "arrow",
    "calendar",
    "canvas",
    "check box",
    "check menu item",
    "color chooser",
    "column header",
    "combo box",
    "date editor",
    "desktop icon",
    "desktop frame",
    "dial",
    "dialog",
    "directory pane",
    "drawing area",
    "file chooser",
    "filler",
    "focus traversable",
    "font chooser",
    "frame",
    "glass pane",
    "html container",
    "icon",
    "image",
    "internal frame",
    "label",
    "layered pane",
    "list",
    "list item",
    "menu",
    "menu bar",
    "menu item",
    "option pane",
    "page tab",
    "page tab list",
    "panel",
    "password text",
    "popup menu",
    "progress bar",
    "button",
    "radio button",
    "radio menu item",
    "root pane",
    "row header",
    "scroll bar",
    "scroll pane",
    "separator",
    "slider",
    "spin button",
    "split pane",
    "status bar",
    "table",
    "table cell",
    "table column header",
    "table row header",
    "tearoff menu item",
    "terminal",
    "text",
    "toggle button",
    "tool bar",
    "tool tip",
    "tree",
    "tree table",
    "unknown",
    "viewport",
    "window",
    "extended",
    "header",
    "footer",
    "paragraph",
    "ruler",
    "application",
    "autocomplete",
    "editbar",
    "embedded",
    "entry",
    "chart",
    "caption",
    "document frame",
    "heading",
    "page",
    "section",
    "redundant object",
    "form",
    "link",
    "input method window",
    "table row",
    "tree item",
    "document spreadsheet",
    "document presentation",
    "document text",
    "document web",
    "document email",
    "comment",
    "list box",
    "grouping",
    "image map",
    "notification",
    "info bar",
    "level bar",
    "title bar",
    "block quote",
    "audio",
    "video",
    "definition",
    "article",
    "landmark",
    "log",
    "marquee",
    "math",
    "rating",
    "timer",
    "static",
    "math fraction",
    "math root",
    "subscript",
    "superscript",
    "description list",
    "description term",
    "description value",
    "footnote",
    "content deletion",
    "content insertion",
    "mark",
    "suggestion",
    "push button menu",
];

/// How far a suggestion may be from what was written before it stops being a
/// suggestion and starts being noise.
const SUGGEST_WITHIN: usize = 3;

/// How much of a role a name has to contain before containment counts as a
/// signal. Below this, everything contains everything: `e` is inside half the
/// vocabulary and means nothing.
const CONTAINMENT_MIN: usize = 4;

/// The canonical spelling of a role, or `None` if it is not one.
///
/// Generous about how it was written — case, spaces, hyphens and underscores are
/// all levelled — and exact about what comes back: always an entry of [`ROLES`],
/// which is what the resolver compares against.
#[must_use]
pub fn canonical(written: &str) -> Option<&'static str> {
    let wanted = normalise(written);
    ROLES.iter().copied().find(|role| normalise(role) == wanted)
}

/// The real roles closest to something that is not one, best first.
///
/// Every role at the minimum edit distance is returned, provided that minimum is
/// within [`SUGGEST_WITHIN`]. There is deliberately no cap: a caller — or a
/// model — reading this is trying to find the name it meant, and a truncated
/// list can omit exactly the one it needed. In practice the minimum-distance set
/// is one or two entries, so the honest rule and the tidy one agree.
///
/// Empty means nothing was close, which is itself worth saying: it usually means
/// the caller wrote a *class* of widget AT-SPI does not have a role for.
#[must_use]
pub fn suggest(written: &str) -> Vec<&'static str> {
    let wanted = normalise(written);
    if wanted.is_empty() {
        return Vec::new();
    }

    let scored: Vec<(usize, &'static str)> = ROLES
        .iter()
        .filter_map(|role| {
            let key = normalise(role);
            let apart = distance(&wanted, &key);
            // Edit distance alone misses the case where a caller wrote a real
            // role with extra words welded on — `pushbutton` is four edits from
            // both `button` and `push button menu`, which is far enough to look
            // like noise and is in fact exactly right. Containment catches that
            // class, and the length floor keeps it from catching everything.
            let embedded = wanted.len().min(key.len()) >= CONTAINMENT_MIN
                && (wanted.contains(key.as_str()) || key.contains(wanted.as_str()));
            (apart <= SUGGEST_WITHIN || embedded).then_some((apart, *role))
        })
        .collect();
    let Some(best) = scored.iter().map(|(apart, _)| *apart).min() else {
        return Vec::new();
    };

    let mut closest: Vec<&'static str> = scored
        .into_iter()
        .filter(|(apart, _)| *apart == best)
        .map(|(_, role)| role)
        .collect();
    closest.sort_unstable();
    closest
}

/// Whether any real role could satisfy this matcher.
///
/// The point of asking: `[role*="Button"]` is well-formed, names nothing
/// impossible on its face, and can never match — role names are lower case, so
/// the capital B rules out all 130. Checking against the vocabulary turns that
/// into an error where it was written rather than an empty result later, which
/// is the same trade [`canonical`] makes for the type selector.
#[must_use]
pub fn satisfiable(matcher: &Matcher) -> bool {
    ROLES.iter().any(|role| matches(role, matcher))
}

/// Whether a concrete attribute value satisfies a matcher.
///
/// Lives here rather than in the resolver because the vocabulary check above
/// needs exactly the comparison the resolver will make later — if the two
/// disagreed, `satisfiable` would bless queries that never match, which is the
/// failure it exists to prevent.
#[must_use]
pub fn matches(value: &str, matcher: &Matcher) -> bool {
    let Matcher::Matches {
        op,
        value: want,
        case,
    } = matcher
    else {
        // `Present` asks only whether there is a value at all, and there is.
        return true;
    };

    let (have, want) = match case {
        Case::Sensitive => (value.to_owned(), want.clone()),
        Case::Insensitive => (value.to_lowercase(), want.to_lowercase()),
    };
    match op {
        MatchOp::Exact => have == want,
        MatchOp::Contains => have.contains(&want),
        MatchOp::Prefix => have.starts_with(&want),
        MatchOp::Suffix => have.ends_with(&want),
    }
}

/// Case-folded, with every separator dropped, so the spellings a caller might
/// reasonably reach for collapse onto one key.
fn normalise(written: &str) -> String {
    written
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Levenshtein distance, two rows at a time.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }

    let mut previous: Vec<usize> = (0..=a.len()).collect();
    let mut current = vec![0usize; a.len() + 1];
    for (j, bj) in b.iter().enumerate() {
        current[0] = j + 1;
        for (i, ai) in a.iter().enumerate() {
            let substitution = previous[i] + usize::from(ai != bj);
            current[i + 1] = substitution.min(previous[i + 1] + 1).min(current[i] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[a.len()]
}

#[cfg(test)]
mod tests {
    use super::{ROLES, canonical, distance, normalise, suggest};

    #[test]
    fn the_vocabulary_is_distinct_under_normalisation() {
        // The generous spellings `canonical` accepts are only safe if they do
        // not collapse two roles onto one key. If a future AT-SPI release adds
        // one that does, this is where it is caught rather than by a selector
        // silently resolving to the wrong widget.
        let mut keys: Vec<String> = ROLES.iter().map(|role| normalise(role)).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "two roles normalise to the same key");
    }

    #[test]
    fn spellings_a_caller_might_reach_for_all_resolve() {
        for written in [
            "list item",
            "list-item",
            "list_item",
            "ListItem",
            "LIST ITEM",
        ] {
            assert_eq!(canonical(written), Some("list item"), "{written}");
        }
    }

    #[test]
    fn a_role_that_is_not_one_is_not_invented() {
        assert_eq!(canonical("textbox"), None);
        assert_eq!(canonical(""), None);
    }

    #[test]
    fn the_names_a_model_reaches_for_suggest_the_real_one() {
        // Not arbitrary examples: these are the spellings that come from other
        // widget vocabularies — HTML, Qt, WinUI — which is what a generated
        // selector is most likely to carry.
        // `pushbutton` is four edits from `button` — far enough to look like
        // noise on distance alone, which is why containment is also a signal.
        assert!(suggest("pushbutton").contains(&"button"));
        assert!(suggest("checkbox").contains(&"check box"));
        assert!(suggest("menuitem").contains(&"menu item"));
        assert!(suggest("listbox").contains(&"list box"));
    }

    #[test]
    fn nothing_close_suggests_nothing() {
        // Silence is the honest answer when the caller has named a category
        // AT-SPI has no role for. Offering a distant role would be worse than
        // offering none: it reads as confirmation.
        assert_eq!(suggest("qwertyuiopasdfgh"), [] as [&str; 0]);
    }

    #[test]
    fn distance_is_the_textbook_one() {
        assert_eq!(distance("", ""), 0);
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("button", "button"), 0);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("abc", ""), 3);
    }
}
