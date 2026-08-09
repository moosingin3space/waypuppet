//! The `waypuppet` command.
//!
//! One binary, two roles, in the shape of `tmux`: `waypuppet run` starts a
//! session — a headless compositor holding the application under test — and
//! every other verb is a short-lived client of that session's control socket.
//!
//! # Status
//!
//! Every verb works: `doctor`, `serve`, `run`, `ps`, `shot`, `logs`, `kill`,
//! `sessions`, `wait`, `clipboard`, `tree`, and the input verbs `key`, `type`,
//! `click`, `move`, `scroll` and `resize`.
//!
//! What is left is selectors — naming a widget from the accessibility tree
//! instead of a coordinate — which is the rest of `plans/07`.

mod client;

use std::{path::Path, path::PathBuf, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};
use waypuppet_compositor::{Rendering, session};
use waypuppet_proto::{
    Button, ClientState, Cursor, Point, Selection, Settled, Size, Target, WindowId,
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
    /// Report what waypuppet can render with, and why not if it cannot.
    ///
    /// Run this first in any new container. It is the fastest way to tell a
    /// missing render node apart from a missing library.
    Doctor,

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
        /// An optional command to run inside the session.
        #[arg(last = true)]
        argv: Vec<String>,
    },

    /// Send key chords, such as `ctrl+c`, `Return` or `F5`.
    Key {
        /// Chords to send, in order.
        #[arg(required = true)]
        chords: Vec<String>,
    },

    /// Type a string as individual key events.
    Type {
        /// The text to type.
        text: String,
    },

    /// Click a pointer button, optionally moving there first.
    Click {
        /// Where to click, as `X,Y`. The pointer stays put if omitted.
        #[arg(long)]
        at: Option<String>,
        /// Which button.
        #[arg(long, default_value = "left")]
        button: String,
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
    /// A semantic view of the window: roles, names and bounds, rather than
    /// pixels. Toolkits built on AccessKit — freya among them — publish one
    /// without being asked.
    Tree,
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

    if matches!(cli.command, Command::Doctor) {
        return doctor(cli.json);
    }

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
        Command::Doctor => return doctor(cli.json),
        Command::Serve {
            size,
            scale,
            software,
            exit_with_child,
            argv,
        } => serve(&socket, &size, scale, software, &argv, exit_with_child).await,
        Command::Run {
            size,
            scale,
            software,
            detach,
            argv,
        } => run(&cli.session, &socket, &size, scale, software, &argv, detach).await,
        Command::Ps => ps(&socket, cli.json).await,
        Command::Shot { window, output } => shot(&socket, window, output, cli.json).await,
        Command::Logs { follow } => logs(&socket, follow).await,
        Command::Sessions => sessions().await,
        Command::Kill => kill(&socket).await,
        Command::Key { chords } => key(&socket, chords).await,
        Command::Type { text } => write(&socket, text).await,
        Command::Click { at, button } => click(&socket, at.as_deref(), &button).await,
        Command::Move { x, y } => pointer(&socket, x, y).await,
        Command::Scroll { dx, dy } => scroll(&socket, dx, dy).await,
        Command::Resize { size, window } => resize(&socket, &size, window).await,
        Command::Wait { condition, timeout } => wait(&socket, condition, timeout, cli.json).await,
        Command::Clipboard { action } => clipboard(&socket, action).await,
        Command::Tree => tree(&socket, cli.json).await,
    };

    match outcome {
        Ok(code) => code,
        Err(message) => {
            eprintln!("waypuppet: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Run a session in this process.
async fn serve(
    socket: &Path,
    size: &str,
    scale: f64,
    software: bool,
    argv: &[String],
    exit_with_child: bool,
) -> Result<ExitCode, String> {
    let config = config(size, scale, software)?;
    let exit = session::serve(&config, socket, argv, exit_with_child)
        .await
        .map_err(|error| error.to_string())?;

    // Exit with the child's status, so a session composes with anything that
    // checks exit codes.
    Ok(exit.map_or(ExitCode::SUCCESS, |code| {
        ExitCode::from(u8::try_from(code).unwrap_or(1))
    }))
}

/// Start a session, in this process or the background.
async fn run(
    name: &str,
    socket: &Path,
    size: &str,
    scale: f64,
    software: bool,
    argv: &[String],
    detach: bool,
) -> Result<ExitCode, String> {
    if !detach {
        return serve(socket, size, scale, software, argv, true).await;
    }

    // Detaching re-executes this binary as a session, rather than forking, so
    // there is one code path for how a session starts.
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--session")
        .arg(name)
        .arg("serve")
        .arg("--size")
        .arg(size)
        .arg("--scale")
        .arg(scale.to_string())
        .stdin(std::process::Stdio::null());
    // Forwarded, or a detached session would quietly render on the GPU while
    // the foreground one rendered on llvmpipe — the two producing different
    // pixels is the whole reason the flag exists.
    if software {
        command.arg("--software");
    }
    if !argv.is_empty() {
        command.arg("--").args(argv);
    }
    command.spawn().map_err(|error| error.to_string())?;

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

/// Send key chords.
async fn key(socket: &Path, chords: Vec<String>) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    session
        .key(client::deadline(client::DEADLINE), chords)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Type a string.
async fn write(socket: &Path, text: String) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    session
        .write(client::deadline(client::DEADLINE), text)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Click a pointer button.
async fn click(socket: &Path, at: Option<&str>, button: &str) -> Result<ExitCode, String> {
    let at = match at {
        Some(text) => {
            Some(parse_point(text).ok_or_else(|| format!("--at wants X,Y, got {text:?}"))?)
        }
        None => None,
    };
    let button = parse_button(button)
        .ok_or_else(|| format!("--button wants left, right or middle, got {button:?}"))?;

    let session = client::connect(socket).await?;
    session
        .click(
            client::deadline(client::DEADLINE),
            at,
            button,
            Target::Focused,
        )
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;
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
async fn tree(socket: &Path, json: bool) -> Result<ExitCode, String> {
    let session = client::connect(socket).await?;
    let root = session
        .tree(client::deadline(client::DEADLINE), Target::Focused)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|failure| failure.to_string())?;

    if json {
        println!(
            "{}",
            serde_json::to_string(&root).map_err(|error| error.to_string())?
        );
    } else {
        print_node(&root, 0);
    }
    Ok(ExitCode::SUCCESS)
}

/// One node and its children, indented.
///
/// Bounds are printed in the same coordinates `click --at` takes, which is the
/// whole point of having them: a selector that resolves to a rectangle should
/// be one `--at` away from being clicked.
fn print_node(node: &waypuppet_proto::Node, depth: usize) {
    let indent = "  ".repeat(depth);
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
    println!("{indent}{}{name}{bounds}", node.role);
    for child in &node.children {
        print_node(child, depth + 1);
    }
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

/// Report the rendering devices available, or explain their absence.
fn doctor(json: bool) -> ExitCode {
    let devices = match waypuppet_compositor::probe() {
        Ok(devices) => devices,
        Err(error) => {
            eprintln!("waypuppet: cannot initialise EGL: {error}");
            return ExitCode::FAILURE;
        }
    };

    if json {
        let rendered: Vec<_> = devices
            .iter()
            .map(|device| serde_json::json!({ "path": device.path, "software": device.software }))
            .collect();
        println!("{}", serde_json::json!({ "devices": rendered }));
    } else if devices.is_empty() {
        println!("no EGL devices");
        println!();
        println!("The container has no render node. Pass --device=dri to flatpak run,");
        println!("or --device /dev/dri to podman run.");
    } else {
        for device in &devices {
            let path = device
                .path
                .as_ref()
                .map_or_else(|| "(no drm node)".to_owned(), |p| p.display().to_string());
            let kind = if device.software {
                "software"
            } else {
                "hardware"
            };
            println!("{path}\t{kind}");
        }
    }

    if devices.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Build a session config from the command line.
fn config(size: &str, scale: f64, software: bool) -> Result<waypuppet_compositor::Config, String> {
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
        ..waypuppet_compositor::Config::default()
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
