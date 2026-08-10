//! The `waypuppet` command.
//!
//! One binary, two roles, in the shape of `tmux`: `waypuppet run` starts a
//! session — a headless compositor holding the application under test — and
//! every other verb is a short-lived client of that session's control socket.
//!
//! # Status
//!
//! Every verb works: `doctor`, `serve`, `run`, `ps`, `shot`, `logs`, `kill`,
//! `sessions`, `wait`, `clipboard`, `tree`, `select`, and the input verbs
//! `key`, `type`, `click`, `move`, `scroll` and `resize`.
//!
//! Selectors name a widget instead of a coordinate: `click --on`,
//! `key`/`type --focus-on`, and `select` to try one without touching the
//! application. The grammar is CSS and lives in `waypuppet-select`; what this
//! crate adds is the two ways to write one — text, and the JSON form of the
//! query — and the rendering of what comes back, which for an ambiguous match
//! means printing selectors a caller can paste rather than a count.

mod client;
mod sandbox;

use std::{path::Path, path::PathBuf, process::ExitCode, time::Duration};

use clap::{Args, Parser, Subcommand};
use waypuppet_compositor::{Rendering, session};
use waypuppet_proto::{
    Button, ClientState, Cursor, Failure, Locator, Point, Query, Selection, Settled, Size, Target,
    WindowId,
};

/// What `wait` exits with when its condition did not hold in time.
///
/// Distinct from the 1 every other failure uses, because it is not a failure:
/// `waypuppet wait --exit; test $? -eq 3` is a caller asking "is it still
/// running?" and getting a straight answer.
///
/// 3 rather than the obvious 2, which clap already spends on usage errors — a
/// script cannot tell "the condition did not hold" from "you misspelled a flag"
/// if both exit the same way.
const TIMED_OUT: u8 = 3;

