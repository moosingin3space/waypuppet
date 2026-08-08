//! The `waypuppet` command.
//!
//! One binary, two roles, in the shape of `tmux`: `waypuppet run` starts a
//! session — a headless compositor holding the application under test — and
//! every other verb is a short-lived client of that session's control socket.
//!
//! # Status
//!
//! The command surface below is the design, and it compiles, but only `doctor`
//! does anything yet. See `CLAUDE.md` for the roadmap.

#![forbid(unsafe_code)]

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};

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

    /// Move the pointer.
    Move {
        /// Horizontal position.
        x: i32,
        /// Vertical position.
        y: i32,
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
    /// what makes this different from sleeping and hoping.
    Wait {
        /// Wait until at least this many windows are mapped.
        #[arg(long)]
        windows: Option<usize>,
        /// Wait until a window title matches this regular expression.
        #[arg(long)]
        title: Option<String>,
        /// Wait until this many further frames have been committed.
        #[arg(long)]
        frames: Option<u32>,
        /// Wait until no frame has been committed for this many milliseconds.
        #[arg(long)]
        idle: Option<u64>,
        /// Wait until the client exits.
        #[arg(long)]
        exit: bool,
        /// Give up after this many milliseconds.
        #[arg(long, default_value_t = 5_000)]
        timeout: u64,
    },

    /// Print the accessibility tree the application publishes.
    ///
    /// A semantic view of the window: roles, names and bounds, rather than
    /// pixels. Toolkits built on AccessKit — freya among them — publish one
    /// without being asked.
    Tree,

    /// Print what the client wrote to stdout and stderr.
    Logs {
        /// Keep printing as more arrives.
        #[arg(long, short)]
        follow: bool,
    },

    /// List running sessions.
    Sessions,

    /// Stop the client and shut the session down.
    Kill,

    /// Run the compositor in the foreground, serving a control socket.
    ///
    /// `run` starts this for you. Reach for it directly when you want the
    /// compositor's own logs on your terminal.
    Serve {
        /// Output size, as `WIDTHxHEIGHT`.
        #[arg(long, default_value = "1280x800")]
        size: String,
    },
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

    match cli.command {
        Command::Doctor => doctor(cli.json),
        Command::Serve { size } => serve(&size),
        other => {
            eprintln!(
                "waypuppet: `{}` is designed but not implemented yet — see CLAUDE.md",
                verb(&other)
            );
            ExitCode::FAILURE
        }
    }
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

/// Run the compositor in the foreground.
fn serve(size: &str) -> ExitCode {
    let Some(output) = parse_size(size) else {
        eprintln!("waypuppet: --size wants WIDTHxHEIGHT, got {size:?}");
        return ExitCode::FAILURE;
    };

    let config = waypuppet_compositor::Config {
        output,
        ..waypuppet_compositor::Config::default()
    };

    let mut compositor = match waypuppet_compositor::Compositor::new(&config) {
        Ok(compositor) => compositor,
        Err(error) => {
            eprintln!("waypuppet: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Printed rather than logged: this is the one thing a caller driving the
    // compositor by hand needs, and it should not depend on the log filter.
    println!("WAYLAND_DISPLAY={}", compositor.socket_name());

    if let Err(error) = compositor.run() {
        eprintln!("waypuppet: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Parse `WIDTHxHEIGHT`.
fn parse_size(text: &str) -> Option<waypuppet_proto::Size> {
    let (width, height) = text.split_once(['x', 'X'])?;
    Some(waypuppet_proto::Size {
        width: width.trim().parse().ok()?,
        height: height.trim().parse().ok()?,
    })
}

/// The subcommand's name, for the not-implemented message.
fn verb(command: &Command) -> &'static str {
    match command {
        Command::Doctor => "doctor",
        Command::Run { .. } => "run",
        Command::Ps => "ps",
        Command::Shot { .. } => "shot",
        Command::Key { .. } => "key",
        Command::Type { .. } => "type",
        Command::Move { .. } => "move",
        Command::Click { .. } => "click",
        Command::Scroll { .. } => "scroll",
        Command::Resize { .. } => "resize",
        Command::Wait { .. } => "wait",
        Command::Tree => "tree",
        Command::Logs { .. } => "logs",
        Command::Sessions => "sessions",
        Command::Kill => "kill",
        Command::Serve { .. } => "serve",
    }
}
