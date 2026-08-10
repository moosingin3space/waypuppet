//! Text to structure: a CSS selector in, a [`Query`] out.
//!
//! # The grammar
//!
//! ```text
//! selector := step (combinator step)*
//! combinator := WS | WS? '>' WS?
//! step     := (role | '*')? qualifier*  |  qualifier+
//! role     := ident                       -- `button`, `list-item`
//! qualifier := '[' attr (op value flag?)? ']'  |  ':not(' step ')'
//! attr     := 'role' | 'name'
//! op       := '=' | '*=' | '^=' | '$='
//! value    := '"' … '"' | '\'' … '\'' | ident
//! flag     := 'i'
//! ```
//!
//! It is a subset of CSS, chosen so that what a caller already knows transfers
//! and what does not exist is *refused by name* rather than accepted and
//! quietly ignored. `button#save > .primary:nth-child(2)` is a sentence every
//! CSS author can write and none of it means anything against an accessibility
//! tree; each piece of it gets its own diagnostic saying what to write instead.
//! See [`crate::error`] — that catalogue is the reason this is hand-written
//! rather than delegated to a CSS engine, which would parse all of it happily
//! and then match nothing.
//!
//! # Two decisions worth knowing
//!
//! **Roles are resolved here, once.** A type selector is looked up in the
//! closed AT-SPI vocabulary and stored in its canonical spelling, so `list-item`
//! and `List Item` both become `list item` and the resolver compares literally.
//! A role that is not one is an error at the point it was written — the same
//! trade `wait --title` makes by compiling its pattern when the wait is parked
//! rather than against every window.
//!
//! **An attribute may be constrained once per step.** `[name="a"][name="b"]` is
//! legal CSS meaning both must hold; a [`Predicate`] holds one matcher per
//! attribute, so honouring only the last would silently change what the caller
//! asked for. It is refused instead.

use waypuppet_proto::{Case, Combinator, MatchOp, Matcher, Predicate, Query, Step};

use crate::error::{ErrorKind, ParseError, unknown_role};
use crate::roles;

/// Parse a selector into a [`Query`].
///
/// The returned query has no [`Query::index`]; position is a caller's concern
/// (`--index N`), deliberately outside the grammar — CSS's `:nth-child()` is
/// one-based and counts siblings, and this is neither.
///
/// # Errors
///
/// [`ParseError`], which renders as a compiler-style diagnostic naming what was
/// found, where, and what to write instead.
pub fn parse(input: &str) -> Result<Query, ParseError> {
    Parser { input, at: 0 }.query()
}

/// Which attribute a `[…]` qualifier constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attr {
    Role,
    Name,
}

/// A cursor over the selector text.
struct Parser<'a> {
    input: &'a str,
    at: usize,
}

impl<'a> Parser<'a> {
    /// The whole selector.
    fn query(&mut self) -> Result<Query, ParseError> {
        if looks_like_xpath(self.input) {
            return Err(self.err(0..self.input.len(), ErrorKind::LooksLikeXPath));
        }

        self.skip_ws();
        let Some(lead) = self.peek() else {
            return Err(self.err(0..self.input.len(), ErrorKind::Empty));
        };
        match lead {
            '>' => {
                return Err(self.err(
                    self.at..self.at + 1,
                    ErrorKind::LeadingCombinator { got: lead },
                ));
            }
            '+' | '~' => {
                return Err(self.err(
                    self.at..self.at + 1,
                    ErrorKind::SiblingCombinator { got: lead },
                ));
            }
            ',' => return Err(self.err(self.at..self.at + 1, ErrorKind::SelectorList)),
            _ => {}
        }

        let first = self.step()?;
        let mut rest = Vec::new();
        loop {
            let spaced = self.skip_ws();
            let Some(next) = self.peek() else { break };
            let combinator = match next {
                '>' => {
                    let at = self.at;
                    self.bump();
                    self.skip_ws();
                    if self
                        .peek()
                        .is_none_or(|c| matches!(c, '>' | '+' | '~' | ','))
                    {
                        return Err(
                            self.err(at..at + 1, ErrorKind::DanglingCombinator { got: '>' })
                        );
                    }
                    Combinator::Child
                }
                '+' | '~' => {
                    return Err(self.err(
                        self.at..self.at + 1,
                        ErrorKind::SiblingCombinator { got: next },
                    ));
                }
                ',' => return Err(self.err(self.at..self.at + 1, ErrorKind::SelectorList)),
                _ if spaced => Combinator::Descendant,
                _ => {
                    return Err(self.err(
                        self.at..self.at + next.len_utf8(),
                        ErrorKind::Unexpected { got: next },
                    ));
                }
            };
            let predicate = self.step()?;
            rest.push(Step {
                combinator,
                predicate,
            });
        }

        Ok(Query {
            first,
            rest,
            index: None,
        })
    }

