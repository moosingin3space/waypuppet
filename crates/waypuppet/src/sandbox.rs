//! Putting a session somewhere it can run, without the caller having to know.
//!
//! `waypuppet run -- ./target/debug/my-app` is the whole interface. Everything
//! here is what stands behind that sentence: working out whether this machine
//! can host a session at all, finding a runtime when it cannot, deriving which
//! directories have to be visible from inside it, and starting the thing.
//!
//! The decision itself lives in `waypuppet-sandbox` and is pure. This module is
//! the edge: it reads the environment, resolves the paths, and turns an
//! [`Obstacle`] into something a person can act on.
//!
//! [`Obstacle`]: waypuppet_sandbox::Obstacle

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use waypuppet_sandbox::{
    Facts, FlatpakRuntime, Obstacle, Plan, Project, Reenter, Request, Sandbox,
};

/// How long to wait for a sandboxed session to start listening.
///
/// Longer than the direct case deserves, because a first `flatpak run` of a
/// runtime does real work before waypuppet's `main` is reached.
const START: Duration = Duration::from_mins(1);

/// The sandbox flags, as `run` and `doctor` take them.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `--sandbox`.
    pub sandbox: Sandbox,
    /// `--sandbox-mount`, repeated.
    pub mounts: Vec<PathBuf>,
    /// `--image`.
    pub image: Option<String>,
}

/// Work out where a session should run.
///
/// `facts` comes in rather than being gathered here so a caller that has already
/// probed EGL — `doctor` prints the device list either way — does not pay for it
/// twice.
///
/// # Errors
///
/// Returns a rendered diagnostic — the problem, then the fix — when there is
/// nowhere to put one, or when a `waypuppet.toml` could not be used.
pub fn plan(
    options: &Options,
    argv: &[String],
    socket: &Path,
    capture_dir: &Path,
    facts: &Facts,
) -> Result<Plan, String> {
    let request = request(options, argv, socket, capture_dir)?;
    waypuppet_sandbox::resolve(&request, facts).map_err(|obstacle| explain(&obstacle))
}

/// Gather what the machine offers.
///
/// The EGL probe is the expensive half and is skipped where the answer cannot
/// matter: only `--sandbox auto` asks whether this process can already render,
/// and bringing up a renderer to answer a question nobody asked would put a
/// visible pause on every `run --sandbox none`.
#[must_use]
pub fn facts(sandbox: Sandbox) -> Facts {
    let renders = sandbox == Sandbox::Auto && renders();
    waypuppet_sandbox::examine(renders, sandbox)
}

/// Whether this process found anything to render with.
fn renders() -> bool {
    waypuppet_compositor::probe().is_ok_and(|devices| !devices.is_empty())
}

/// Build the request, resolving every path as it goes.
///
/// This is where symlinks stop existing. `waypuppet-sandbox` promises nothing
/// about paths it was handed unresolved, and for good reason: on an image-based
/// system `/home` is a symlink to `/var/home` and does not exist inside a
/// sandbox at all, so a mount derived from the unresolved spelling is a mount of
/// nothing.
fn request(
    options: &Options,
    argv: &[String],
    socket: &Path,
    capture_dir: &Path,
) -> Result<Request, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the working directory: {error}"))?;
    let cwd = waypuppet_sandbox::settle(&cwd);
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot find waypuppet's own binary: {error}"))?;

    let project = Project::find(&cwd).map_err(|error| error.to_string())?;
    if let Some(project) = &project {
        tracing::debug!(path = %project.path.display(), "using project configuration");
    }

    // The caller's flags win over the file's: the file is what a repository says
    // by default, and a flag is somebody overriding it on purpose.
    let mut extra = options
        .mounts
        .iter()
        .map(|m| waypuppet_sandbox::settle(m))
        .collect::<Vec<_>>();
    extra.extend(
        project
            .iter()
            .flat_map(|project| project.mounts.iter().cloned()),
    );

    Ok(Request {
        sandbox: options.sandbox,
        exe: waypuppet_sandbox::settle(&exe),
        command: argv
            .first()
            .and_then(|argv0| waypuppet_sandbox::command_path(argv0)),
        cwd,
        capture_dir: waypuppet_sandbox::settle(capture_dir),
        socket_dir: waypuppet_sandbox::settle(socket.parent().unwrap_or(Path::new("/"))),
        extra,
        runtime: project.as_ref().and_then(|project| project.runtime.clone()),
        image: options
            .image
            .clone()
            .or_else(|| project.as_ref().and_then(|project| project.image.clone())),
    })
}

