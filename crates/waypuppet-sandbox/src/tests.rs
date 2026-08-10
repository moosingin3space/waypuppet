//! Tests for the decision, which is the part worth testing.
//!
//! Every case here is one that would otherwise need a machine arranged for it: a
//! host with no flatpak, a container with no render node, a runtime installed at
//! the wrong branch. That is the return on keeping [`resolve`] pure — none of
//! them needs a container to check, so all of them get checked.

use std::path::{Path, PathBuf};

use super::{
    Direct, Enclosure, Engine, Facts, FlatpakRuntime, Obstacle, Plan, Reenter, Request, Sandbox,
    mounts::tidy, resolve, writable,
};

/// A request that names four distinct, plausible directories.
fn request(sandbox: Sandbox) -> Request {
    Request {
        sandbox,
        exe: PathBuf::from("/usr/local/bin/waypuppet"),
        command: Some(PathBuf::from("/var/home/you/project/target/debug/app")),
        cwd: PathBuf::from("/var/home/you/project"),
        capture_dir: PathBuf::from("/tmp"),
        socket_dir: PathBuf::from("/run/user/1000/waypuppet"),
        extra: Vec::new(),
        runtime: None,
        image: None,
    }
}

/// A machine with flatpak and the preferred runtime, but no way to render here.
fn needs_a_sandbox() -> Facts {
    Facts {
        enclosure: Enclosure::Host,
        renders: false,
        render_node: true,
        flatpak: Some(vec![FlatpakRuntime::parse(
            "org.freedesktop.Platform//25.08",
        )]),
        engines: vec![Engine::Podman],
    }
}

fn reentered(plan: &Plan) -> &Reenter {
    match plan {
        Plan::Reenter(reenter) => reenter,
        Plan::Direct(direct) => panic!("expected a sandbox, got {direct:?}"),
    }
}

#[test]
fn a_machine_that_can_render_stays_put() {
    let facts = Facts {
        renders: true,
        ..needs_a_sandbox()
    };
    assert_eq!(
        resolve(&request(Sandbox::Auto), &facts),
        Ok(Plan::Direct(Direct::Capable))
    );
}

#[test]
fn none_stays_put_even_where_a_sandbox_would_help() {
    assert_eq!(
        resolve(&request(Sandbox::None), &needs_a_sandbox()),
        Ok(Plan::Direct(Direct::Asked))
    );
}

#[test]
fn flatpak_is_used_even_where_this_process_could_manage() {
    // The point of asking for it explicitly: an application built against the
    // SDK needs the SDK whether or not waypuppet itself does, and waypuppet
    // cannot see that from here.
    let facts = Facts {
        renders: true,
        ..needs_a_sandbox()
    };
    let plan = resolve(&request(Sandbox::Flatpak), &facts).expect("flatpak is installed");
    assert_eq!(
        reentered(&plan).runtime,
        super::Runtime::Flatpak(FlatpakRuntime::parse("org.freedesktop.Platform//25.08"))
    );
}

#[test]
fn the_platform_runtime_is_preferred_over_the_sdk() {
    // Both installed, and the lighter one wins: the Platform carries everything
    // needed to *run* an application, and is already present on any machine with
    // a graphical flatpak on it.
    let facts = Facts {
        flatpak: Some(vec![
            FlatpakRuntime::parse("org.freedesktop.Sdk//25.08"),
            FlatpakRuntime::parse("org.freedesktop.Platform//25.08"),
        ]),
        ..needs_a_sandbox()
    };
    let plan = resolve(&request(Sandbox::Auto), &facts).expect("a runtime is installed");
    assert_eq!(
        reentered(&plan).runtime,
        super::Runtime::Flatpak(FlatpakRuntime::parse("org.freedesktop.Platform//25.08"))
    );
}

#[test]
fn a_named_runtime_is_never_quietly_substituted() {
    // A `waypuppet.toml` naming a runtime is a statement that the application
    // needs *that* one. Falling back to another because it happens to be
    // installed would answer a question nobody asked, and would do it silently.
    let mut request = request(Sandbox::Auto);
    request.runtime = Some(FlatpakRuntime::parse("org.example.Custom//1"));

    assert_eq!(
        resolve(&request, &needs_a_sandbox()),
        Err(Obstacle::NoRuntime {
            wanted: FlatpakRuntime::parse("org.example.Custom//1"),
        })
    );
}

