//! The render fingerprint: what a capture's pixels depend on, stated so that two
//! machines can compare it.
//!
//! waypuppet composites the frame, so waypuppet is an input to every golden image
//! downstream of it — and so is the runtime, which supplies mesa and the fonts the
//! application draws with. Upgrade any of them and a consumer's whole golden suite
//! starts failing at once, with nothing to say why. That is the failure this exists
//! to convert into a sentence.
//!
//! No I/O. Everything arrives as data, the same split [`crate`-adjacent]
//! `waypuppet-sandbox` draws between `resolve` and `detect`, and for the same
//! reason: the interesting cases are a runtime whose commit could not be read and
//! a session with nothing to pin to, and both should be three lines of test rather
//! than a machine nobody will arrange.
//!
//! # The format
//!
//! ```text
//! wp-render/1|epoch=1|arch=x86_64|renderer=software|runtime=org.freedesktop.Platform//25.08@3f2a1c9d…
//! ```
//!
//! One line, fields in a fixed order, no whitespace. That string is the artifact:
//! it is what a consumer checks into their repository, and it is what gets hashed.
//! The digest — `wp1:4f3a9c2e1b70` — is a label for it, short enough to paste into
//! a CI variable or read out over a call.
//!
//! **The canonical form is the thing to record, not the digest.** A digest can say
//! that something changed and can never say what, because it cannot be reversed.
//! Recording the full line is what lets [`Fingerprint::differences`] answer "the
//! runtime moved from commit a to commit b" instead of "these twelve characters
//! are not those twelve characters". `--expect` therefore takes either, and is
//! more useful when given the line.
//!
//! # Two things the format states rather than implies
//!
//! **The epoch is bumped by hand, and is not waypuppet's version.** A version
//! number answers "is this the same build", which is a different question from "does
//! this draw the same pixels" and answers it far too often: every patch release would
//! invalidate every golden everywhere, and a suite people re-bless reflexively is
//! worse than no suite. [`Epoch`] carries the rule for when to bump it. The cost is
//! real and is the reason it is documented so heavily — forgetting to bump it lets
//! pixels drift silently, which is the exact failure the fingerprint exists to catch.
//!
//! **Unpinned gets its own digest prefix**, `wp1u:` rather than `wp1:`. A session
//! running straight on the host, or in a container named by a mutable tag, has
//! nothing content-addressed behind it — its fonts and its mesa can change under it
//! with the fingerprint unmoved. Making that a different *string* means a pinned
//! expectation cannot accidentally match an unpinned observation, so no comparison
//! has to remember to check. It is the same move as splitting `wait` conditions so
//! neither half is handed one it cannot see: structure, rather than a rule somebody
//! has to follow.

use std::fmt;
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// Version of the fingerprint format itself, leading every canonical form.
///
/// Bumped when the *set or order of fields* changes, which is a different event
/// from [`Epoch`] and has to stay that way: this says "you are reading a format
/// you may not understand", that one says "the pixels moved".
pub const FORMAT: u32 = 1;

/// Prefix on a digest whose fingerprint is pinned to a content-addressed runtime.
pub const PINNED_PREFIX: &str = "wp1";

/// Prefix on a digest whose fingerprint is not pinned to anything.
pub const UNPINNED_PREFIX: &str = "wp1u";

/// Hex characters kept from the SHA-256 of the canonical form.
///
/// Twelve is short enough to read aloud and long enough that a collision is not
/// a thing to plan around: this labels a handful of configurations, not a
/// content-addressed store, and the canonical form is always there to settle an
/// argument.
const DIGEST_LENGTH: usize = 12;