/// Run a GUI application inside a headless compositor, then look at it and
/// drive it.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Which session to talk to. Sessions are named so several can run at
    /// once; the socket lives at `$XDG_RUNTIME_DIR/waypuppet/<name>.sock`.
    #[arg(
        long,
        short,
        global = true,
        env = "WAYPUPPET_SESSION",
        default_value = "default"
    )]
    session: String,

    /// Emit responses as JSON rather than for human eyes.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report what waypuppet can render with, and where it would run a session.
    ///
    /// Run this first in any new container. It is the fastest way to tell a
    /// missing render node apart from a missing library — and it prints the
    /// resolved sandbox plan, which is where to look when a session can see the
    /// binary but the binary cannot see its files.
    Doctor {
        #[command(flatten)]
        sandbox: SandboxArgs,
    },

    /// Install the runtime waypuppet would use.
    ///
    /// A verb rather than something `run` does on its own: fetching a runtime
    /// means downloading the better part of a gigabyte, and a tool that does
    /// that without being asked is a tool nobody trusts on a tethered
    /// connection.
    Setup {
        /// Install this runtime rather than the one waypuppet would pick.
        #[arg(long, value_name = "ID//BRANCH")]
        runtime: Option<String>,
    },

    /// Start a session and spawn a command inside it.
    ///
    /// Blocks until the client exits unless `--detach` is given. The client's
    /// stdout and stderr are captured, so a program that dies on startup
    /// explains itself instead of vanishing.
    Run {
        /// Output size, as `WIDTHxHEIGHT`.
        #[arg(long, default_value = "1280x800")]
        size: String,
        /// Output scale factor.
        #[arg(long, default_value_t = 1.0)]
        scale: f64,
        /// Render with llvmpipe even where a GPU is available.
        ///
        /// Pixels differ between software and hardware rasterisation, so a
        /// golden image is only comparable against a capture from the same
        /// renderer. CI has no render node, so this is how a developer machine
        /// is made to match it.
        #[arg(long)]
        software: bool,
        /// Leave the session running in the background and return immediately.
        #[arg(long)]
        detach: bool,
        /// Where captures land when `shot` does not name a path.
        #[arg(long, value_name = "DIR")]
        capture_dir: Option<PathBuf>,
        #[command(flatten)]
        sandbox: SandboxArgs,
        /// The command to run, and its arguments.
        #[arg(required = true, last = true)]
        argv: Vec<String>,
    },

    /// List every window the session is hosting.
    Ps,

    /// Capture the session and write it to a PNG.
    Shot {
        /// Capture one window rather than the whole output.
        #[arg(long)]
        window: Option<u32>,
        /// Where to write the image. Defaults to a path in the session's
        /// capture directory, which is printed either way.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },

    /// Print what the client wrote to stdout and stderr.
    Logs {
        /// Keep printing as more arrives, until the client exits.
        #[arg(long, short)]
        follow: bool,
    },

    /// List running sessions.
    Sessions,

    /// Stop the client and shut the session down.
    Kill,

    /// Run a session in the foreground, serving a control socket.
    ///
    /// `run` starts this for you. Reach for it directly when you want the
    /// compositor's own logs on your terminal.
    ///
    /// Deliberately has no `--sandbox`: this is the verb that means "here, now",
    /// and it is what `run` re-executes inside a runtime. Keeping it incapable
    /// of entering one is what makes a session unable to recurse into itself,
    /// rather than relying on the detection to notice.
    Serve {
        /// Output size, as `WIDTHxHEIGHT`.
        #[arg(long, default_value = "1280x800")]
        size: String,
        /// Output scale factor.
        #[arg(long, default_value_t = 1.0)]
        scale: f64,
        /// Render with llvmpipe even where a GPU is available.
        #[arg(long)]
        software: bool,
        /// Exit when the spawned command does.
        #[arg(long)]
        exit_with_child: bool,
        /// Where captures land when `shot` does not name a path.
        #[arg(long, value_name = "DIR")]
        capture_dir: Option<PathBuf>,
        /// An optional command to run inside the session.
        #[arg(last = true)]
        argv: Vec<String>,
    },

    /// Send key chords, such as `ctrl+c`, `Return` or `F5`.
    Key {
        /// Chords to send, in order.
        #[arg(required = true)]
        chords: Vec<String>,
        /// Focus this widget first, by clicking it. See `select --help`.
        #[arg(long, value_name = "SELECTOR")]
        focus_on: Option<String>,
        /// Which match of `--focus-on` to take, zero-based, in tree order.
        #[arg(long, requires = "focus_on", value_name = "N")]
        index: Option<usize>,
    },

    /// Type a string as individual key events.
    Type {
        /// The text to type.
        text: String,
        /// Focus this widget first, by clicking it. See `select --help`.
        #[arg(long, value_name = "SELECTOR")]
        focus_on: Option<String>,
        /// Which match of `--focus-on` to take, zero-based, in tree order.
        #[arg(long, requires = "focus_on", value_name = "N")]
        index: Option<usize>,
    },

    /// Click a pointer button, optionally moving there first.
    Click {
        /// Where to click, as `X,Y`. The pointer stays put if omitted.
        #[arg(long, conflicts_with = "on")]
        at: Option<String>,
        /// Which widget to click, as a selector. See `select --help`.
        #[arg(long, value_name = "SELECTOR")]
        on: Option<String>,
        /// Which match of `--on` to take, zero-based, in tree order.
        #[arg(long, requires = "on", value_name = "N")]
        index: Option<usize>,
        /// Which button.
        #[arg(long, default_value = "left")]
        button: String,
    },

    /// Resolve a selector against the accessibility tree, changing nothing.
    ///
    /// The dry run for `click --on`: it prints where the click would land and
    /// what it matched, so a selector can be tried and narrowed without
    /// touching the application.
    ///
    /// A selector is CSS, restricted to what an accessibility tree can answer:
    ///
    ///   button[name="Save"]        a button with exactly that name, anywhere
    ///   [name*="Save"]             any widget whose name contains it
    ///   dialog > button            a direct child; a space means any descendant
    ///   list-item[name^="Ch 3"]    role names hyphenate — a space is a step
    ///   page-tab:not([name="New"]) exclusion
    ///
    /// Operators are `=`, `*=`, `^=`, `$=`, and a trailing `i` folds case.
    /// Anything CSS has that is missing here is refused by name, with what to
    /// write instead. A selector may also be given as the JSON form of the
    /// query, which is what an agent emitting the structure would send.
    #[command(verbatim_doc_comment)]
    Select {
        /// The selector.
        selector: String,
        /// Which match to take, zero-based, in tree order.
        #[arg(long, value_name = "N")]
        index: Option<usize>,
    },

    /// Move the pointer.
    Move {
        /// Horizontal position.
        x: i32,
        /// Vertical position.
        y: i32,
    },

    /// Scroll by whole wheel clicks.
    Scroll {
        /// Horizontal clicks, positive rightwards.
        #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
        dx: i32,
        /// Vertical clicks, positive downwards.
        #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
        dy: i32,
    },

    /// Resize a window, by reconfiguring it the way a real compositor would.
    Resize {
        /// The new size, as `WIDTHxHEIGHT`.
        size: String,
        /// Which window. Defaults to the focused one.
        #[arg(long)]
        window: Option<u32>,
    },

    /// Block until something is true.
    ///
    /// Every condition is a protocol-level fact the session observed, which is
    /// what makes this different from sleeping and hoping. Exactly one has to
    /// be given.
    ///
    /// Exits 0 when the condition held and 3 when the timeout elapsed first.
    /// Those are different answers: "it did not happen" is an outcome a script
    /// may well be expecting, and it must not look like the 1 that means
    /// waypuppet could not answer at all.
    Wait {
        #[command(flatten)]
        condition: WaitFor,
        /// Give up after this many milliseconds.
        #[arg(long, default_value_t = 5_000)]
        timeout: u64,
    },

    /// Read or write a selection.
    ///
    /// Setting one and letting the application paste is a genuinely different
    /// test from `type`: a terminal distinguishes a paste from keystrokes —
    /// bracketed paste exists for exactly that reason — so both are worth
    /// having.
    Clipboard {
        #[command(subcommand)]
        action: ClipboardAction,
    },

    /// Print the accessibility tree the application publishes.
    ///
    /// With `--selectors`, each line carries the shortest selector that picks
    /// that widget and nothing else — so driving the application is a matter of
    /// copying one rather than composing it.
    ///
    /// A semantic view of the window: roles, names and bounds, rather than
    /// pixels. Toolkits built on AccessKit — freya among them — publish one
    /// without being asked.
    Tree {
        /// Print the shortest selector for each widget alongside it.
        #[arg(long)]
        selectors: bool,
    },
}

/// What to do with a selection.
#[derive(Debug, Subcommand)]
enum ClipboardAction {
    /// Print the selection's contents.
    ///
    /// Prints nothing and exits 0 when nothing owns the selection, which is an
    /// ordinary state of affairs rather than an error.
    Get {
        /// Read the primary selection rather than the clipboard.
        ///
        /// The X11-style one, filled by selecting text and pasted with the
        /// middle button.
        #[arg(long)]
        primary: bool,
    },
    /// Offer text to the application as a selection.
    Set {
        /// Read the text from stdin rather than the command line.
        #[arg(long, conflicts_with = "text")]
        stdin: bool,
        /// Set the primary selection rather than the clipboard.
        #[arg(long)]
        primary: bool,
        /// The text to offer.
        #[arg(required_unless_present = "stdin")]
        text: Option<String>,
    },
}

