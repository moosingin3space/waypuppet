//! Tests for the fingerprint format.
//!
//! The properties worth checking here are the ones a consumer's whole golden
//! suite rests on: that a recorded line reads back as the thing that was
//! recorded, that two machines agreeing on the fields agree on the digest, and
//! that a pinned expectation cannot be satisfied by an unpinned observation.

use arbtest::arbtest;

use super::{Epoch, Expectation, Fingerprint, Pin, Renderer, Unpinned, Verdict};

/// A pinned fingerprint, as an ordinary flatpak-hosted session produces.
fn pinned() -> Fingerprint {
    Fingerprint {
        epoch: Epoch(1),
        arch: "x86_64".to_owned(),
        renderer: Renderer::Software,
        pin: Pin::Runtime {
            id: "org.freedesktop.Platform".to_owned(),
            branch: "25.08".to_owned(),
            commit: "3f2a1c9d4e5b6a7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c".to_owned(),
        },
    }
}

/// The same session with nothing behind it.
fn unpinned() -> Fingerprint {
    Fingerprint {
        pin: Pin::Nothing(Unpinned::Host),
        ..pinned()
    }
}

#[test]
fn a_recorded_line_reads_back_as_what_was_recorded() {
    let print = pinned();
    assert_eq!(Fingerprint::parse(&print.canonical()), Ok(print));
}

#[test]
fn the_canonical_form_is_one_line_with_the_fields_in_order() {
    let canonical = pinned().canonical();
    assert!(!canonical.contains(char::is_whitespace), "{canonical}");
    let fields: Vec<_> = canonical.split('|').collect();
    assert_eq!(fields[0], "wp-render/1");
    assert!(fields[1].starts_with("epoch="));
    assert!(fields[2].starts_with("arch="));
    assert!(fields[3].starts_with("renderer="));
    assert!(fields[4].starts_with("runtime="));
}

/// The whole point of the digest: same inputs, same label, on any machine.
#[test]
fn the_digest_follows_from_the_fields_alone() {
    assert_eq!(pinned().digest(), pinned().digest());
    let elsewhere = Fingerprint {
        renderer: Renderer::Hardware,
        ..pinned()
    };
    assert_ne!(pinned().digest(), elsewhere.digest());
}

/// The structural half of the design: a pinned expectation cannot be met by an
/// unpinned observation, because the strings cannot collide.
#[test]
fn pinned_and_unpinned_digests_cannot_be_confused() {
    assert!(pinned().digest().starts_with("wp1:"));
    assert!(unpinned().digest().starts_with("wp1u:"));
    assert_ne!(pinned().digest(), unpinned().digest());
}

#[test]
fn a_matching_expectation_is_a_match_in_either_form() {
    let print = pinned();
    for form in [print.digest(), print.canonical()] {
        let expectation = Expectation::parse(&form).expect("parses");
        assert_eq!(expectation.check(&print), Verdict::Match, "{form}");
    }
}

/// What the canonical form buys over the digest, and the reason to record it.
#[test]
fn a_recorded_line_says_which_field_moved_and_a_digest_cannot() {
    let expected = pinned();
    let observed = Fingerprint {
        epoch: Epoch(2),
        ..pinned()
    };

    let explained = Expectation::parse(&expected.canonical())
        .expect("parses")
        .check(&observed);
    let Verdict::Explained(differences) = explained else {
        panic!("a canonical expectation should explain: {explained:?}");
    };
    assert_eq!(differences.len(), 1);
    assert_eq!(differences[0].field, "epoch");
    assert_eq!(differences[0].expected, "1");
    assert_eq!(differences[0].observed, "2");

    let opaque = Expectation::parse(&expected.digest())
        .expect("parses")
        .check(&observed);
    assert!(matches!(opaque, Verdict::Opaque { .. }), "{opaque:?}");
}

#[test]
fn a_runtime_that_moved_commit_names_both() {
    let expected = pinned();
    let mut observed = pinned();
    if let Pin::Runtime { commit, .. } = &mut observed.pin {
        "0000000000000000000000000000000000000000000000000000000000000000".clone_into(commit);
    }

    let differences = observed.differences(&expected);
    assert_eq!(differences.len(), 1);
    assert_eq!(differences[0].field, "runtime");
    assert!(differences[0].expected.contains("3f2a1c9d"));
    assert!(differences[0].observed.contains("00000000"));
}

/// Every rejection has to say what to write instead — the same rule the selector
/// grammar holds itself to, and for the same reason: whoever reads this cannot
/// ask a follow-up question.
#[test]
fn every_rejection_names_the_shape_it_wanted() {
    for bad in [
        "",
        "nonsense",
        "wp-render/99|epoch=1|arch=x86_64|renderer=software|runtime=none",
        "wp-render/1|epoch=x|arch=x86_64|renderer=software|runtime=none",
        "wp-render/1|epoch=1|arch=x86_64|renderer=magic|runtime=none",
        "wp-render/1|epoch=1|arch=x86_64|renderer=software|runtime=org.x//1",
        "wp1:zzzzzzzzzzzz",
        "wp1:abc",
    ] {
        let error = Expectation::parse(bad).expect_err(bad);
        // Every rejection quotes something concrete — the shape that was wanted,
        // or the command that writes one. Asserted on the backticks rather than
        // on a list of words, because a keyword list is a test of this test's
        // vocabulary: the first version accepted three phrasings and failed a
        // message that named the exact command to run.
        assert!(
            error.contains('`'),
            "`{bad}` was rejected without quoting anything to write instead: {error}"
        );
    }
}

#[test]
fn every_unpinned_reason_says_what_to_do_about_it() {
    for reason in [
        Unpinned::Host,
        Unpinned::Image("ubuntu:24.04".to_owned()),
        Unpinned::UnknownCommit {
            runtime: "org.freedesktop.Platform//25.08".to_owned(),
        },
    ] {
        assert!(!reason.fix().is_empty(), "{reason} has no fix");
    }
}

/// Generated over the real vocabulary, so the property cannot pass vacuously:
/// anything this crate can write, it can read back unchanged.
#[test]
fn recording_and_reading_back_is_a_round_trip() {
    arbtest(|u| {
        let print = Fingerprint {
            epoch: Epoch(u.arbitrary()?),
            arch: (*u.choose(&["x86_64", "aarch64", "riscv64"])?).to_owned(),
            renderer: *u.choose(&[Renderer::Software, Renderer::Hardware])?,
            pin: if u.arbitrary()? {
                Pin::Runtime {
                    id: (*u.choose(&["org.freedesktop.Platform", "org.freedesktop.Sdk"])?)
                        .to_owned(),
                    branch: (*u.choose(&["25.08", "24.08"])?).to_owned(),
                    commit: (*u.choose(&["3f2a1c9d", "0badc0de"])?).to_owned(),
                }
            } else {
                Pin::Nothing(Unpinned::Host)
            },
        };

        let read = Fingerprint::parse(&print.canonical()).expect("a written line parses");
        assert_eq!(read, print);
        assert_eq!(read.digest(), print.digest());
        Ok(())
    });
}
