//! `wm-browser` — CLI entry point.
//!
//! Two surfaces over the same dispatcher:
//!   * `wm-browser daemon` runs the long-lived agorabus service.
//!   * `wm-browser <tool> …` launches a one-shot headed browser, runs a
//!     single tool, prints the JSON reply, and closes.
//!
//! One-shot tools route through [`daemon::run_tool`] so their argument
//! handling and result shape match the daemon exactly.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;
use wintermute_browser::daemon;
use wintermute_browser::idle::DEFAULT_IDLE_SECS;
use wintermute_browser::protocol::Tool;
use wintermute_browser::session::BrowserSession;

#[derive(Parser)]
#[command(name = "wm-browser", version, about = "Voice-driven web browsing for the wintermute fleet")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the long-lived daemon: subscribe to `wm.browser.cmd`, dispatch
    /// tool calls, publish replies, and exit after `--idle-secs` of
    /// inactivity (PRD AC8).
    Daemon {
        /// Idle timeout in seconds before the daemon exits cleanly.
        #[arg(long, default_value_t = DEFAULT_IDLE_SECS)]
        idle_secs: u64,
    },
    /// Navigate the one-shot browser to a URL.
    Open {
        /// Absolute URL to open.
        url: String,
    },
    /// Print an accessibility snapshot of the current page.
    Read,
    /// Filter the latest snapshot by a text/role query.
    Find {
        /// Case-insensitive query matched against node role and name.
        query: String,
    },
    /// Click the element identified by a snapshot ref.
    Click {
        /// Opaque snapshot ref (e.g. `n12`) from a prior `read`/`find`.
        node_ref: String,
    },
    /// Type text into the element identified by a snapshot ref.
    Type {
        /// Opaque snapshot ref of a textbox.
        node_ref: String,
        /// Text to type.
        text: String,
        /// Press Enter after typing (submit the form).
        #[arg(long)]
        submit: bool,
    },
    /// Navigate back in history.
    Back,
    /// Capture a PNG screenshot of the current page.
    Screenshot,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wm-browser: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Parse args and dispatch. Returns the process exit code; tool failures
/// print a `{"ok":false,…}` envelope and yield [`ExitCode::FAILURE`].
async fn run() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();

    if let Command::Daemon { idle_secs } = cli.command {
        init_tracing();
        daemon::run(idle_secs).await?;
        return Ok(ExitCode::SUCCESS);
    }

    let (tool, args) = to_tool(&cli.command);
    let mut session = BrowserSession::launch().await?;
    let outcome = daemon::run_tool(&mut session, tool, &args).await;
    session.close().await;

    match outcome {
        Ok(result) => {
            println!("{}", serde_json::to_string_pretty(&json!({ "ok": true, "result": result }))?);
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "ok": false, "error": e.to_string() }))?
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Map a one-shot subcommand to its [`Tool`] and JSON argument envelope.
/// `Daemon` is handled separately and never reaches here.
fn to_tool(command: &Command) -> (Tool, Value) {
    match command {
        Command::Open { url } => (Tool::Open, json!({ "url": url })),
        Command::Read => (Tool::Read, Value::Null),
        Command::Find { query } => (Tool::Find, json!({ "query": query })),
        Command::Click { node_ref } => (Tool::Click, json!({ "ref": node_ref })),
        Command::Type { node_ref, text, submit } => {
            (Tool::Type, json!({ "ref": node_ref, "text": text, "submit": submit }))
        }
        Command::Back => (Tool::Back, Value::Null),
        Command::Screenshot => (Tool::Screenshot, Value::Null),
        Command::Daemon { .. } => (Tool::Read, Value::Null),
    }
}

/// Best-effort tracing init for the daemon (honours `RUST_LOG`).
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .try_init();
}