/// How waypuppet's own rendering is versioned, for pixel purposes only.
///
/// # When to bump this
///
/// Bump it when a capture of an unchanged application could come out different:
/// the offscreen GLES path, the readback, the PNG encode, output construction,
/// the default scale or format, or the choice of rasteriser for a given request.
///
/// Do **not** bump it for anything that cannot reach a pixel — a new verb, a
/// protocol field, a CLI message, a fix to `wait`. Those are what the crate
/// version is for.
///
/// # Why this is not the crate version
///
/// Deriving it from the version would invalidate every consumer's goldens on
/// every patch release. The predictable result is people re-blessing without
/// looking, and a suite that gets re-blessed reflexively has stopped being a
/// test. The trade is that this one is a promise a human has to keep; that is
/// why it is a single documented constant rather than something clever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Epoch(pub u32);

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Which class of rasteriser produced the frame.
///
/// Only the class, not the device: llvmpipe and a GPU antialias differently and
/// that is the difference that reaches a golden, whereas two GPUs of the same
/// generation are not a distinction this can meaningfully police. The runtime
/// commit is what pins the actual mesa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    /// llvmpipe, or whatever software rasteriser the runtime supplies.
    Software,
    /// A GPU.
    Hardware,
}

impl Renderer {
    /// The word used in the canonical form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Hardware => "hardware",
        }
    }
}

impl fmt::Display for Renderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a fingerprint has nothing content-addressed behind it.
///
/// Kept as reasons rather than a bare `None`, because the fixes differ and a
/// consumer reading this is deciding what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unpinned {
    /// The session runs directly on this machine, against whatever it has.
    Host,
    /// A container image, named by a reference that may move under it.
    ///
    /// A tag is not a pin. A digest reference is, but waypuppet does not resolve
    /// one on the caller's behalf — inventing a pin the caller did not ask for
    /// would be claiming a guarantee nobody made.
    Image(String),
    /// A flatpak runtime whose commit could not be read.
    ///
    /// Distinct from [`Host`](Self::Host) on purpose: this one is pinnable and
    /// merely was not, so the fix is to find out why `flatpak info` failed
    /// rather than to change how the session runs.
    UnknownCommit {
        /// The runtime that was in use, as `id//branch`.
        runtime: String,
    },
}

impl Unpinned {
    /// What to do about it, in one line.
    #[must_use]
    pub fn fix(&self) -> String {
        match self {
            Self::Host => "run the session in a flatpak runtime — `--sandbox flatpak` — \
                 so there is a commit to pin to"
                .to_owned(),
            Self::Image(reference) => format!(
                "`{reference}` is a tag, which can move. Name an image by digest, or use \
                 `--sandbox flatpak`, whose runtimes are content-addressed."
            ),
            Self::UnknownCommit { runtime } => format!(
                "`flatpak info --show-commit {runtime}` did not answer. The runtime is \
                 pinnable; something stopped it being read."
            ),
        }
    }
}

impl fmt::Display for Unpinned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host => f.write_str("none (host)"),
            Self::Image(reference) => write!(f, "none (image {reference})"),
            Self::UnknownCommit { runtime } => write!(f, "none ({runtime}, commit unknown)"),
        }
    }
}

/// What the frame's pixels are pinned to, when anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pin {
    /// A flatpak runtime, identified by the commit it is deployed at.
    ///
    /// This is the whole reason to prefer flatpak for anything comparing
    /// pixels: mesa and the font set both come out of the runtime, so its
    /// `OSTree` commit already moves when either of them does. Nothing else has
    /// to be measured.
    Runtime {
        /// Application id, such as `org.freedesktop.Platform`.
        id: String,
        /// Branch, such as `25.08`.
        branch: String,
        /// The `OSTree` commit the runtime is deployed at.
        commit: String,
    },
    /// Nothing to pin to, and why.
    Nothing(Unpinned),
}

impl Pin {
    /// The `runtime=` field of the canonical form.
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            Self::Runtime { id, branch, commit } => format!("{id}//{branch}@{commit}"),
            Self::Nothing(_) => "none".to_owned(),
        }
    }
}

impl fmt::Display for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime { id, branch, commit } => write!(f, "{id}//{branch}@{commit}"),
            Self::Nothing(why) => why.fmt(f),
        }
    }
}

/// Everything a capture's pixels depend on that waypuppet can state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// waypuppet's render epoch.
    pub epoch: Epoch,
    /// Machine architecture, since mesa is built per architecture.
    pub arch: String,
    /// Which class of rasteriser.
    pub renderer: Renderer,
    /// What that rasteriser and its fonts come from.
    pub pin: Pin,
}

