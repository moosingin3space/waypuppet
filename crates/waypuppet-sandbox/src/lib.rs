//! Deciding which container runtime a session needs, and how to re-enter it.
//!
//! waypuppet needs system libraries and a render node. So, usually, does the
//! application under test — and **both have to end up in the same place**,
//! because they talk over a Unix socket and share graphics buffers. Left to the
//! caller that is a `flatpak run` line with six flags on it, most of them about
//! paths, which is exactly the kind of mechanics that makes a tool not worth
//! reaching for.
//!
//! So waypuppet works it out: [`resolve`] turns what the caller asked for into a
//! [`Plan`], which is either "run here" or "re-execute inside this runtime, with
//! these directories visible".
//!
//! # The decision is pure; only the looking is not
//!
//! [`resolve`] takes a [`Request`] and a set of [`Facts`] and does no I/O
//! whatsoever. Everything that touches the machine — reading `/.flatpak-info`,
//! asking flatpak what it has installed, finding `/dev/dri` — happens in
//! [`examine`] and arrives as data.
//!
//! That split is not tidiness. The interesting cases here are the ones that are
//! awkward to stand up: a machine with no flatpak, a container with no render
//! node, a runtime that is installed at the wrong branch. With the decision
//! separated from the detection, every one of them is three lines of test rather
//! than a fixture nobody will build. The mount-set derivation in [`mounts`] is
//! pure for the same reason, which is why [`Request`] carries paths that are
//! already absolute and symlink-free: resolving them is I/O, and it is done once
//! at the edge.
//!
//! # Path identity is the thing most likely to be wrong
//!
//! A capture written to `/tmp/x.png` inside the sandbox must be reported at a
//! path the *caller* can open. Flatpak mounts a `--filesystem` at the same
//! absolute path it has outside, so identity holds — but only for the paths that
//! were actually passed, and `/tmp` is a private tmpfs otherwise. A write there
//! succeeds and vanishes, which is the worst available failure because it has no
//! symptom.
//!
//! Two things follow, and both are load-bearing:
//!
//! - **Symlinks are resolved before anything is mounted.** On an image-based
//!   system `/home` is a symlink to `/var/home`, and inside the sandbox `/home`
//!   does not exist at all. A mount derived from an unresolved path is a mount
//!   the session cannot reach.
//! - **The session is told what is visible**, via [`VISIBLE`], so a capture aimed
//!   somewhere else is refused by name instead of being written into the void.

mod detect;
mod mounts;
mod paths;
mod project;

use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
};

pub use crate::{
    detect::{examine, runtime_installed},
    mounts::tidy,
    paths::{command_path, settle},
    project::{FILE as PROJECT_FILE, Project, ProjectError},
};

/// The environment variable naming the directory a session's control socket
/// lives in.
///
/// `XDG_RUNTIME_DIR` is where it lives by default, and inside a sandbox that
/// variable still has a value — a *different* directory with the same name. The
/// two agreeing on the host is a coincidence of how flatpak sets things up, and
/// a coincidence is not something to hang the control socket on. So the
/// directory is named outright when a session is re-entered, and inferred only
/// when nobody said.
pub const SOCKET_DIR: &str = "WAYPUPPET_SOCKET_DIR";

/// The environment variable listing the host paths a sandboxed session can see.
///
/// Set only when waypuppet put the session in a sandbox, so an unset variable
/// means "everything", which is the truth for a session running on the host.
/// Entries are separated the way `PATH` is.
pub const VISIBLE: &str = "WAYPUPPET_SANDBOX_PATHS";

/// The variables waypuppet is authoritative for, which a child must never
/// inherit.
///
/// A client that picked up the caller's real `WAYLAND_DISPLAY` would open a
/// window on their desktop and look like it had worked. That is the worst
/// available failure — it is indistinguishable from success — and it is why this
/// list is applied at the sandbox boundary as well as at the child, rather than
/// only at the child.
pub const AUTHORITATIVE: &[&str] = &[
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "AT_SPI_BUS_ADDRESS",
];