/// Where a session should run.
///
/// Shared by `run` and `doctor` so the plan `doctor` prints is the plan `run`
/// would follow, rather than an approximation of it. A caller checking why their
/// application cannot see its files needs those two to be the same thing.
#[derive(Debug, Clone, Args)]
struct SandboxArgs {
    /// Which sandbox to host the session in.
    ///
    /// `auto` runs here when this machine can already render and finds a runtime
    /// when it cannot. `flatpak` uses one whether or not it is needed, which is
    /// what an application built against a runtime wants. `none` never leaves
    /// this process.
    #[arg(
        long,
        default_value = "auto",
        value_name = "auto|flatpak|podman|none",
        global = false
    )]
    sandbox: String,
    /// Make another directory visible inside the sandbox. Repeatable.
    ///
    /// The socket, capture, working and command directories are worked out
    /// already; this is for anything else the application reads.
    #[arg(long, value_name = "DIR")]
    sandbox_mount: Vec<PathBuf>,
    /// The container image to use, for `--sandbox podman`.
    ///
    /// There is no default, and there cannot be: no well-known image carries an
    /// arbitrary application's libraries.
    #[arg(long, value_name = "IMAGE")]
    image: Option<String>,
}

impl SandboxArgs {
    /// The options, with `--sandbox` parsed.
    ///
    /// # Errors
    ///
    /// Returns the list of accepted values when `--sandbox` is not one of them.
    fn options(&self) -> Result<sandbox::Options, String> {
        Ok(sandbox::Options {
            sandbox: waypuppet_sandbox::Sandbox::parse(&self.sandbox)?,
            mounts: self.sandbox_mount.clone(),
            image: self.image.clone(),
        })
    }
}

/// What `wait` blocks on.
///
/// A clap group rather than five independent options, so "exactly one" is
/// enforced where the arguments are described instead of by a hand-written
/// check that has to be kept in step with them.
#[derive(Debug, clap::Args)]
#[group(required = true, multiple = false)]
struct WaitFor {
    /// Wait until at least this many windows are mapped.
    ///
    /// The reliable one, and what to reach for after `run --detach`.
    #[arg(long)]
    windows: Option<usize>,
    /// Wait until a mapped window's title matches this regular expression.
    ///
    /// The other reliable one. Anchor it — `^Files$` rather than `Files` — when
    /// the application has more than one window.
    #[arg(long)]
    title: Option<String>,
    /// Wait until the focused window has committed this many further frames.
    ///
    /// Frames arrive whether or not you caused them, so this means "it drew
    /// again", not "the effect is visible". It fails immediately when no window
    /// exists yet; --windows is what that case wants.
    #[arg(long)]
    frames: Option<u32>,
    /// Wait until no client has committed a frame for this many milliseconds.
    ///
    /// A heuristic rather than a fact, and a trap for anything that repaints on
    /// a timer: an application with a spinner, an animation or a blinking
    /// cursor is never idle, so this never fires. Prefer --windows or --title.
    #[arg(long)]
    idle: Option<u64>,
    /// Wait until the client exits.
    #[arg(long)]
    exit: bool,
}