    /// One step: an optional type selector, then any number of qualifiers.
    fn step(&mut self) -> Result<Predicate, ParseError> {
        let start = self.at;
        let mut predicate = Predicate::default();
        let mut constrained = false;

        match self.peek() {
            Some(c) if is_ident_start(c) => {
                let (written, span) = self.ident();
                let Some(canonical) = roles::canonical(written) else {
                    return Err(self.not_a_role(written, &span));
                };
                predicate.role = Some(exact(canonical));
                constrained = true;
            }
            Some('*') => {
                self.bump();
                constrained = true;
            }
            _ => {}
        }

        while let Some(c) = self.peek() {
            match c {
                '[' => self.attribute(&mut predicate)?,
                ':' => self.pseudo(&mut predicate)?,
                '.' => {
                    let span = self.sigil_span();
                    let written = self.input[span.start + 1..span.end].to_owned();
                    return Err(self.err(span, ErrorKind::Class { written }));
                }
                '#' => {
                    let span = self.sigil_span();
                    let written = self.input[span.start + 1..span.end].to_owned();
                    return Err(self.err(span, ErrorKind::Id { written }));
                }
                _ => break,
            }
            constrained = true;
        }

        if constrained {
            Ok(predicate)
        } else {
            Err(self.err(start..self.at, ErrorKind::EmptyStep))
        }
    }

    /// A `[…]` qualifier.
    fn attribute(&mut self, predicate: &mut Predicate) -> Result<(), ParseError> {
        let open = self.at;
        self.bump();
        self.skip_ws();

        let Some(c) = self.peek() else {
            return Err(self.err(open..open + 1, ErrorKind::Unclosed { open: '[' }));
        };
        if c == ']' {
            self.bump();
            return Err(self.err(open..self.at, ErrorKind::EmptyStep));
        }
        if !is_ident_start(c) {
            return Err(self.err(
                self.at..self.at + c.len_utf8(),
                ErrorKind::Unexpected { got: c },
            ));
        }

        let (written, span) = self.ident();
        let attr = match written {
            "role" => Attr::Role,
            "name" => Attr::Name,
            _ => {
                return Err(self.err(
                    span,
                    ErrorKind::UnknownAttribute {
                        written: written.to_owned(),
                    },
                ));
            }
        };
        self.skip_ws();

        let matcher = if self.peek() == Some(']') {
            self.bump();
            Matcher::Present
        } else {
            let matcher = self.comparison()?;
            self.skip_ws();
            if self.peek() != Some(']') {
                return Err(self.err(open..open + 1, ErrorKind::Unclosed { open: '[' }));
            }
            self.bump();
            matcher
        };

        let slot = match attr {
            Attr::Role => &mut predicate.role,
            Attr::Name => &mut predicate.name,
        };
        if slot.is_some() {
            return Err(self.err(
                span,
                ErrorKind::RepeatedAttribute {
                    written: written.to_owned(),
                },
            ));
        }
        *slot = Some(match attr {
            Attr::Role => self.checked_role(matcher, open)?,
            Attr::Name => matcher,
        });
        Ok(())
    }