/// Which sandbox the caller asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sandbox {
    /// Work it out: run here if this process can already do the job, and find a
    /// runtime otherwise.
    #[default]
    Auto,
    /// A flatpak runtime, always, even if this process could manage without one.
    Flatpak,
    /// A container image, always.
    Podman,
    /// Here, whatever the state of the machine.
    ///
    /// The escape hatch for somebody who has already arranged an environment and
    /// does not want waypuppet second-guessing it.
    None,
}

impl Sandbox {
    /// Parse the `--sandbox` value.
    ///
    /// # Errors
    ///
    /// Returns the list of accepted values, for a caller to print, when `text`
    /// is not one of them.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "flatpak" => Ok(Self::Flatpak),
            "podman" | "docker" | "container" => Ok(Self::Podman),
            "none" | "here" => Ok(Self::None),
            _ => Err(format!(
                "--sandbox wants auto, flatpak, podman or none, got {text:?}"
            )),
        }
    }
}

impl fmt::Display for Sandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Flatpak => "flatpak",
            Self::Podman => "podman",
            Self::None => "none",
        })
    }
}

/// A container engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// Podman, preferred because it needs no daemon and no privileges.
    Podman,
    /// Docker.
    Docker,
}

impl Engine {
    /// The binary to invoke.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.program())
    }
}

/// Where this process is running.
///
/// Detected from the markers each runtime leaves behind, because there is no
/// portable way to ask. It matters for exactly one decision, but an important
/// one: a process already inside a container cannot open another, so a container
/// that cannot render is a dead end rather than something to try harder at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Enclosure {
    /// Not in a container, as far as anything can tell.
    #[default]
    Host,
    /// Inside a flatpak sandbox — `/.flatpak-info` exists.
    Flatpak,
    /// Inside an OCI container — `/run/.containerenv` or `/.dockerenv` exists.
    Container(Engine),
}

impl fmt::Display for Enclosure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host => f.write_str("host"),
            Self::Flatpak => f.write_str("flatpak"),
            Self::Container(engine) => write!(f, "{engine} container"),
        }
    }
}

/// A flatpak runtime, as `id//branch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatpakRuntime {
    /// The application id, such as `org.freedesktop.Platform`.
    pub id: String,
    /// The branch, such as `25.08`.
    pub branch: String,
}

impl FlatpakRuntime {
    /// Parse `id//branch`, or `id` at the default branch.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text.split_once("//") {
            Some((id, branch)) => Self {
                id: id.to_owned(),
                branch: branch.to_owned(),
            },
            None => Self {
                id: text.to_owned(),
                branch: DEFAULT_BRANCH.to_owned(),
            },
        }
    }
}

impl fmt::Display for FlatpakRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}//{}", self.id, self.branch)
    }
}

/// The freedesktop branch waypuppet asks for when nobody says otherwise.
pub const DEFAULT_BRANCH: &str = "25.08";

/// The runtimes waypuppet will use, in the order it prefers them.
///
/// `Platform` first and `Sdk` second, which is the opposite of what waypuppet's
/// own Justfile does — and deliberately so. The SDK adds a compiler toolchain,
/// which is what *building* waypuppet needs and what *running* an application
/// does not; the Platform carries libwayland, EGL, GBM and mesa, which is the
/// whole list. It is also already installed on any machine with a single
/// graphical flatpak on it, so preferring it is usually the difference between
/// working immediately and asking somebody to download a gigabyte.
#[must_use]
pub fn preferred_runtimes() -> Vec<FlatpakRuntime> {
    ["org.freedesktop.Platform", "org.freedesktop.Sdk"]
        .into_iter()
        .map(|id| FlatpakRuntime {
            id: id.to_owned(),
            branch: DEFAULT_BRANCH.to_owned(),
        })
        .collect()
}

/// What the machine offers, as gathered by [`examine`].
///
/// Data, not a handle to anything: [`resolve`] reads this and never looks at the
/// machine itself, which is what makes the decision testable.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// Where this process is running.
    pub enclosure: Enclosure,
    /// Whether this process found something to render with.
    ///
    /// The same EGL probe `doctor` reports, and the only capability worth
    /// testing: if the binary is running at all then its libraries resolved, so
    /// a render device is what is left to be missing.
    pub renders: bool,
    /// Whether the machine has a DRM render node to hand a sandbox.
    ///
    /// Distinct from [`Facts::renders`], which can be false — no EGL, no mesa —
    /// while the node is right there.
    pub render_node: bool,
    /// Flatpak runtimes installed, or `None` when flatpak is not installed at
    /// all.
    ///
    /// The distinction earns its `Option`: "no flatpak" and "flatpak with
    /// nothing suitable in it" have different fixes, and collapsing them would
    /// tell somebody to install what they already have.
    pub flatpak: Option<Vec<FlatpakRuntime>>,
    /// Container engines on `PATH`, in preference order.
    pub engines: Vec<Engine>,
}