#[test]
fn a_runtime_at_the_wrong_branch_is_not_the_runtime() {
    let facts = Facts {
        flatpak: Some(vec![FlatpakRuntime::parse(
            "org.freedesktop.Platform//24.08",
        )]),
        ..needs_a_sandbox()
    };
    let Err(obstacle) = resolve(&request(Sandbox::Auto), &facts) else {
        panic!("24.08 is not the branch waypuppet asks for");
    };
    assert!(matches!(obstacle, Obstacle::NoRuntime { .. }));
}

#[test]
fn a_container_with_nothing_to_render_on_is_a_dead_end() {
    // The case worth an explicit error rather than a confusing one: none of the
    // remaining options can be reached from in here, so "try harder" is wrong
    // and every fix is about the container the caller already started.
    for enclosure in [
        Enclosure::Flatpak,
        Enclosure::Container(Engine::Podman),
        Enclosure::Container(Engine::Docker),
    ] {
        let facts = Facts {
            enclosure,
            ..needs_a_sandbox()
        };
        assert_eq!(
            resolve(&request(Sandbox::Auto), &facts),
            Err(Obstacle::Nested { enclosure }),
            "{enclosure} should be reported as nested"
        );
    }
}

#[test]
fn a_missing_runtime_is_not_laundered_into_a_container_image() {
    // Falling through to podman here would be wrong twice over: the fix is one
    // `waypuppet setup` away, and the image it would reach for carries libraries
    // the caller has never heard of.
    let facts = Facts {
        flatpak: Some(Vec::new()),
        ..needs_a_sandbox()
    };
    let mut request = request(Sandbox::Auto);
    request.image = Some("example:latest".to_owned());

    assert!(matches!(
        resolve(&request, &facts),
        Err(Obstacle::NoRuntime { .. })
    ));
}

#[test]
fn without_flatpak_an_image_is_used_when_one_was_named() {
    let facts = Facts {
        flatpak: None,
        ..needs_a_sandbox()
    };
    let mut request = request(Sandbox::Auto);
    request.image = Some("example:latest".to_owned());

    let plan = resolve(&request, &facts).expect("podman and an image are available");
    assert_eq!(
        reentered(&plan).runtime,
        super::Runtime::Image {
            engine: Engine::Podman,
            reference: "example:latest".to_owned(),
        }
    );
}

#[test]
fn a_machine_with_nothing_at_all_says_so_once() {
    let facts = Facts {
        flatpak: None,
        engines: Vec::new(),
        ..needs_a_sandbox()
    };
    assert_eq!(
        resolve(&request(Sandbox::Auto), &facts),
        Err(Obstacle::NoSandbox)
    );
}

#[test]
fn podman_without_an_image_says_which_engine_it_would_have_used() {
    let facts = Facts {
        engines: vec![Engine::Docker],
        ..needs_a_sandbox()
    };
    assert_eq!(
        resolve(&request(Sandbox::Podman), &facts),
        Err(Obstacle::NoImage {
            engine: Engine::Docker
        })
    );
}

#[test]
fn every_obstacle_names_a_fix() {
    // The rule carried from `waypuppet-select`: a diagnostic that only says no
    // is half a diagnostic. `Obstacle::fix` is an exhaustive match, so a new
    // variant cannot be added without answering this — the list here is what
    // makes sure the answer is not an empty string.
    let every = [
        Obstacle::Nested {
            enclosure: Enclosure::Flatpak,
        },
        Obstacle::NoSandbox,
        Obstacle::NoFlatpak,
        Obstacle::NoRuntime {
            wanted: FlatpakRuntime::parse("org.freedesktop.Platform//25.08"),
        },
        Obstacle::NoEngine,
        Obstacle::NoImage {
            engine: Engine::Podman,
        },
    ];

    for obstacle in every {
        let fix = obstacle.fix();
        assert!(!fix.is_empty(), "{obstacle:?} has no fix");
        // A fix a reader cannot act on is not a fix. Every one of these either
        // names a command to run or a flag to pass.
        assert!(
            fix.contains('`'),
            "{obstacle:?} does not name anything to run: {fix}"
        );
        assert!(!obstacle.to_string().is_empty(), "{obstacle:?} has no text");
    }
}

