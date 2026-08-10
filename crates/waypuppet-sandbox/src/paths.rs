//! Turning a path the caller wrote into the path a sandbox will see.
//!
//! This is the one piece of the sandbox that has bitten hardest in practice, and
//! it looks like nothing. On an image-based system `/home` is a symlink to
//! `/var/home`; inside a flatpak sandbox `/home` **does not exist at all**. So a
//! caller in `/home/you/project` who is handed a mount of `/home/you/project`
//! gets a sandbox that cannot see their project, and an error about a missing
//! directory that is plainly right there.
//!
//! [`settle`] is the fix: resolve symlinks before a path is used for anything,
//! so both sides are talking about the same directory rather than about two
//! spellings of it.

use std::path::{Component, Path, PathBuf};

/// Make `path` absolute and symlink-free, without requiring it to exist.
///
/// Existence cannot be required: the capture directory is a perfectly ordinary
/// thing to name before creating it, and refusing to plan a session around a
/// directory that is one `mkdir` away would be pedantry. So the deepest part of
/// the path that *does* exist is canonicalised, and the rest is appended as
/// written — which is exactly as much resolution as is available.
#[must_use]
pub fn settle(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    let absolute = lexical(&absolute);

    let mut trailing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut probe: &Path = &absolute;
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut settled = real;
            settled.extend(trailing.iter().rev());
            return settled;
        }
        let (Some(parent), Some(name)) = (probe.parent(), probe.file_name()) else {
            // Nothing along the way exists — a path under a directory that has
            // not been created yet. Lexically absolute is the best available
            // answer, and it is the same answer the caller would get.
            return absolute;
        };
        trailing.push(name);
        probe = parent;
    }
}

/// Resolve `.` and `..` without touching the filesystem.
///
/// Done before canonicalising because `..` past a symlink means something
/// different to the kernel than it does to a reader, and the reader's meaning is
/// the one a caller typing `../app` intended.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Where `argv[0]` actually is, when it names a file on this machine.
///
/// Returns `None` for a bare name such as `gtk3-widget-factory`, which is
/// resolved against the `PATH` of whatever ends up running it — inside a
/// sandbox, that is the sandbox's `PATH`, and there is no host directory to
/// mount. Guessing at one by searching the host's `PATH` would produce a mount
/// that is at best useless and at worst shadows the runtime's own copy.
#[must_use]
pub fn command_path(argv0: &str) -> Option<PathBuf> {
    if !argv0.contains('/') {
        return None;
    }
    Some(settle(Path::new(argv0)))
}