/// What the caller wants done.
///
/// Every path here must already be absolute and symlink-free — see [`settle`],
/// which is how the CLI gets them that way. Resolving them is I/O, so it happens
/// once at the edge rather than in the middle of a decision that is otherwise
/// pure.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Which sandbox the caller asked for.
    pub sandbox: Sandbox,
    /// waypuppet's own binary, which has to exist inside the sandbox too.
    pub exe: PathBuf,
    /// The application under test, when `argv[0]` names a file on this machine.
    ///
    /// `None` when it does not — `waypuppet run -- gtk3-widget-factory` names
    /// something on the sandbox's `PATH`, which is not a host path and not
    /// something to mount.
    pub command: Option<PathBuf>,
    /// The working directory the caller ran from.
    pub cwd: PathBuf,
    /// Where captures land.
    pub capture_dir: PathBuf,
    /// Where the control socket lives.
    pub socket_dir: PathBuf,
    /// Anything else the caller or `waypuppet.toml` asked to have mounted.
    pub extra: Vec<PathBuf>,
    /// A flatpak runtime to use instead of the preferred ones.
    pub runtime: Option<FlatpakRuntime>,
    /// A container image, which podman needs and cannot guess.
    pub image: Option<String>,
}

/// What waypuppet decided to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Run the session in this process.
    Direct(Direct),
    /// Re-execute inside a runtime, and run the session there.
    Reenter(Box<Reenter>),
}

/// Why a session is running here rather than in a sandbox.
///
/// Carried rather than discarded because `doctor` has to explain the decision,
/// and "waypuppet can already render here" and "you told me not to" are
/// different answers to the same question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direct {
    /// This process can already render, so there is nothing a sandbox would add.
    Capable,
    /// The caller passed `--sandbox none`.
    Asked,
}

impl fmt::Display for Direct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Capable => "this process can already render",
            Self::Asked => "--sandbox none",
        })
    }
}

/// Which runtime a session is re-entered into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runtime {
    /// A flatpak runtime.
    Flatpak(FlatpakRuntime),
    /// A container image, run by an engine.
    Image {
        /// Which engine runs it.
        engine: Engine,
        /// The image reference.
        reference: String,
    },
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flatpak(runtime) => write!(f, "flatpak {runtime}"),
            Self::Image { engine, reference } => write!(f, "{engine} {reference}"),
        }
    }
}

/// The plan for putting a session inside a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reenter {
    /// Which runtime.
    pub runtime: Runtime,
    /// The host directories to make visible, at the same absolute paths.
    pub mounts: Vec<PathBuf>,
    /// Where the control socket goes.
    ///
    /// Always one of [`Reenter::mounts`], and named separately because both
    /// sides have to agree on it exactly. Digging it back out of the mount list
    /// by position would be the kind of coincidence this module exists to
    /// remove.
    pub socket: PathBuf,
    /// The directory to run in, which is the one the caller ran from.
    ///
    /// Stated rather than left to the runtime's own idea of a starting
    /// directory, because `waypuppet run -- ./target/debug/app` is the shape the
    /// README promises and a relative `argv[0]` only resolves if the working
    /// directory came across too.
    pub cwd: PathBuf,
    /// waypuppet's own binary, which is what gets executed inside.
    pub exe: PathBuf,
    /// Whether to hand the sandbox a render node.
    pub dri: bool,
}