impl WaitFor {
    /// The single condition that was given.
    ///
    /// # Errors
    ///
    /// Returns a message if none was, which clap's group should already have
    /// refused — stated rather than asserted, because a panic is a poor way to
    /// report an argument.
    fn condition(self) -> Result<waypuppet_proto::Condition, String> {
        use waypuppet_proto::Condition;

        if let Some(count) = self.windows {
            return Ok(Condition::Windows(count));
        }
        if let Some(pattern) = self.title {
            return Ok(Condition::TitleMatches(pattern));
        }
        if let Some(frames) = self.frames {
            return Ok(Condition::Frames(frames));
        }
        if let Some(millis) = self.idle {
            return Ok(Condition::Idle(Duration::from_millis(millis)));
        }
        if self.exit {
            return Ok(Condition::Exit);
        }
        Err("wait needs one of --windows, --title, --frames, --idle or --exit".to_owned())
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("WAYPUPPET_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // A CLI verb is one short-lived call, and a session's compositor has its own
    // thread, so neither role needs the multi-threaded scheduler.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("waypuppet: {error}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(dispatch(cli))
}

/// Run whichever verb was asked for.
async fn dispatch(cli: Cli) -> ExitCode {
    let socket = match session::socket_path(&cli.session) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("waypuppet: {error}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = match cli.command {
        Command::Doctor { sandbox } => doctor(&sandbox, &socket, cli.json),
        Command::Setup { runtime } => setup(runtime.as_deref()),
        Command::Serve {
            size,
            scale,
            software,
            exit_with_child,
            capture_dir,
            argv,
        } => {
            serve(
                &socket,
                &size,
                scale,
                software,
                capture_dir,
                &argv,
                exit_with_child,
            )
            .await
        }
        Command::Run {
            size,
            scale,
            software,
            detach,
            capture_dir,
            sandbox,
            argv,
        } => {
            run(
                &cli.session,
                &socket,
                Shape {
                    size: &size,
                    scale,
                    software,
                    capture_dir,
                },
                &sandbox,
                &argv,
                detach,
            )
            .await
        }
        Command::Ps => ps(&socket, cli.json).await,
        Command::Shot { window, output } => shot(&socket, window, output, cli.json).await,
        Command::Logs { follow } => logs(&socket, follow).await,
        Command::Sessions => sessions().await,
        Command::Kill => kill(&socket).await,
        Command::Key {
            chords,
            focus_on,
            index,
        } => key(&socket, chords, focus_on.as_deref(), index).await,
        Command::Type {
            text,
            focus_on,
            index,
        } => write(&socket, text, focus_on.as_deref(), index).await,
        Command::Click {
            at,
            on,
            index,
            button,
        } => click(&socket, at.as_deref(), on.as_deref(), index, &button).await,
        Command::Select { selector, index } => select(&socket, &selector, index, cli.json).await,
        Command::Move { x, y } => pointer(&socket, x, y).await,
        Command::Scroll { dx, dy } => scroll(&socket, dx, dy).await,
        Command::Resize { size, window } => resize(&socket, &size, window).await,
        Command::Wait { condition, timeout } => wait(&socket, condition, timeout, cli.json).await,
        Command::Clipboard { action } => clipboard(&socket, action).await,
        Command::Tree { selectors } => tree(&socket, selectors, cli.json).await,
    };

    match outcome {
        Ok(code) => code,
        Err(message) => {
            eprintln!("waypuppet: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Where captures go when nobody said.
///
/// The session's own directory, beside its socket, rather than `/tmp`. Three
/// reasons, in increasing order of how much trouble they caused:
///
/// - Two sessions both counting from `waypuppet-0000.png` in a shared `/tmp`
///   overwrite each other's captures.
/// - `/tmp` is the one directory every container has its own private copy of, so
///   it is the worst possible default for a path that has to mean the same thing
///   on both sides of a sandbox.
/// - Podman refuses to relabel `/tmp` for `SELinux` at all, and mounting it
///   without relabelling produces a container that cannot read or write it. So
///   `/tmp` as a default did not merely risk confusion; it could not be made to
///   work.
///
/// The socket directory is mounted for the session to be reachable at all, so
/// putting captures inside it costs nothing and removes `/tmp` from the mount
/// list entirely.
fn default_capture_dir(socket: &Path) -> PathBuf {
    socket.with_extension("captures")
}

/// Everything about a session that is fixed for its lifetime.
///
/// Grouped because it has to survive a re-execution intact: `run` may start the
/// session in this process, in a detached copy of this binary, or inside a
/// container runtime, and a capture is only comparable against a previous
/// capture if all three produce the same output.
struct Shape<'a> {
    size: &'a str,
    scale: f64,
    software: bool,
    capture_dir: Option<PathBuf>,
}

impl Shape<'_> {
    /// The `serve` command line that reproduces this shape.
    ///
    /// One list, used by both re-execution paths. They differ in *where* the
    /// binary runs and in nothing else, so building the arguments twice would be
    /// two chances to forget a flag — and the flag most likely to be forgotten
    /// is `--software`, whose absence is invisible until a golden image fails on
    /// somebody else's machine.
    fn serve_args(
        &self,
        name: &str,
        captures: &Path,
        argv: &[String],
        exit_with_child: bool,
    ) -> Vec<String> {
        let mut line = vec![
            "--session".to_owned(),
            name.to_owned(),
            "serve".to_owned(),
            "--size".to_owned(),
            self.size.to_owned(),
            "--scale".to_owned(),
            self.scale.to_string(),
            // Always stated, never inherited. Inside a sandbox the default would
            // be resolved against a `/tmp` that is private to it, and the
            // captures would be written somewhere the caller cannot look.
            "--capture-dir".to_owned(),
            captures.display().to_string(),
        ];
        if self.software {
            line.push("--software".to_owned());
        }
        if exit_with_child {
            line.push("--exit-with-child".to_owned());
        }
        if !argv.is_empty() {
            line.push("--".to_owned());
            line.extend(argv.iter().cloned());
        }
        line
    }
}

/// Run a session in this process.
async fn serve(
    socket: &Path,
    size: &str,
    scale: f64,
    software: bool,
    capture_dir: Option<PathBuf>,
    argv: &[String],
    exit_with_child: bool,
) -> Result<ExitCode, String> {
    let config = config(
        size,
        scale,
        software,
        capture_dir.unwrap_or_else(|| default_capture_dir(socket)),
    )?;
    let exit = session::serve(&config, socket, argv, exit_with_child)
        .await
        .map_err(|error| error.to_string())?;

    // Exit with the child's status, so a session composes with anything that
    // checks exit codes.
    Ok(exit.map_or(ExitCode::SUCCESS, |code| {
        ExitCode::from(u8::try_from(code).unwrap_or(1))
    }))
}

/// Start a session, wherever it turns out to belong.
///
/// This is the verb the whole sandbox exists for. A caller types
/// `waypuppet run -- ./target/debug/my-app` and never learns whether a container
/// was involved; working out whether one is needed, and putting both halves in
/// it, happens here.
async fn run(
    name: &str,
    socket: &Path,
    shape: Shape<'_>,
    where_to: &SandboxArgs,
    argv: &[String],
    detach: bool,
) -> Result<ExitCode, String> {
    let options = where_to.options()?;
    // Resolved before planning, because it is one of the directories the sandbox
    // has to be able to see — and resolved the same way on both sides, so the
    // path `shot` prints means the same thing to whoever reads it.
    let captures = waypuppet_sandbox::settle(
        &shape
            .capture_dir
            .clone()
            .unwrap_or_else(|| default_capture_dir(socket)),
    );
    let facts = sandbox::facts(options.sandbox);
    let plan = sandbox::plan(&options, argv, socket, &captures, &facts)?;

    let reenter = match plan {
        waypuppet_sandbox::Plan::Direct(why) => {
            tracing::debug!(%why, "hosting the session in this process");
            if !detach {
                return serve(
                    socket,
                    shape.size,
                    shape.scale,
                    shape.software,
                    Some(captures),
                    argv,
                    true,
                )
                .await;
            }
            None
        }
        waypuppet_sandbox::Plan::Reenter(reenter) => Some(reenter),
    };

    // A detached session outlives this process either way, so it is always a
    // separate one — re-executing this binary rather than forking, so that there
    // is one code path for how a session starts.
    let serve_args = shape.serve_args(name, &captures, argv, !detach && reenter.is_some());

    if let Some(reenter) = reenter {
        let code = sandbox::start(&reenter, &serve_args, socket, detach).await?;
        if detach {
            println!("session {name} started in {}", reenter.runtime);
        }
        return Ok(code);
    }

    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    std::process::Command::new(exe)
        .args(&serve_args)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;

    // Poll rather than sleep a guessed interval: the process is up before the
    // socket is, and how long that takes depends on the machine.
    client::wait_for_socket(socket, Duration::from_secs(30)).await?;
    println!("session {name} started");
    Ok(ExitCode::SUCCESS)
}

/// List the session's windows.
async fn ps(socket: &Path, json: bool) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    let status = session
        .status(client::deadline(client::DEADLINE))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&status).map_err(|error| error.to_string())?
        );
        return Ok(ExitCode::SUCCESS);
    }