    /// The `op value flag?` tail of an attribute, with the span bookkeeping the
    /// value's own errors need.
    fn comparison(&mut self) -> Result<Matcher, ParseError> {
        let op = self.operator()?;
        self.skip_ws();
        let value = self.value()?;
        self.skip_ws();
        let case = self.case_flag()?;
        Ok(Matcher::Matches { op, value, case })
    }

    /// An attribute operator, or a diagnostic for one of CSS's that means
    /// something else here.
    fn operator(&mut self) -> Result<MatchOp, ParseError> {
        let start = self.at;
        let Some(c) = self.peek() else {
            return Err(self.err(start..start, ErrorKind::ExpectedValue));
        };
        let op = match c {
            '=' => {
                self.bump();
                // `=~` is nobody's CSS, but it is what somebody reaching for a
                // regular expression writes, so it is named rather than
                // reported as a stray tilde.
                if self.peek() == Some('~') {
                    self.bump();
                    return Err(self.err(
                        start..self.at,
                        ErrorKind::UnsupportedOperator {
                            written: "=~".to_owned(),
                        },
                    ));
                }
                MatchOp::Exact
            }
            '*' => self.paired(MatchOp::Contains)?,
            '^' => self.paired(MatchOp::Prefix)?,
            '$' => self.paired(MatchOp::Suffix)?,
            '~' | '|' | '!' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                }
                return Err(self.err(
                    start..self.at,
                    ErrorKind::UnsupportedOperator {
                        written: self.input[start..self.at].to_owned(),
                    },
                ));
            }
            _ => {
                return Err(self.err(
                    start..start + c.len_utf8(),
                    ErrorKind::Unexpected { got: c },
                ));
            }
        };
        Ok(op)
    }

    /// A two-character operator whose second character is `=`.
    fn paired(&mut self, op: MatchOp) -> Result<MatchOp, ParseError> {
        let start = self.at;
        self.bump();
        if self.peek() == Some('=') {
            self.bump();
            Ok(op)
        } else {
            Err(self.err(
                start..self.at,
                ErrorKind::UnsupportedOperator {
                    written: self.input[start..self.at].to_owned(),
                },
            ))
        }
    }

    /// A quoted or bare attribute value.
    fn value(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some(quote @ ('"' | '\'')) => {
                let open = self.at;
                self.bump();
                let mut out = String::new();
                loop {
                    let Some(c) = self.peek() else {
                        return Err(self.err(
                            open..self.input.len(),
                            ErrorKind::UnterminatedString { quote },
                        ));
                    };
                    self.bump();
                    if c == quote {
                        return Ok(out);
                    }
                    if c == '\\' {
                        // A backslash escapes whatever follows it, which is how
                        // a name containing a quote is written. A trailing one
                        // is an unterminated string, not a literal backslash.
                        let Some(escaped) = self.peek() else {
                            return Err(self.err(
                                open..self.input.len(),
                                ErrorKind::UnterminatedString { quote },
                            ));
                        };
                        self.bump();
                        out.push(escaped);
                    } else {
                        out.push(c);
                    }
                }
            }
            Some(c) if is_value_char(c) => {
                let start = self.at;
                while self.peek().is_some_and(is_value_char) {
                    self.bump();
                }
                Ok(self.input[start..self.at].to_owned())
            }
            _ => Err(self.err(self.at..self.at, ErrorKind::ExpectedValue)),
        }
    }

    /// The optional `i` flag after a value.
    fn case_flag(&mut self) -> Result<Case, ParseError> {
        match self.peek() {
            Some('i' | 'I') => {
                let start = self.at;
                let (written, span) = self.ident();
                if written.len() == 1 {
                    Ok(Case::Insensitive)
                } else {
                    self.at = start;
                    Err(self.err(
                        span,
                        ErrorKind::BadCaseFlag {
                            written: written.to_owned(),
                        },
                    ))
                }
            }
            Some(c) if is_ident_start(c) => {
                let (written, span) = self.ident();
                Err(self.err(
                    span,
                    ErrorKind::BadCaseFlag {
                        written: written.to_owned(),
                    },
                ))
            }
            _ => Ok(Case::Sensitive),
        }
    }

    /// A `:…` qualifier: `:not(…)`, or a diagnostic for anything else.
    fn pseudo(&mut self, predicate: &mut Predicate) -> Result<(), ParseError> {
        let start = self.at;
        self.bump();

        if self.peek() == Some(':') {
            self.bump();
            let written = if self.peek().is_some_and(is_ident_start) {
                self.ident().0.to_owned()
            } else {
                String::new()
            };
            return Err(self.err(start..self.at, ErrorKind::PseudoElement { written }));
        }

        let Some(c) = self.peek() else {
            return Err(self.err(
                start..self.at,
                ErrorKind::PseudoClass {
                    written: String::new(),
                },
            ));
        };
        if !is_ident_start(c) {
            return Err(self.err(
                self.at..self.at + c.len_utf8(),
                ErrorKind::Unexpected { got: c },
            ));
        }
        let (written, _) = self.ident();

        if written != "not" {
            // Swallow any argument so the caret covers `:nth-child(2)` rather
            // than stopping at the name — the argument is part of what the
            // caller wrote and part of what they have to remove.
            self.skip_balanced();
            return Err(self.err(
                start..self.at,
                ErrorKind::PseudoClass {
                    written: written.to_owned(),
                },
            ));
        }

        let open = self.at;
        if self.peek() != Some('(') {
            return Err(self.err(start..self.at, ErrorKind::Unclosed { open: '(' }));
        }
        self.bump();
        self.skip_ws();
        let inner = self.step()?;
        self.skip_ws();
        if self.peek() != Some(')') {
            return Err(self.err(open..open + 1, ErrorKind::Unclosed { open: '(' }));
        }
        self.bump();
        predicate.not.push(inner);
        Ok(())
    }

    /// Check a `[role…]` matcher against the closed AT-SPI vocabulary, and
    /// canonicalise an exact one.
    ///
    /// An exact role is stored the way `atspi` spells it, so the resolver never
    /// normalises. A partial one cannot be canonicalised, so it is checked for
    /// being satisfiable at all — `[role*="Button"]` can match nothing, roles
    /// being lower case, and saying so here beats an empty result later.
    fn checked_role(&self, matcher: Matcher, open: usize) -> Result<Matcher, ParseError> {
        let Matcher::Matches { op, value, case } = matcher else {
            return Ok(Matcher::Present);
        };
        if op == MatchOp::Exact {
            let Some(canonical) = roles::canonical(&value) else {
                return Err(self.err(open..self.at, unknown_role(&value)));
            };
            return Ok(Matcher::Matches {
                op,
                value: canonical.to_owned(),
                case,
            });
        }

        let matcher = Matcher::Matches {
            op,
            value: value.clone(),
            case,
        };
        if roles::satisfiable(&matcher) {
            Ok(matcher)
        } else {
            Err(self.err(open..self.at, ErrorKind::UnsatisfiableRole { op, value }))
        }
    }

    /// The diagnostic for a type selector that is not a role.
    ///
    /// Before giving up and suggesting near-misses, check whether the caller
    /// wrote a real role *with its spaces* — `list item`, `page tab list`. That
    /// is the one place CSS's syntax and AT-SPI's vocabulary genuinely collide,
    /// so it is the mistake most worth catching: reported as an unknown role,
    /// `list item` complains about `item` and offers `icon`, which points away
    /// from the answer the caller already has in their hand.
    fn not_a_role(&self, written: &str, span: &std::ops::Range<usize>) -> ParseError {
        match self.spaced_role(span) {
            Some((span, role)) => self.err(
                span.clone(),
                ErrorKind::SpacedRole {
                    written: self.input[span].to_owned(),
                    role,
                },
            ),
            None => self.err(span.clone(), unknown_role(written)),
        }
    }

    /// The longest run of space-separated words around `span` that spells a
    /// role, if there is one.
    ///
    /// Longest first, because `page tab list` contains `page tab`, and
    /// suggesting the shorter one would send a caller to a selector that parses
    /// and matches the wrong thing — worse than the error they started with.
    fn spaced_role(
        &self,
        span: &std::ops::Range<usize>,
    ) -> Option<(std::ops::Range<usize>, &'static str)> {
        // Words separated by nothing but whitespace form a run; any other
        // character ends one, because `button[x] item` is not a role written
        // with spaces, it is a selector with a mistake in it.
        let mut runs: Vec<Vec<std::ops::Range<usize>>> = Vec::new();
        let mut run: Vec<std::ops::Range<usize>> = Vec::new();
        let mut word: Option<usize> = None;
        for (at, c) in self.input.char_indices() {
            if is_ident_char(c) {
                word.get_or_insert(at);
            } else {
                if let Some(start) = word.take() {
                    run.push(start..at);
                }
                if !c.is_whitespace() {
                    runs.push(std::mem::take(&mut run));
                }
            }
        }
        if let Some(start) = word {
            run.push(start..self.input.len());
        }
        runs.push(run);

        let run = runs.into_iter().find(|run| run.contains(span))?;
        let here = run.iter().position(|word| word == span)?;
        for length in (2..=run.len()).rev() {
            for start in 0..=run.len() - length {
                let end = start + length;
                if start > here || end <= here {
                    continue;
                }
                let words: Vec<&str> = run[start..end]
                    .iter()
                    .map(|word| &self.input[word.clone()])
                    .collect();
                if let Some(role) = roles::canonical(&words.join(" ")) {
                    return Some((run[start].start..run[end - 1].end, role));
                }
            }
        }
        None
    }

    // --- cursor ---------------------------------------------------------

    fn peek(&self) -> Option<char> {
        self.input[self.at..].chars().next()
    }

    fn bump(&mut self) {
        if let Some(c) = self.peek() {
            self.at += c.len_utf8();
        }
    }

    /// Consume whitespace; report whether there was any, because in CSS that is
    /// itself a combinator.
    fn skip_ws(&mut self) -> bool {
        let start = self.at;
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
        self.at != start
    }

    /// An identifier, and the span it occupied. Only call with an identifier
    /// start under the cursor.
    fn ident(&mut self) -> (&'a str, std::ops::Range<usize>) {
        let start = self.at;
        while self.peek().is_some_and(is_ident_char) {
            self.bump();
        }
        (&self.input[start..self.at], start..self.at)
    }

    /// The span of a `.foo` or `#foo` sigil and the identifier after it.
    fn sigil_span(&mut self) -> std::ops::Range<usize> {
        let start = self.at;
        self.bump();
        while self.peek().is_some_and(is_ident_char) {
            self.bump();
        }
        start..self.at
    }

    /// Skip a balanced `(…)` argument, if one is there.
    fn skip_balanced(&mut self) {
        if self.peek() != Some('(') {
            return;
        }
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            self.bump();
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    fn err(&self, span: std::ops::Range<usize>, kind: ErrorKind) -> ParseError {
        ParseError {
            input: self.input.to_owned(),
            span,
            kind,
        }
    }
}

/// An exact, case-sensitive match — what a type selector compiles to.
fn exact(value: &str) -> Matcher {
    Matcher::Matches {
        op: MatchOp::Exact,
        value: value.to_owned(),
        case: Case::Sensitive,
    }
}

/// Whether this reads as `XPath`.
///
/// Worth detecting rather than letting fail as a stray `/`: `XPath` is what
/// accessibility automation uses nearly everywhere else — Selenium, Appium — so
/// it is the wrong dialect somebody is most likely to arrive with, and a
/// translation costs one arm. `[@` is the distinctive part; a bare `@` is not,
/// because an accessible name may contain one.
fn looks_like_xpath(input: &str) -> bool {
    let trimmed = input.trim_start();
    trimmed.starts_with('/') || trimmed.starts_with("./") || input.contains("[@")
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn is_value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}