impl Reenter {
    /// The command that runs `waypuppet serve` inside the runtime.
    ///
    /// `args` is everything after the binary — the `serve` verb and its flags.
    /// Two things are set here rather than left to be inferred: [`SOCKET_DIR`],
    /// so both sides name the same socket instead of both computing one from a
    /// variable the sandbox rewrites, and [`VISIBLE`], so the session can refuse
    /// to write a capture the caller would never find.
    #[must_use]
    pub fn command<I, S>(&self, args: I) -> std::process::Command
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let visible = self.visible();
        match &self.runtime {
            Runtime::Flatpak(runtime) => {
                let mut command = std::process::Command::new("flatpak");
                // `--user` disambiguates: a runtime may be installed both
                // system- and user-wide, and flatpak stops to ask which
                // otherwise — a prompt nobody is there to answer.
                command.arg("run").arg("--user");
                command.arg("--share=network").arg("--share=ipc");
                if self.dri {
                    command.arg("--device=dri");
                }
                for mount in &self.mounts {
                    command.arg(format!("--filesystem={}", mount.display()));
                }
                for name in AUTHORITATIVE {
                    command.arg(format!("--unset-env={name}"));
                }
                command.arg(format!("--env={SOCKET_DIR}={}", self.socket.display()));
                command.arg(format!("--env={VISIBLE}={}", visible.to_string_lossy()));
                command.arg(format!("--cwd={}", self.cwd.display()));
                // `XDG_RUNTIME_DIR` is left alone here, unlike for an image:
                // flatpak supplies a writable one and populates it with symlinks
                // of its own, so overriding it would break more than it fixed.
                command.arg(format!("--command={}", self.exe.display()));
                command.arg(runtime.to_string());
                command.args(args.into_iter().map(Into::into));
                command
            }
            Runtime::Image { engine, reference } => {
                let mut command = std::process::Command::new(engine.program());
                command.arg("run").arg("--rm").arg("--init");
                // Files the session writes have to belong to the caller, or the
                // capture it just reported is one they cannot open.
                if *engine == Engine::Podman {
                    command.arg("--userns=keep-id");
                }
                if self.dri {
                    command.arg("--device").arg("/dev/dri");
                }
                for mount in &self.mounts {
                    // `:Z` relabels for SELinux, which podman needs on
                    // Fedora-family hosts and docker ignores.
                    command
                        .arg("--volume")
                        .arg(format!("{0}:{0}:Z", mount.display()));
                }
                // An image inherits nothing, so the pass-through that flatpak
                // does by default has to be done by hand here.
                for (name, value) in std::env::vars_os() {
                    let named = name.to_string_lossy();
                    if AUTHORITATIVE.iter().any(|hidden| *hidden == named)
                        || named == "XDG_RUNTIME_DIR"
                    {
                        continue;
                    }
                    let mut pair = name;
                    pair.push("=");
                    pair.push(value);
                    command.arg("--env").arg(pair);
                }
                // The Wayland socket goes under `XDG_RUNTIME_DIR`, so it has to
                // name somewhere this process can actually write. The host's
                // value is not mounted, and an image's own is typically created
                // by root at mode 700 — which `--userns=keep-id` then maps to
                // somebody else, and the compositor fails at startup with
                // "Could not write to XDG_RUNTIME_DIR". The session directory is
                // the one place known to be both mounted and ours.
                command
                    .arg("--env")
                    .arg(format!("XDG_RUNTIME_DIR={}", self.socket.display()));
                command.arg("--workdir").arg(&self.cwd);
                command
                    .arg("--env")
                    .arg(format!("{SOCKET_DIR}={}", self.socket.display()));
                let mut visible_arg = OsString::from(format!("{VISIBLE}="));
                visible_arg.push(&visible);
                command.arg("--env").arg(visible_arg);
                command.arg("--entrypoint").arg(&self.exe);
                command.arg(reference);
                command.args(args.into_iter().map(Into::into));
                command
            }
        }
    }

    /// The mount list, separated the way `PATH` is.
    fn visible(&self) -> OsString {
        let mut list = OsString::new();
        for (index, mount) in self.mounts.iter().enumerate() {
            if index > 0 {
                list.push(":");
            }
            list.push(mount);
        }
        list
    }
}