/// Render an obstacle the way `waypuppet-select` renders a bad selector: what is
/// wrong, then what to do about it.
///
/// The shape matters more here than almost anywhere else in waypuppet. The
/// reader has just been told that a container is involved, which is the one
/// thing this whole plan exists to spare them — so the message had better end
/// with a command rather than with a diagnosis.
fn explain(obstacle: &Obstacle) -> String {
    format!("{obstacle}\n\n  help: {}", obstacle.fix())
}

/// Start a session inside a runtime, and wait for it to be reachable.
///
/// `args` is everything after the binary: the `serve` verb and its flags.
///
/// # Errors
///
/// Returns a message if the runtime could not be started, or if it started and
/// the session never came up — in which case whatever it managed to say is
/// included, because that is the part that explains why.
pub async fn start(
    reenter: &Reenter,
    args: &[String],
    socket: &Path,
    detach: bool,
) -> Result<ExitCode, String> {
    // The socket directory has to exist before the sandbox mounts it. Flatpak
    // tolerates a `--filesystem` that is not there, and then the session inside
    // creates it — inside, where nobody else can see it, and the caller waits
    // out the full timeout on a socket that will never appear.
    for mount in &reenter.mounts {
        if let Err(error) = std::fs::create_dir_all(mount) {
            tracing::debug!(path = %mount.display(), %error, "could not create a mount point");
        }
    }

    let mut command = tokio::process::Command::from(reenter.command(args));
    tracing::debug!(?command, "starting a sandboxed session");

    if !detach {
        // Inherit everything. The session's own diagnostics — the client exited,
        // here is its stderr — are the reason `run` is worth using in the
        // foreground, and swallowing them to tidy the output would remove it.
        let status = command
            .status()
            .await
            .map_err(|error| started(reenter, &error))?;
        return Ok(status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from));
    }

    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| started(reenter, &error))?;

    // Two things can happen, and waiting for only the good one is how a
    // mistyped image reference becomes a sixty-second pause. Poll the socket,
    // but let the runtime exiting end the wait immediately and hand back what it
    // said, since that is the message that explains the failure.
    let deadline = tokio::time::Instant::now() + START;
    loop {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return Ok(ExitCode::SUCCESS);
        }
        if let Ok(Some(status)) = child.try_wait() {
            let mut said = String::new();
            if let Some(stderr) = child.stderr.take() {
                said = read_all(stderr).await;
            }
            let said = said.trim();
            return Err(if said.is_empty() {
                format!(
                    "{} exited with {status} before the session started",
                    reenter.runtime
                )
            } else {
                format!(
                    "{} exited with {status} before the session started:\n{said}",
                    reenter.runtime
                )
            });
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.start_kill();
            return Err(format!(
                "the session did not start listening on {} within {START:?}",
                socket.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Everything a stream has to say, for reporting a failed start.
async fn read_all<R>(mut reader: R) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let mut buffer = Vec::new();
    let _ = reader.read_to_end(&mut buffer).await;
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The runtime could not be started at all.
///
/// Almost always a missing binary, which is worth saying outright: the plan was
/// resolved against a `flatpak` that was on `PATH` a moment ago, so anything
/// else here is unusual enough to want the raw error.
fn started(reenter: &Reenter, error: &std::io::Error) -> String {
    format!("cannot start {}: {error}", reenter.runtime)
}

/// Install the runtime waypuppet would use.
///
/// Deliberately a verb of its own rather than something `run` does when it finds
/// a runtime missing. A first run that silently pulls a gigabyte is a bad
/// surprise, and the difference between a tool that is convenient and one that
/// is presumptuous is exactly whether it asked.
///
/// # Errors
///
/// Returns a message if flatpak is not installed or the install failed.
pub fn setup(runtime: Option<&FlatpakRuntime>) -> Result<(), String> {
    let runtime = runtime
        .cloned()
        .unwrap_or_else(|| waypuppet_sandbox::preferred_runtimes().swap_remove(0));

    if waypuppet_sandbox::runtime_installed(&runtime) {
        println!("{runtime} is already installed");
        return Ok(());
    }

    println!("installing {runtime}");
    let status = std::process::Command::new("flatpak")
        .args(["install", "--user", "--assumeyes"])
        .arg(runtime.to_string())
        .status()
        .map_err(|error| {
            format!(
                "cannot run flatpak: {error}\n\n  help: install flatpak, or pass \
                     `--sandbox podman --image <image>` to use a container image instead"
            )
        })?;

    if !status.success() {
        return Err(format!(
            "flatpak could not install {runtime}\n\n  help: `flatpak remotes` should list \
             flathub; add it with `flatpak remote-add --user --if-not-exists flathub \
             https://dl.flathub.org/repo/flathub.flatpakrepo`"
        ));
    }
    Ok(())
}