#[test]
fn the_socket_directory_is_always_mounted() {
    // The one mount that cannot be omitted: without it nothing can talk to the
    // session, and the failure looks like a session that never started.
    let plan = resolve(&request(Sandbox::Flatpak), &needs_a_sandbox()).expect("a runtime");
    let reenter = reentered(&plan);
    assert!(
        reenter
            .mounts
            .iter()
            .any(|mount| reenter.socket.starts_with(mount)),
        "{:?} does not cover {:?}",
        reenter.mounts,
        reenter.socket
    );
}

#[test]
fn a_nested_binary_does_not_get_its_own_mount() {
    // cwd already covers the binary underneath it. Two mounts where one will do
    // is not wrong, but the list is printed by `doctor` for a person to check,
    // and a list with redundant entries is harder to check.
    let plan = resolve(&request(Sandbox::Flatpak), &needs_a_sandbox()).expect("a runtime");
    let mounts = &reentered(&plan).mounts;
    assert!(mounts.contains(&PathBuf::from("/var/home/you/project")));
    assert!(
        !mounts
            .iter()
            .any(|mount| mount.starts_with("/var/home/you/project/target")),
        "{mounts:?} still has the target directory in it"
    );
}

#[test]
fn a_command_on_the_sandbox_path_contributes_no_mount() {
    // `waypuppet run -- gtk3-widget-factory` names something inside the runtime.
    // There is no host directory to mount, and inventing one by searching the
    // host's PATH would shadow the runtime's own copy.
    let mut request = request(Sandbox::Flatpak);
    request.command = None;
    let plan = resolve(&request, &needs_a_sandbox()).expect("a runtime");
    assert_eq!(
        reentered(&plan).mounts,
        vec![
            PathBuf::from("/run/user/1000/waypuppet"),
            PathBuf::from("/tmp"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/var/home/you/project"),
        ]
    );
}

#[test]
fn tidy_drops_the_root_rather_than_mounting_everything() {
    // Reachable by running waypuppet from `/`, which is odd but not an error. A
    // sandbox that mounts `/` is not a sandbox, and arriving there by accident
    // is worse than the mount being missing.
    assert_eq!(tidy(vec![PathBuf::from("/")]), Vec::<PathBuf>::new());
}

#[test]
fn tidy_keeps_siblings_that_merely_share_a_prefix() {
    // `/ab` is not inside `/a`, however the strings compare.
    let kept = tidy(vec![PathBuf::from("/a"), PathBuf::from("/ab")]);
    assert_eq!(kept, vec![PathBuf::from("/a"), PathBuf::from("/ab")]);
}

#[test]
fn tidy_drops_relative_paths() {
    // A relative mount cannot mean the same thing on both sides of the boundary,
    // so there is nothing sensible to do with one. `settle` is what stops them
    // arriving here at all; this is the backstop.
    assert_eq!(
        tidy(vec![PathBuf::from("relative/path")]),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn an_unsandboxed_session_may_write_anywhere() {
    // The variable is set only when waypuppet built the sandbox, so its absence
    // means every path on the machine means what it says. Defaulting the other
    // way would make an ordinary local session refuse to write its captures.
    assert!(writable(None, Path::new("/anywhere/at/all.png")));
}

#[test]
fn a_sandboxed_session_refuses_a_path_it_cannot_really_reach() {
    let visible = [
        PathBuf::from("/run/user/1000/waypuppet"),
        PathBuf::from("/var/home/you/project"),
    ];
    assert!(writable(
        Some(&visible),
        Path::new("/var/home/you/project/shot.png")
    ));
    // The dangerous one: inside a flatpak `/tmp` is a private tmpfs, so this
    // write would succeed, vanish, and report a path the caller cannot open.
    assert!(!writable(Some(&visible), Path::new("/tmp/shot.png")));
}

#[test]
fn a_runtime_without_a_branch_gets_the_default() {
    assert_eq!(
        FlatpakRuntime::parse("org.freedesktop.Platform"),
        FlatpakRuntime::parse("org.freedesktop.Platform//25.08")
    );
}

#[test]
fn the_sandbox_flag_round_trips() {
    for sandbox in [
        Sandbox::Auto,
        Sandbox::Flatpak,
        Sandbox::Podman,
        Sandbox::None,
    ] {
        assert_eq!(Sandbox::parse(&sandbox.to_string()), Ok(sandbox));
    }
    assert!(Sandbox::parse("bubblewrap").is_err());
}