/// Why waypuppet cannot put a session anywhere.
///
/// Every variant names a fix, and [`Obstacle::fix`] is an exhaustive match so a
/// new one cannot be added without answering the question. That rule is carried
/// from `waypuppet-select`, and for the same reason: a diagnostic that only says
/// no is half a diagnostic, and the half that does not help. It matters more
/// here than almost anywhere, because the reader is somebody who has just been
/// told a container is involved and did not want to know that.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Obstacle {
    /// Already inside a container that cannot render, and a container cannot
    /// open another.
    ///
    /// Worth an explicit error rather than a confusing one: this cannot be fixed
    /// by trying harder, and every remaining option is about the *outer*
    /// container, which waypuppet cannot reach from in here.
    Nested {
        /// What this process is inside.
        enclosure: Enclosure,
    },
    /// Nothing on this machine can host a session.
    NoSandbox,
    /// `--sandbox flatpak`, and flatpak is not installed.
    NoFlatpak,
    /// flatpak is installed, but the runtime waypuppet wants is not.
    NoRuntime {
        /// The runtime that would have been used.
        wanted: FlatpakRuntime,
    },
    /// `--sandbox podman`, and neither podman nor docker is installed.
    NoEngine,
    /// There is an engine, but no image was named and there is no default worth
    /// guessing at.
    NoImage {
        /// The engine that would have run it.
        engine: Engine,
    },
}

impl Obstacle {
    /// What to do about it. Never empty.
    #[must_use]
    pub fn fix(&self) -> String {
        match self {
            Self::Nested { .. } => "give this container a render node — `--device /dev/dri` \
                 for podman or docker, `--device=dri` for flatpak — or run waypuppet \
                 outside it. `waypuppet doctor` reports what it found in here."
                .to_owned(),
            Self::NoSandbox => format!(
                "install flatpak, then run `waypuppet setup` to fetch the {} runtime. \
                 If this machine can already render, `--sandbox none` skips all of this.",
                preferred_runtimes()
                    .first()
                    .map_or_else(String::new, ToString::to_string)
            ),
            Self::NoFlatpak => "install flatpak, or pass `--sandbox podman --image <image>` \
                 to use a container image instead."
                .to_owned(),
            Self::NoRuntime { wanted } => format!(
                "run `waypuppet setup`, or `flatpak install --user {wanted}` to fetch it \
                 yourself. waypuppet does not install runtimes on its own, because a first \
                 run that silently downloads a gigabyte is a bad surprise."
            ),
            Self::NoEngine => {
                "install podman, or pass `--sandbox flatpak` to use a flatpak runtime instead."
                    .to_owned()
            }
            Self::NoImage { engine } => format!(
                "name one with `--image <image>`, or put `image = \"<image>\"` in a \
                 waypuppet.toml beside your project. Unlike a flatpak runtime, there is no \
                 well-known {engine} image carrying the libraries an application needs, so \
                 this is the one thing waypuppet cannot work out for you."
            ),
        }
    }
}

impl fmt::Display for Obstacle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Deliberately says nothing about rendering. This is reached both by
            // a container that cannot render and by a caller who asked for a
            // sandbox from inside one that can, and the invariant those share is
            // the containment, not the graphics.
            Self::Nested { enclosure } => write!(
                f,
                "this is already a {enclosure}, and a container cannot open another"
            ),
            Self::NoSandbox => f.write_str(
                "this machine cannot render, and has no flatpak or container engine \
                 to borrow one from",
            ),
            Self::NoFlatpak => f.write_str("flatpak is not installed"),
            Self::NoRuntime { wanted } => write!(f, "the {wanted} runtime is not installed"),
            Self::NoEngine => f.write_str("neither podman nor docker is installed"),
            Self::NoImage { engine } => write!(f, "{engine} needs an image, and none was named"),
        }
    }
}

impl std::error::Error for Obstacle {}