    let client_state = match &status.client {
        ClientState::Starting => "starting".to_owned(),
        ClientState::Running => "running".to_owned(),
        ClientState::Exited { code } => code.map_or_else(
            || "exited (signal)".to_owned(),
            |code| format!("exited {code}"),
        ),
    };
    println!(
        "output {}x{} scale {}  client {client_state}",
        status.output.width, status.output.height, status.scale
    );

    if status.windows.is_empty() {
        println!("no windows");
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "{:<4}{:<24}{:<20}{:<18}FRAMES",
        "ID", "TITLE", "APP_ID", "GEOMETRY"
    );
    for window in &status.windows {
        let geometry = format!(
            "{},{} {}x{}",
            window.geometry.origin.x,
            window.geometry.origin.y,
            window.geometry.size.width,
            window.geometry.size.height
        );
        println!(
            "{:<4}{:<24}{:<20}{:<18}{}",
            window.id.0,
            window.title.as_deref().unwrap_or("-"),
            window.app_id.as_deref().unwrap_or("-"),
            geometry,
            window.frames
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Capture the session.
async fn shot(
    socket: &Path,
    window: Option<u32>,
    output: Option<PathBuf>,
    json: bool,
) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    let target = window.map_or(Target::Output, |id| Target::Window(WindowId(id)));

    // Made absolute, because the path is printed for something else to open and
    // a relative one would be ambiguous about whose working directory it means.
    let output = match output {
        Some(path) if path.is_absolute() => Some(path),
        Some(path) => Some(
            std::env::current_dir()
                .map_err(|error| error.to_string())?
                .join(path),
        ),
        None => None,
    };

    let capture = session
        .shot(client::deadline(client::DEADLINE), target, output)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&capture).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "{}\t{}x{}",
            capture.path.display(),
            capture.size.width,
            capture.size.height
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Print the client's output.
async fn logs(socket: &Path, follow: bool) -> Result<ExitCode, String> {
    use std::io::Write as _;

    let session = client::connect(socket).await?;
    let mut cursor = Cursor::default();

    loop {
        let chunk = session
            .logs(client::deadline(client::DEADLINE), cursor)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|failure| failure.to_string())?;

        print!("{}", chunk.stdout);
        eprint!("{}", chunk.stderr);
        let _ = std::io::stdout().flush();
        cursor = chunk.cursor;

        if !follow || chunk.done {
            return Ok(ExitCode::SUCCESS);
        }
    }
}

// Every verb below returns only once the client has drawn in response, so
// `waypuppet click ... && waypuppet shot ...` needs no sleep between them.
// Success is silent: there is nothing to report but "it happened", and a
// zero exit code already says that.

/// Send key chords, focusing a widget first when asked.
async fn key(
    socket: &Path,
    chords: Vec<String>,
    focus_on: Option<&str>,
    index: Option<usize>,
) -> Result<ExitCode, String> {
    let focus = selector_locator(focus_on, index)?;
    let session = client::connect(socket).await?;
    session
        .key(client::deadline(client::DEADLINE), chords, focus)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| explain(&failure))?;
    Ok(ExitCode::SUCCESS)
}

/// Type a string, focusing a widget first when asked.
async fn write(
    socket: &Path,
    text: String,
    focus_on: Option<&str>,
    index: Option<usize>,
) -> Result<ExitCode, String> {
    let focus = selector_locator(focus_on, index)?;
    let session = client::connect(socket).await?;
    session
        .write(client::deadline(client::DEADLINE), text, focus)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| explain(&failure))?;
    Ok(ExitCode::SUCCESS)
}

/// Click a pointer button.
async fn click(
    socket: &Path,
    at: Option<&str>,
    on: Option<&str>,
    index: Option<usize>,
    button: &str,
) -> Result<ExitCode, String> {
    // `--at` and `--on` are mutually exclusive at the parser, so at most one of
    // these produces anything.
    let locator = match at {
        Some(text) => Some(Locator::At(
            parse_point(text).ok_or_else(|| format!("--at wants X,Y, got {text:?}"))?,
        )),
        None => selector_locator(on, index)?,
    };
    let button = parse_button(button)
        .ok_or_else(|| format!("--button wants left, right or middle, got {button:?}"))?;

    let session = client::connect(socket).await?;
    session
        .click(
            client::deadline(client::DEADLINE),
            locator,
            button,
            Target::Focused,
        )
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| explain(&failure))?;
    Ok(ExitCode::SUCCESS)
}