impl Fingerprint {
    /// Whether this is pinned to something content-addressed.
    #[must_use]
    pub fn pinned(&self) -> bool {
        matches!(self.pin, Pin::Runtime { .. })
    }

    /// The one-line canonical form. This is the thing to record.
    #[must_use]
    pub fn canonical(&self) -> String {
        format!(
            "wp-render/{FORMAT}|epoch={}|arch={}|renderer={}|runtime={}",
            self.epoch,
            self.arch,
            self.renderer.as_str(),
            self.pin.canonical(),
        )
    }

    /// The short label for the canonical form.
    ///
    /// The prefix carries whether this is pinned, so a pinned expectation and an
    /// unpinned observation can never compare equal by accident.
    #[must_use]
    pub fn digest(&self) -> String {
        let hash = Sha256::digest(self.canonical().as_bytes());
        let hex = hash.iter().take(DIGEST_LENGTH.div_ceil(2)).fold(
            String::with_capacity(DIGEST_LENGTH),
            |mut hex, byte| {
                // Infallible into a `String`; the result is the accumulator.
                let _ = write!(hex, "{byte:02x}");
                hex
            },
        );
        let prefix = if self.pinned() {
            PINNED_PREFIX
        } else {
            UNPINNED_PREFIX
        };
        format!("{prefix}:{}", &hex[..DIGEST_LENGTH])
    }

    /// Read a canonical form back.
    ///
    /// Generous about field *order* it is not: the order is part of the format,
    /// because the digest is taken over the string and two orderings would be
    /// two digests for one machine.
    ///
    /// # Errors
    ///
    /// Returns a sentence naming what is wrong and what was expected.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let mut fields = text.split('|');

        let version = fields
            .next()
            .and_then(|head| head.strip_prefix("wp-render/"))
            .ok_or_else(|| {
                format!(
                    "`{text}` is not a render fingerprint; it should start `wp-render/{FORMAT}|`"
                )
            })?;
        if version != FORMAT.to_string() {
            return Err(format!(
                "this is fingerprint format {version}, and this waypuppet writes {FORMAT}. \
                 Re-record it with `waypuppet fingerprint --record`."
            ));
        }

        let mut epoch = None;
        let mut arch = None;
        let mut renderer = None;
        let mut pin = None;

        for field in fields {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| format!("`{field}` is not a `key=value` field"))?;
            match key {
                "epoch" => {
                    epoch =
                        Some(Epoch(value.parse().map_err(|_| {
                            format!("epoch `{value}` is not a whole number")
                        })?));
                }
                "arch" => arch = Some(value.to_owned()),
                "renderer" => {
                    renderer = Some(match value {
                        "software" => Renderer::Software,
                        "hardware" => Renderer::Hardware,
                        other => {
                            return Err(format!(
                                "renderer `{other}` is neither `software` nor `hardware`"
                            ));
                        }
                    });
                }
                "runtime" => pin = Some(parse_pin(value)?),
                other => {
                    return Err(format!(
                        "`{other}` is not a field of fingerprint format {FORMAT}"
                    ));
                }
            }
        }

        Ok(Self {
            epoch: epoch.ok_or("no `epoch` field")?,
            arch: arch.ok_or("no `arch` field")?,
            renderer: renderer.ok_or("no `renderer` field")?,
            pin: pin.ok_or("no `runtime` field")?,
        })
    }

    /// Every field that differs, most significant first.
    ///
    /// Empty means the two are the same fingerprint. This is what a digest
    /// cannot do, and the reason the canonical form is what gets recorded.
    #[must_use]
    pub fn differences(&self, expected: &Self) -> Vec<Difference> {
        let mut found = Vec::new();
        let mut note = |field: &'static str, expected: String, observed: String| {
            if expected != observed {
                found.push(Difference {
                    field,
                    expected,
                    observed,
                });
            }
        };

        note("epoch", expected.epoch.to_string(), self.epoch.to_string());
        note("arch", expected.arch.clone(), self.arch.clone());
        note(
            "renderer",
            expected.renderer.to_string(),
            self.renderer.to_string(),
        );
        note("runtime", expected.pin.to_string(), self.pin.to_string());
        found
    }
}

