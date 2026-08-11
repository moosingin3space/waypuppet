//! Looking at the machine, so [`crate::resolve`] does not have to.
//!
//! Everything here is I/O and guesswork, which is precisely why it is quarantined
//! in one module and hands back plain data. The guesswork is unavoidable — there
//! is no portable way to ask "am I in a container?" — but it is confined to
//! reading two well-known marker files, and the answer it produces is used for
//! exactly one decision.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{Enclosure, Engine, Facts, FlatpakRuntime, Sandbox};

/// What this machine offers.
///
/// `renders` is the caller's EGL probe — the same one `doctor` reports. It comes
/// in as an argument rather than being taken here, because probing EGL means
/// bringing up a renderer and that belongs to the compositor crate; this crate
/// stays a decision about the environment rather than a second place that knows
/// how to render.
///
/// `want` is what the caller asked for, and is used only to skip work that
/// cannot change the answer. Asking flatpak what it has installed costs a
/// subprocess, and on the ordinary path — a developer machine that can already
/// render, with `--sandbox auto` — the answer is settled before flatpak is
/// relevant. Nothing else about the result depends on it.
#[must_use]
pub fn examine(renders: bool, want: Sandbox) -> Facts {
    let enclosure = enclosure();
    let render_node = render_node();

    // `Auto` on a machine that can already render never reaches a runtime, and
    // `None` never reaches one by definition.
    let settled = want == Sandbox::None || (want == Sandbox::Auto && renders);
    if settled {
        return Facts {
            enclosure,
            renders,
            render_node,
            flatpak: None,
            engines: Vec::new(),
        };
    }

    Facts {
        enclosure,
        renders,
        render_node,
        flatpak: flatpak_runtimes(),
        engines: engines(),
    }
}

/// Where this process is running.
///
/// Flatpak is checked first because a flatpak sandbox is built on the same
/// primitives an OCI container is and can carry both markers; the more specific
/// answer is the useful one.
fn enclosure() -> Enclosure {
    if Path::new("/.flatpak-info").exists() {
        return Enclosure::Flatpak;
    }
    if Path::new("/run/.containerenv").exists() {
        return Enclosure::Container(Engine::Podman);
    }
    if Path::new("/.dockerenv").exists() {
        return Enclosure::Container(Engine::Docker);
    }
    Enclosure::Host
}

/// Whether there is a DRM node to hand a sandbox.
///
/// Asked of the filesystem rather than of EGL, because the two questions differ
/// in the case that matters: a machine can have `/dev/dri/renderD128` sitting
/// there while this process cannot render at all for want of a mesa the sandbox
/// would supply. Passing `--device` when there is nothing to pass is an error
/// from podman, so it is worth knowing.
fn render_node() -> bool {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with("renderD") || name.starts_with("card")
    })
}

/// The flatpak runtimes installed, or `None` when flatpak is not.
fn flatpak_runtimes() -> Option<Vec<FlatpakRuntime>> {
    which("flatpak")?;

    // Both installations, deliberately. A runtime installed system-wide is as
    // usable as a user one, and reporting "not installed" for something plainly
    // present is the kind of wrongness that makes a tool untrustworthy.
    let output = Command::new("flatpak")
        .args(["list", "--runtime", "--columns=application,branch"])
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::debug!("`flatpak list` failed; treating flatpak as unusable");
        return None;
    }

    let listed = String::from_utf8_lossy(&output.stdout);
    Some(
        listed
            .lines()
            .filter_map(|line| {
                let (id, branch) = line.split_once('\t')?;
                Some(FlatpakRuntime {
                    id: id.trim().to_owned(),
                    branch: branch.trim().to_owned(),
                })
            })
            .collect(),
    )
}

/// The `OSTree` commit a runtime is deployed at, when it can be read.
///
/// This is what makes a flatpak-hosted capture comparable across machines and
/// across time: mesa and the font set both come out of the runtime, so this one
/// value already moves when either of them does, and nothing else has to be
/// measured to notice. It is the reason `--sandbox flatpak` is preferred over a
/// container for anything holding golden images — an image tag can move under
/// you and say nothing.
///
/// `None` rather than an error, because a fingerprint that cannot be pinned is a
/// real answer with its own fix. See [`waypuppet_fingerprint::Unpinned`].
#[must_use]
pub fn runtime_commit(runtime: &FlatpakRuntime) -> Option<String> {
    which("flatpak")?;

    let output = Command::new("flatpak")
        .args(["info", "--show-commit", &runtime.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::debug!(%runtime, "`flatpak info --show-commit` failed; the runtime is unpinned");
        return None;
    }

    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    // An empty answer is a failure that exited zero, and treating it as a commit
    // would put a fingerprint on the wire that pins nothing while claiming to.
    if commit.is_empty() {
        tracing::debug!(%runtime, "`flatpak info --show-commit` said nothing");
        return None;
    }
    Some(commit)
}

/// Container engines on `PATH`, podman first.
///
/// Podman leads because it needs no daemon and no group membership, so it is the
/// one more likely to work without anybody having arranged anything.
fn engines() -> Vec<Engine> {
    [Engine::Podman, Engine::Docker]
        .into_iter()
        .filter(|engine| which(engine.program()).is_some())
        .collect()
}

/// Find an executable on `PATH`.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| executable(candidate))
}

/// Whether a path is a file this process could run.
///
/// The mode check is deliberately loose — any execute bit at all. Working out
/// which of them applies means knowing this process's uid and gid and the file's
/// owner, and getting that subtly wrong would report a missing podman on a
/// machine that has one.
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Whether a flatpak runtime is installed, asked directly.
///
/// Used by `waypuppet setup` to check its own work; the plan itself is resolved
/// from the list [`examine`] gathers.
#[must_use]
pub fn runtime_installed(runtime: &FlatpakRuntime) -> bool {
    Command::new("flatpak")
        .arg("info")
        .arg(OsStr::new(&runtime.to_string()))
        .output()
        .is_ok_and(|output| output.status.success())
}
