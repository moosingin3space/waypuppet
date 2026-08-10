//! `waypuppet.toml`: what a repository can say once instead of every caller
//! passing flags.
//!
//! Optional, and that is the whole point. A project that needs a particular
//! runtime, or a directory outside the ones [`crate::mounts`] derives, says so
//! here and everyone who checks it out gets it. A project that needs neither
//! never writes the file — and if the defaults only worked once somebody had
//! written one, the file would be a requirement wearing a disguise.
//!
//! ```toml
//! [sandbox]
//! # A flatpak runtime, when the application needs a particular one. Named as
//! # `id//branch`, the way flatpak names it.
//! runtime = "org.freedesktop.Sdk//25.08"
//! # A container image, for `--sandbox podman`. There is no default: no
//! # well-known image carries an arbitrary application's libraries.
//! image = "ghcr.io/example/app-ci:latest"
//! # Anything the derived four do not cover. Relative paths resolve against this
//! # file, not against whoever happened to run waypuppet.
//! mounts = ["./assets", "/opt/shared-fonts"]
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{FlatpakRuntime, paths};

/// The file name searched for.
pub const FILE: &str = "waypuppet.toml";

/// What a project asked for.
#[derive(Debug, Clone)]
pub struct Project {
    /// Where the file was found, for reporting.
    pub path: PathBuf,
    /// A runtime to use instead of the preferred ones.
    pub runtime: Option<FlatpakRuntime>,
    /// A container image.
    pub image: Option<String>,
    /// Extra directories to mount, resolved against the file's own directory.
    pub mounts: Vec<PathBuf>,
}

/// A `waypuppet.toml` that could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// The file could not be read.
    #[error("{path}: {why}")]
    Unreadable {
        /// Where it was.
        path: PathBuf,
        /// What the filesystem said.
        why: String,
    },
    /// The file is not valid TOML, or does not have the shape expected.
    #[error(
        "{path}: {why}\n\n  help: the shape is a [sandbox] table with optional \
             `runtime`, `image` and `mounts` keys"
    )]
    Malformed {
        /// Where it was.
        path: PathBuf,
        /// What the parser objected to.
        why: String,
    },
}

/// The file as it is written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    sandbox: SandboxTable,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxTable {
    runtime: Option<String>,
    image: Option<String>,
    #[serde(default)]
    mounts: Vec<String>,
}

impl Project {
    /// Find and read the nearest `waypuppet.toml`, walking up from `from`.
    ///
    /// Upwards, because a caller runs waypuppet from wherever in the tree they
    /// happen to be and the file belongs at its root — the same search `cargo`
    /// and every other project tool does, so it needs no explaining.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError`] when a file was found and could not be used.
    /// A file that is not there is not an error: it is the ordinary case.
    pub fn find(from: &Path) -> Result<Option<Self>, ProjectError> {
        let mut directory = Some(from);
        while let Some(current) = directory {
            let candidate = current.join(FILE);
            if candidate.is_file() {
                return Self::read(&candidate).map(Some);
            }
            directory = current.parent();
        }
        Ok(None)
    }

    /// Read one, at a known path.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError`] if it cannot be read or does not parse.
    pub fn read(path: &Path) -> Result<Self, ProjectError> {
        let text = std::fs::read_to_string(path).map_err(|error| ProjectError::Unreadable {
            path: path.to_path_buf(),
            why: error.to_string(),
        })?;
        let file: File = toml::from_str(&text).map_err(|error| ProjectError::Malformed {
            path: path.to_path_buf(),
            why: error.message().to_owned(),
        })?;

        // Against the file's directory, not the working directory: the whole
        // value of putting this in a repository is that it means the same thing
        // wherever it is read from.
        let root = path.parent().unwrap_or(Path::new("."));
        let mounts = file
            .sandbox
            .mounts
            .iter()
            .map(|mount| paths::settle(&root.join(mount)))
            .collect();

        Ok(Self {
            path: path.to_path_buf(),
            runtime: file.sandbox.runtime.as_deref().map(FlatpakRuntime::parse),
            image: file.sandbox.image,
            mounts,
        })
    }
}