/// Work out where a session should run.
///
/// Does no I/O: everything it knows about the machine arrives in `facts`, which
/// [`examine`] gathers.
///
/// # Errors
///
/// Returns an [`Obstacle`] when there is nowhere to put a session. Each one
/// names the missing piece and the command that supplies it.
pub fn resolve(request: &Request, facts: &Facts) -> Result<Plan, Obstacle> {
    match request.sandbox {
        Sandbox::None => Ok(Plan::Direct(Direct::Asked)),
        // The order matters: being able to render here settles it before
        // anything is asked about flatpak, so the ordinary case on a developer's
        // own machine costs no subprocesses and no surprises.
        Sandbox::Auto if facts.renders => Ok(Plan::Direct(Direct::Capable)),
        Sandbox::Auto => {
            // Nothing below can work from in here, and saying which of the three
            // it is would only be a longer way of saying that.
            if facts.enclosure != Enclosure::Host {
                return Err(Obstacle::Nested {
                    enclosure: facts.enclosure,
                });
            }
            match flatpak_runtime(request, facts) {
                Ok(runtime) => Ok(reenter(request, facts, Runtime::Flatpak(runtime))),
                // Falling through to an engine only when flatpak is absent
                // entirely. A flatpak that is merely missing a runtime is one
                // `waypuppet setup` away, which is a better answer than
                // silently reaching for a container image the caller has not
                // heard of.
                Err(Obstacle::NoFlatpak) => match image(request, facts) {
                    Ok(runtime) => Ok(reenter(request, facts, runtime)),
                    Err(_) => Err(Obstacle::NoSandbox),
                },
                Err(obstacle) => Err(obstacle),
            }
        }
        Sandbox::Flatpak => {
            if facts.enclosure != Enclosure::Host {
                return Err(Obstacle::Nested {
                    enclosure: facts.enclosure,
                });
            }
            let runtime = flatpak_runtime(request, facts)?;
            Ok(reenter(request, facts, Runtime::Flatpak(runtime)))
        }
        Sandbox::Podman => {
            if facts.enclosure != Enclosure::Host {
                return Err(Obstacle::Nested {
                    enclosure: facts.enclosure,
                });
            }
            let runtime = image(request, facts)?;
            Ok(reenter(request, facts, runtime))
        }
    }
}

/// Which flatpak runtime to use.
fn flatpak_runtime(request: &Request, facts: &Facts) -> Result<FlatpakRuntime, Obstacle> {
    let installed = facts.flatpak.as_ref().ok_or(Obstacle::NoFlatpak)?;

    // A runtime the caller named is used as given or reported missing as given.
    // Falling back to a different one would be answering a question they did not
    // ask, and `waypuppet.toml` naming a runtime is usually a statement that the
    // application needs *that* one.
    if let Some(wanted) = &request.runtime {
        return if installed.contains(wanted) {
            Ok(wanted.clone())
        } else {
            Err(Obstacle::NoRuntime {
                wanted: wanted.clone(),
            })
        };
    }

    preferred_runtimes()
        .into_iter()
        .find(|runtime| installed.contains(runtime))
        .ok_or_else(|| Obstacle::NoRuntime {
            wanted: preferred_runtimes().swap_remove(0),
        })
}

/// Which image to use, and what runs it.
fn image(request: &Request, facts: &Facts) -> Result<Runtime, Obstacle> {
    let engine = *facts.engines.first().ok_or(Obstacle::NoEngine)?;
    let reference = request.image.clone().ok_or(Obstacle::NoImage { engine })?;
    Ok(Runtime::Image { engine, reference })
}

/// Assemble the plan for a runtime that has been settled on.
fn reenter(request: &Request, facts: &Facts, runtime: Runtime) -> Plan {
    Plan::Reenter(Box::new(Reenter {
        runtime,
        mounts: mounts::derive(request),
        socket: request.socket_dir.clone(),
        cwd: request.cwd.clone(),
        exe: request.exe.clone(),
        dri: facts.render_node,
    }))
}

/// Read [`VISIBLE`] from the environment.
///
/// `None` means the variable is unset, which is the ordinary case: waypuppet
/// sets it only when it built the sandbox itself, so its absence says "this
/// session is not confined" rather than "this session can see nothing". Getting
/// that default the wrong way round would make an ordinary local session refuse
/// to write its own captures.
#[must_use]
pub fn visible_paths() -> Option<Vec<PathBuf>> {
    let raw = std::env::var_os(VISIBLE)?;
    Some(std::env::split_paths(&raw).collect())
}

/// Whether a session may write to `path` and have the caller find it there.
///
/// `visible` is what [`visible_paths`] returned, so `None` permits everything.
#[must_use]
pub fn writable(visible: Option<&[PathBuf]>, path: &Path) -> bool {
    let Some(visible) = visible else {
        return true;
    };
    visible.iter().any(|entry| path.starts_with(entry))
}

#[cfg(test)]
mod tests;
