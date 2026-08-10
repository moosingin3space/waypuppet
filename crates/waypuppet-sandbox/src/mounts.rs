//! Which host directories a sandboxed session has to be able to see.
//!
//! Derived rather than asked for. A caller who has to write out a mount list has
//! not been spared the container, only handed a worse interface to it — so the
//! four directories that cover the ordinary case are worked out from the command
//! itself:
//!
//! - **the socket directory**, or nothing can talk to the session at all;
//! - **the capture directory**, or `shot` reports a path that does not exist for
//!   the caller;
//! - **the working directory**, because an application is usually run from its
//!   project and reads things out of it;
//! - **the directory of the command**, and of waypuppet's own binary, because
//!   neither can be executed inside a sandbox that cannot see it.
//!
//! Nothing here does I/O: [`crate::Request`] arrives with its paths already
//! absolute and symlink-free, so this is set algebra over strings and can be
//! tested against paths that do not exist.

use std::path::PathBuf;

use crate::Request;

/// The mount list for a request.
#[must_use]
pub(crate) fn derive(request: &Request) -> Vec<PathBuf> {
    let mut paths = vec![
        request.socket_dir.clone(),
        request.capture_dir.clone(),
        request.cwd.clone(),
    ];

    // The binaries, by their directories. waypuppet's own is not optional: it is
    // the thing being executed inside.
    paths.extend(request.exe.parent().map(PathBuf::from));
    // `None` when `argv[0]` named something on the sandbox's own `PATH`, which
    // is not a host path and would be a mount of nothing.
    paths.extend(
        request
            .command
            .as_ref()
            .and_then(|command| command.parent())
            .map(PathBuf::from),
    );

    paths.extend(request.extra.iter().cloned());
    tidy(paths)
}

/// Reduce a set of paths to the shortest list covering all of them.
///
/// Drops duplicates and anything already inside something else on the list, so a
/// project directory and a binary underneath it become one mount rather than
/// two. Also drops the root: a sandbox that mounts `/` is not a sandbox, and
/// waypuppet should not arrive there by accident because somebody ran it from
/// `/`.
#[must_use]
pub fn tidy(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = paths
        .into_iter()
        // `parent()` is `None` only for the root, which is exactly what to drop.
        .filter(|path| path.is_absolute() && path.parent().is_some())
        .collect();

    // Component-wise ordering puts every ancestor before its descendants, which
    // is what makes one pass enough.
    candidates.sort();
    candidates.dedup();

    let mut kept: Vec<PathBuf> = Vec::new();
    for path in candidates {
        if kept.iter().any(|covered| path.starts_with(covered)) {
            continue;
        }
        kept.push(path);
    }
    kept
}