/// Resolve a selector and print where a click would land, changing nothing.
///
/// The dry run. `click --on` is one call precisely so nothing can move between
/// resolving and clicking, which leaves no way to see what a selector picked —
/// so this exists to be that way, and it is the verb to iterate a selector
/// with.
async fn select(
    socket: &Path,
    selector: &str,
    index: Option<usize>,
    json: bool,
) -> Result<ExitCode, String> {
    let query = parse_selector(selector, index)?;
    let session = client::connect(socket).await?;
    let tree = session
        .tree(client::deadline(client::DEADLINE), Target::Focused)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;

    // Resolved here rather than server side, unlike every other selector path.
    // Nothing is being driven, so there is no race to close, and doing it
    // locally means the verb works against any tree — including one saved from
    // an earlier run, when that becomes worth having.
    let query = waypuppet_select::validate(&query).map_err(|failure| explain(&failure))?;
    let point = waypuppet_select::resolve(&tree, &query).map_err(|failure| explain(&failure))?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "selector": waypuppet_select::print(&query),
                "point": { "x": point.x, "y": point.y },
            })
        );
    } else {
        println!("{},{}", point.x, point.y);
    }
    Ok(ExitCode::SUCCESS)
}

/// Move the pointer.
async fn pointer(socket: &Path, x: i32, y: i32) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    session
        .pointer(
            client::deadline(client::DEADLINE),
            Point { x, y },
            Target::Focused,
        )
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Scroll by wheel clicks.
async fn scroll(socket: &Path, dx: i32, dy: i32) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    session
        .scroll(client::deadline(client::DEADLINE), dx, dy)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Resize a window.
async fn resize(socket: &Path, size: &str, window: Option<u32>) -> Result<ExitCode, String> {
    let size =
        parse_size(size).ok_or_else(|| format!("resize wants WIDTHxHEIGHT, got {size:?}"))?;
    let target = window.map_or(Target::Focused, |id| Target::Window(WindowId(id)));

    let session = client::connect(socket).await?;
    session
        .resize(client::deadline(client::DEADLINE), size, target)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Block until a condition holds, or report that it did not.
///
/// The only verb whose failure to happen is not an error. Everything else here
/// either works or explains itself; `wait` has a third answer, and a script
/// polling for something that may legitimately not occur needs to be able to
/// tell it from a session that fell over.
async fn wait(
    socket: &Path,
    condition: WaitFor,
    timeout: u64,
    json: bool,
) -> Result<ExitCode, String> {
    let condition = condition.condition()?;
    let timeout = Duration::from_millis(timeout);

    let session = client::connect(socket).await?;
    // The deadline sits beyond the timeout rather than replacing it: the
    // session answers an elapsed timeout as a value, and only a session that
    // has stopped answering at all should ever trip the transport.
    let settled = session
        .wait(
            client::deadline(timeout + client::DEADLINE),
            condition,
            timeout,
        )
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;

    if json {
        println!("{}", serde_json::json!({ "settled": settled }));
    }

    Ok(match settled {
        // Silent, like every other verb that worked.
        Settled::Met => ExitCode::SUCCESS,
        Settled::TimedOut => {
            if !json {
                eprintln!("waypuppet: timed out after {}ms", timeout.as_millis());
            }
            ExitCode::from(TIMED_OUT)
        }
    })
}

/// Read or write a selection.
async fn clipboard(socket: &Path, action: ClipboardAction) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;

    match action {
        ClipboardAction::Get { primary } => {
            let text = session
                .clipboard_get(client::deadline(client::DEADLINE), selection(primary))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|failure| failure.to_string())?;

            // Printed without a trailing newline: a selection is the bytes it
            // is, and `$(waypuppet clipboard get)` should give back exactly
            // what was copied. An empty selection prints nothing at all.
            if let Some(text) = text {
                print!("{text}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        }
        // `stdin` is not read here: clap has already enforced that exactly one
        // of it and `text` is present, so the absence of `text` *is* `--stdin`.
        ClipboardAction::Set {
            stdin: _,
            primary,
            text,
        } => {
            let text = text.map_or_else(read_stdin, Ok)?;

            session
                .clipboard_set(client::deadline(client::DEADLINE), selection(primary), text)
                .await
                .map_err(|error| error.to_string())?
                .map_err(|failure| failure.to_string())?;
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// Print the application's accessibility tree.
async fn tree(socket: &Path, selectors: bool, json: bool) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    let root = session
        .tree(client::deadline(client::DEADLINE), Target::Focused)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;

    match (json, selectors) {
        // The nested tree, as the application published it.
        (true, false) => println!(
            "{}",
            serde_json::to_string(&root).map_err(|error| error.to_string())?
        ),
        // Flat, with a selector per entry, because that is the shape the
        // question has when selectors are what is wanted: "what is here, and
        // how do I name it?" is a list, not a tree — and `depth` keeps the
        // structure available to anything that still wants it.
        (true, true) => {
            let mut entries = Vec::new();
            collect_entries(&root, &root, &mut Vec::new(), &mut entries);
            println!(
                "{}",
                serde_json::to_string(&entries).map_err(|error| error.to_string())?
            );
        }
        (false, _) => print_node(&root, &root, &mut Vec::new(), selectors),
    }
    Ok(ExitCode::SUCCESS)
}

/// One node and its children, indented.
///
/// Bounds are printed in the same coordinates `click --at` takes, which is the
/// whole point of having them: a selector that resolves to a rectangle should
/// be one `--at` away from being clicked.
fn print_node<'a>(
    root: &'a waypuppet_proto::Node,
    node: &'a waypuppet_proto::Node,
    ancestors: &mut Vec<&'a waypuppet_proto::Node>,
    selectors: bool,
) {
    let indent = "  ".repeat(ancestors.len());
    let name = node
        .name
        .as_ref()
        .map_or_else(String::new, |name| format!(" {name:?}"));
    let bounds = node.bounds.as_ref().map_or_else(String::new, |bounds| {
        format!(
            "  [{},{} {}x{}]",
            bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height
        )
    });
    let selector = if selectors {
        let unique = waypuppet_select::identify(root, ancestors, node);
        match unique.index {
            Some(index) => format!("  → {} --index {index}", unique.selector),
            None => format!("  → {}", unique.selector),
        }
    } else {
        String::new()
    };
    println!("{indent}{}{name}{bounds}{selector}", node.role);

    ancestors.push(node);
    for child in &node.children {
        print_node(root, child, ancestors, selectors);
    }
    ancestors.pop();
}

/// The flat `--json --selectors` form, in the same document order everything
/// else uses.
fn collect_entries<'a>(
    root: &'a waypuppet_proto::Node,
    node: &'a waypuppet_proto::Node,
    ancestors: &mut Vec<&'a waypuppet_proto::Node>,
    out: &mut Vec<serde_json::Value>,
) {
    let unique = waypuppet_select::identify(root, ancestors, node);
    out.push(serde_json::json!({
        "selector": unique.selector,
        "index": unique.index,
        "depth": ancestors.len(),
        "role": node.role,
        "name": node.name,
        "bounds": node.bounds,
    }));

    ancestors.push(node);
    for child in &node.children {
        collect_entries(root, child, ancestors, out);
    }
    ancestors.pop();
}

/// Everything on stdin, for `clipboard set --stdin`.
///
/// Read whole rather than by lines: a selection is the bytes it is, and a
/// trailing newline that a caller piped in is part of what they meant to copy.
fn read_stdin() -> Result<String, String> {
    use std::io::Read as _;

    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|error| error.to_string())?;
    Ok(buffer)
}

/// Which selection a `--primary` flag names.
fn selection(primary: bool) -> Selection {
    if primary {
        Selection::Primary
    } else {
        Selection::Clipboard
    }
}

/// Stop the session.
async fn kill(socket: &Path) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    session
        .kill(client::deadline(client::DEADLINE))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// List sessions that are actually listening.
async fn sessions() -> Result<ExitCode, String> {
    let directory = session::socket_path("any")
        .map_err(|error| error.to_string())?
        .parent()
        .map(Path::to_path_buf)
        .ok_or("no session directory")?;

    let Ok(entries) = std::fs::read_dir(&directory) else {
        println!("no sessions");
        return Ok(ExitCode::SUCCESS);
    };

    let mut found = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "sock") {
            continue;
        }
        // A socket file proves nothing — a crashed session leaves one behind.
        // Connecting is the only way to tell a live session from a corpse.
        if let (Ok(_), Some(name)) = (
            tokio::net::UnixStream::connect(&path).await,
            path.file_stem(),
        ) {
            println!("{}", name.to_string_lossy());
            found += 1;
        }
    }
    if found == 0 {
        println!("no sessions");
    }
    Ok(ExitCode::SUCCESS)
}