/// Reads the `runtime=` field.
fn parse_pin(value: &str) -> Result<Pin, String> {
    if value == "none" {
        // A recorded `none` says only that whoever recorded it had nothing to
        // pin to; why is not in the line, and inventing a reason on the way back
        // in would be stating something the format never carried.
        return Ok(Pin::Nothing(Unpinned::Host));
    }
    let (runtime, commit) = value.split_once('@').ok_or_else(|| {
        format!("runtime `{value}` has no `@commit`; it should read `id//branch@commit`")
    })?;
    let (id, branch) = runtime
        .split_once("//")
        .ok_or_else(|| format!("runtime `{runtime}` is not `id//branch`"))?;
    Ok(Pin::Runtime {
        id: id.to_owned(),
        branch: branch.to_owned(),
        commit: commit.to_owned(),
    })
}

/// One field that came out other than expected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    /// Which field.
    pub field: &'static str,
    /// What was recorded.
    pub expected: String,
    /// What this machine has.
    pub observed: String,
}

impl fmt::Display for Difference {
    /// One line, because the caller decides the indentation.
    ///
    /// A two-line form was tried and dropped: whatever prefix the caller puts on
    /// the first line does not reach the second, so it arrives misaligned in
    /// exactly the output somebody is reading to find out what broke.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: expected {}, observed {}",
            self.field, self.expected, self.observed
        )
    }
}

/// What a caller pinned, which is either the whole line or just its label.
///
/// Both are accepted because both get used: a digest is what fits in a CI
/// variable, and the canonical form is what fits in a repository. Only the
/// second can explain a mismatch, and [`Verdict`] says so rather than leaving
/// somebody wondering why they got less than the documentation promised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// A digest, such as `wp1:4f3a9c2e1b70`.
    Digest(String),
    /// A full canonical form.
    Full(Box<Fingerprint>),
}

impl Expectation {
    /// Read whichever form was given.
    ///
    /// # Errors
    ///
    /// Returns a sentence naming what is wrong, from whichever form it looks
    /// most like.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.starts_with("wp-render/") {
            return Fingerprint::parse(text).map(|print| Self::Full(Box::new(print)));
        }
        if let Some((prefix, rest)) = text.split_once(':')
            && (prefix == PINNED_PREFIX || prefix == UNPINNED_PREFIX)
            && rest.len() == DIGEST_LENGTH
            && rest.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Ok(Self::Digest(text.to_owned()));
        }
        Err(format!(
            "`{text}` is neither a digest nor a canonical form. A digest reads \
             `{PINNED_PREFIX}:` or `{UNPINNED_PREFIX}:` and {DIGEST_LENGTH} hex \
             characters; a canonical form starts `wp-render/{FORMAT}|`. \
             `waypuppet fingerprint --record` writes the second."
        ))
    }

    /// Check an observed fingerprint against this.
    #[must_use]
    pub fn check(&self, observed: &Fingerprint) -> Verdict {
        match self {
            Self::Digest(digest) if *digest == observed.digest() => Verdict::Match,
            Self::Digest(digest) => Verdict::Opaque {
                expected: digest.clone(),
                observed: observed.digest(),
            },
            Self::Full(expected) if **expected == *observed => Verdict::Match,
            Self::Full(expected) => Verdict::Explained(observed.differences(expected)),
        }
    }
}

/// The result of checking an observation against an expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// They agree.
    Match,
    /// They disagree, and the expectation was a digest, so that is all anyone
    /// can say about it.
    Opaque {
        /// What was pinned.
        expected: String,
        /// What this machine produced.
        observed: String,
    },
    /// They disagree, field by field.
    Explained(Vec<Difference>),
}

impl Verdict {
    /// Whether the observation matched.
    #[must_use]
    pub fn matched(&self) -> bool {
        matches!(self, Self::Match)
    }
}

#[cfg(test)]
mod tests;