/// Report what waypuppet can render with, and where it would put a session.
///
/// Two questions, and the second is the one that was missing. Getting a path
/// wrong is the likeliest way for the sandbox to fail and the hardest to guess
/// at from the outside — a session that starts, renders, and cannot see the
/// application's files looks like a broken application. So the resolved plan is
/// printed here, without a `--debug` flag to remember, and it is the *same* plan
/// `run` follows rather than a description of one.
///
/// Note what decides the exit code: the plan, not the device list. "Nothing to
/// render with here, and a runtime that supplies it" is a working machine, and
/// reporting it as a failure would be wrong in exactly the case this plan
/// exists to make ordinary.
fn doctor(args: &SandboxArgs, socket: &Path, json: bool) -> Result<ExitCode, String> {
    let options = args.options()?;
    let devices = waypuppet_compositor::probe();
    let renders = devices.as_ref().is_ok_and(|devices| !devices.is_empty());

    let facts = waypuppet_sandbox::examine(renders, options.sandbox);
    let captures = waypuppet_sandbox::settle(&default_capture_dir(socket));
    // No argv: `doctor` is asked before there is a command in mind. Every other
    // mount is derived the same way it would be for a real run, and the
    // command's own directory is the one addition a real run makes.
    let plan = sandbox::plan(&options, &[], socket, &captures, &facts);

    if json {
        return Ok(doctor_json(
            devices.as_deref().unwrap_or_default(),
            &facts,
            plan.as_ref(),
        ));
    }

    match &devices {
        Ok(devices) if devices.is_empty() => println!("render      none"),
        Ok(devices) => {
            for device in devices {
                let path = device
                    .path
                    .as_ref()
                    .map_or_else(|| "(no drm node)".to_owned(), |p| p.display().to_string());
                let kind = if device.software {
                    "software"
                } else {
                    "hardware"
                };
                println!("render      {path}\t{kind}");
            }
        }
        Err(error) => println!("render      EGL unavailable: {error}"),
    }
    println!("enclosure   {}", facts.enclosure);

    match &plan {
        Ok(waypuppet_sandbox::Plan::Direct(why)) => {
            println!("sandbox     none — {why}");
        }
        Ok(waypuppet_sandbox::Plan::Reenter(reenter)) => {
            println!("sandbox     {}", reenter.runtime);
            println!("binary      {}", reenter.exe.display());
            println!("socket      {}", reenter.socket.display());
            println!(
                "render node {}",
                if reenter.dri {
                    "passed in"
                } else {
                    "none to pass"
                }
            );
            for (index, mount) in reenter.mounts.iter().enumerate() {
                let label = if index == 0 { "mounts" } else { "" };
                println!("{label:<12}{}", mount.display());
            }
            println!();
            println!("The command's own directory is added to that list when one is given.");
        }
        Err(problem) => {
            println!("sandbox     nowhere");
            println!();
            println!("waypuppet: {problem}");
        }
    }

    Ok(if plan.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The machine-readable form of the same report.
fn doctor_json(
    devices: &[waypuppet_compositor::RenderDevice],
    facts: &waypuppet_sandbox::Facts,
    plan: Result<&waypuppet_sandbox::Plan, &String>,
) -> ExitCode {
    let rendered: Vec<_> = devices
        .iter()
        .map(|device| serde_json::json!({ "path": device.path, "software": device.software }))
        .collect();

    let sandbox = match plan {
        Ok(waypuppet_sandbox::Plan::Direct(why)) => serde_json::json!({
            "kind": "direct",
            "why": why.to_string(),
        }),
        Ok(waypuppet_sandbox::Plan::Reenter(reenter)) => serde_json::json!({
            "kind": "reenter",
            "runtime": reenter.runtime.to_string(),
            "binary": reenter.exe,
            "socket": reenter.socket,
            "render_node": reenter.dri,
            "mounts": reenter.mounts,
        }),
        Err(problem) => serde_json::json!({ "kind": "none", "problem": problem }),
    };

    println!(
        "{}",
        serde_json::json!({
            "devices": rendered,
            "enclosure": facts.enclosure.to_string(),
            "sandbox": sandbox,
        })
    );
    if plan.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Install the runtime waypuppet would use.
fn setup(runtime: Option<&str>) -> Result<ExitCode, String> {
    sandbox::setup(
        runtime
            .map(waypuppet_sandbox::FlatpakRuntime::parse)
            .as_ref(),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Build a session config from the command line.
fn config(
    size: &str,
    scale: f64,
    software: bool,
    capture_dir: PathBuf,
) -> Result<waypuppet_compositor::Config, String> {
    let output =
        parse_size(size).ok_or_else(|| format!("--size wants WIDTHxHEIGHT, got {size:?}"))?;
    Ok(waypuppet_compositor::Config {
        output,
        scale,
        rendering: if software {
            Rendering::Software
        } else {
            Rendering::Fastest
        },
        capture_dir,
    })
}

/// Parse `WIDTHxHEIGHT`.
fn parse_size(text: &str) -> Option<Size> {
    let (width, height) = text.split_once(['x', 'X'])?;
    Some(Size {
        width: width.trim().parse().ok()?,
        height: height.trim().parse().ok()?,
    })
}

/// Parse a selector into the [`Locator`] a verb carries, if one was given.
///
/// `index` without a selector is refused by clap (`requires`), so this only has
/// to handle "no selector at all".
fn selector_locator(
    selector: Option<&str>,
    index: Option<usize>,
) -> Result<Option<Locator>, String> {
    selector
        .map(|selector| parse_selector(selector, index).map(Locator::On))
        .transpose()
}

/// Parse a selector, in either of the two forms a caller may write it.
///
/// Text is the form for a person at a shell; JSON is the form for something
/// emitting the structure directly, which is what makes an MCP server a thin
/// wrapper over [`Query`] rather than a second grammar to keep in step. They are
/// told apart by the first character, because a selector can never begin with
/// `{` — and the error says which parser ran, so a JSON typo is never reported
/// as a syntax error in a language the caller was not writing.
///
/// [`Query`]: waypuppet_proto::Query
fn parse_selector(selector: &str, index: Option<usize>) -> Result<Query, String> {
    let mut query = if selector.trim_start().starts_with('{') {
        serde_json::from_str::<Query>(selector).map_err(|error| {
            format!(
                "this begins with `{{`, so it was read as a JSON query: {error}\n\n  \
                 help: the shape is {{\"first\":{{\"role\":{{\"matches\":\
                 {{\"op\":\"exact\",\"value\":\"button\",\"case\":\"sensitive\"}}}}}}}}, \
                 or write it as a selector: button[name=\"Save\"]"
            )
        })?
    } else {
        waypuppet_select::parse(selector).map_err(|error| error.to_string())?
    };
    query.index = index;
    Ok(query)
}

/// Render a failure for the terminal.
///
/// Everything but [`Failure::Ambiguous`] says what it needs to in its own
/// `Display`. That one does not, and cannot: it carries every match with the
/// ancestor chain that locates it, and the whole reason for carrying them is so
/// a caller can *see what matched and pick*. Flattening that to "matched 7
/// nodes" would throw away the part that turns a rejection into a next step, so
/// this is where the matches become selectors somebody can paste back.
fn explain(failure: &Failure) -> String {
    use std::fmt::Write;

    let Failure::Ambiguous { count, matches } = failure else {
        return failure.to_string();
    };

    // No cap. A broad selector against a real tree can honestly match dozens,
    // and truncating the list hides exactly the match a caller was looking for.
    let mut out = format!("selector matched {count} nodes; narrow it, or pick one with --index:\n");
    for (index, matched) in matches.iter().enumerate() {
        let _ = write!(out, "\n  [{index}] {}", waypuppet_select::narrowed(matched));
        match matched.bounds {
            Some(bounds) => {
                let _ = write!(
                    out,
                    "\n       at {},{} {}x{}",
                    bounds.origin.x, bounds.origin.y, bounds.size.width, bounds.size.height
                );
            }
            // Visible here rather than as a surprise at click time: an index
            // pointing at this one fails with `MatchedNoBounds`, and saying so
            // now is cheaper than saying it after.
            None => out.push_str("\n       not laid out — nowhere to click"),
        }
    }
    out
}

/// Parse `X,Y`.
fn parse_point(text: &str) -> Option<Point> {
    let (x, y) = text.split_once(',')?;
    Some(Point {
        x: x.trim().parse().ok()?,
        y: y.trim().parse().ok()?,
    })
}

/// Parse a button name.
///
/// A string on the command line, an enum on the wire — the conversion happens
/// once, here, so nothing downstream has to wonder what "mouse1" might mean.
fn parse_button(text: &str) -> Option<Button> {
    match text.trim().to_ascii_lowercase().as_str() {
        "left" | "l" | "1" => Some(Button::Left),
        "right" | "r" | "3" => Some(Button::Right),
        "middle" | "m" | "2" => Some(Button::Middle),
        _ => None,
    }
}
