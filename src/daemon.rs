//! The long-running `wm-browser daemon`.
//!
//! Subscribes to `wm.browser.` on agorabus, lazily launches Chromium on
//! the first command, dispatches each [`Command`] to a [`BrowserSession`],
//! and publishes a [`Reply`] on `wm.browser.reply`. A separate publisher
//! connection mirrors the canonical `wintermute-dialog` pattern (reading
//! and writing on the same subscribed socket would interleave broadcast
//! events with reply lines).
//!
//! Idle timeout (PRD AC8) and the bus socket override are wired here; the
//! pure pieces live in [`crate::idle`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Instant;

use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::idle::{default_lock_path, remove_lock, write_lock, IdleTimer};
use crate::protocol::{Command, Reply, Tool};
use crate::session::BrowserSession;

/// agorabus topic the daemon listens on.
pub const TOPIC_CMD: &str = "wm.browser.cmd";
/// agorabus topic the daemon publishes replies on.
pub const TOPIC_REPLY: &str = "wm.browser.reply";
/// Subscribe prefix covering the command topic.
pub const SUBSCRIBE_PREFIX: &str = "wm.browser.";

/// Resolve the bus socket: `WM_BROWSER_BUS_SOCKET` override then
/// [`agorabus::default_socket_path`].
#[must_use]
pub fn bus_socket() -> PathBuf {
    std::env::var("WM_BROWSER_BUS_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| agorabus::default_socket_path())
}

/// Execute one decoded command against `session`, launching the browser
/// lazily if it is not yet up. Returns the [`Reply`] to publish.
///
/// The browser handle lives in an `Option`; the first command that needs
/// it triggers [`BrowserSession::launch`]. Launch failure yields an
/// error reply rather than crashing the daemon.
pub async fn handle_command(session: &mut Option<BrowserSession>, cmd: Command) -> Reply {
    let Some(tool) = Tool::parse(&cmd.tool) else {
        return Reply::err(&cmd.cmd_id, format!("unknown tool: {}", cmd.tool));
    };
    if session.is_none() {
        match BrowserSession::launch().await {
            Ok(s) => *session = Some(s),
            Err(e) => return Reply::err(&cmd.cmd_id, format!("browser launch failed: {e}")),
        }
    }
    let Some(s) = session.as_mut() else {
        return Reply::err(&cmd.cmd_id, "browser unavailable");
    };
    match run_tool(s, tool, &cmd.args).await {
        Ok(result) => Reply::ok(&cmd.cmd_id, result),
        Err(e) => Reply::err(&cmd.cmd_id, e.to_string()),
    }
}

/// Dispatch a parsed [`Tool`] with its raw `args` against a live session.
///
/// Argument extraction is centralised here so both the daemon and the
/// one-shot CLI route through identical semantics.
///
/// # Errors
/// Propagates missing-argument and tool-execution failures.
pub async fn run_tool(session: &mut BrowserSession, tool: Tool, args: &Value) -> Result<Value> {
    match tool {
        Tool::Open => {
            let url = str_arg(args, "url")?;
            session.open(&url).await
        }
        Tool::Read => session.read().await,
        Tool::Find => {
            let query = str_arg(args, "query")?;
            session.find(&query).await
        }
        Tool::Click => {
            let r = str_arg(args, "ref")?;
            session.click(&r).await
        }
        Tool::Type => {
            let r = str_arg(args, "ref")?;
            let text = str_arg(args, "text")?;
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
            session.type_text(&r, &text, submit).await
        }
        Tool::Back => session.back().await,
        Tool::Screenshot => session.screenshot().await,
    }
}

/// Extract a required string argument `key` from `args`.
fn str_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .with_context(|| format!("missing required arg `{key}`"))
}

/// Run the daemon to completion.
///
/// Connects to agorabus (a missing bus is *not* an error — logs and
/// exits rc=0, same as the fleet's other daemons), subscribes to
/// [`SUBSCRIBE_PREFIX`], writes its lockfile, then loops over bus events
/// and the idle deadline until either fires. On any clean exit the
/// lockfile is removed (PRD AC8).
///
/// # Errors
/// Propagates fatal agorabus I/O failures.
pub async fn run(idle_secs: u64) -> Result<()> {
    let lock_path = default_lock_path();
    let sock = bus_socket();

    let Some(mut sub_client) = agorabus::Client::try_connect(&sock).await? else {
        warn!(socket = %sock.display(), "wm-browser: agorabus not reachable; exiting");
        return Ok(());
    };
    let pid = std::process::id();
    sub_client
        .announce(
            &format!("wm-browser-{pid}-sub"),
            pid,
            "",
            "wm-browser command subscribe",
        )
        .await?;
    sub_client.subscribe(SUBSCRIBE_PREFIX).await?;

    let mut pub_client = agorabus::Client::connect(&sock).await?;
    pub_client
        .announce(&format!("wm-browser-{pid}"), pid, "", "wm-browser reply path")
        .await?;

    write_lock(&lock_path, pid).context("write lockfile")?;
    info!(
        socket = %sock.display(),
        lock = %lock_path.display(),
        idle_secs,
        "wm-browser: daemon up"
    );

    let result = serve_loop(&mut sub_client, &mut pub_client, idle_secs).await;

    // Clean exit: drop the lockfile regardless of how the loop ended.
    if let Err(e) = remove_lock(&lock_path) {
        warn!(error = %e, "wm-browser: failed to remove lockfile on exit");
    }
    result
}

/// The select loop: bus events vs. idle deadline. Factored out of
/// [`run`] so the lockfile teardown wraps every exit path.
async fn serve_loop(
    sub_client: &mut agorabus::Client,
    pub_client: &mut agorabus::Client,
    idle_secs: u64,
) -> Result<()> {
    let mut session: Option<BrowserSession> = None;
    let mut idle = IdleTimer::from_secs(idle_secs, Instant::now());

    loop {
        let now = Instant::now();
        let remaining = idle.remaining(now);
        tokio::select! {
            ev = sub_client.next_event() => {
                match ev {
                    Ok(Some(ev)) => {
                        if ev.topic != TOPIC_CMD {
                            continue;
                        }
                        idle.touch(Instant::now());
                        let cmd: Command = match serde_json::from_value(ev.data.clone()) {
                            Ok(c) => c,
                            Err(e) => {
                                warn!(error = %e, "wm-browser: undecodable command; skipping");
                                continue;
                            }
                        };
                        let cmd_id = cmd.cmd_id.clone();
                        let reply = handle_command(&mut session, cmd).await;
                        let payload = match serde_json::to_value(&reply) {
                            Ok(v) => v,
                            Err(e) => {
                                error!(error = %e, "wm-browser: reply serialise failed");
                                continue;
                            }
                        };
                        if let Err(e) = pub_client.publish(TOPIC_REPLY, payload).await {
                            error!(cmd_id = %cmd_id, error = %e, "wm-browser: publish reply failed");
                        }
                        idle.touch(Instant::now());
                    }
                    Ok(None) => {
                        info!("wm-browser: bus closed; daemon exiting");
                        break;
                    }
                    Err(e) => {
                        error!(error = %e, "wm-browser: subscribe read failed; exiting");
                        break;
                    }
                }
            }
            () = sleep(remaining) => {
                if idle.is_expired(Instant::now()) {
                    info!("wm-browser: idle timeout reached; exiting rc=0");
                    break;
                }
            }
        }
    }

    if let Some(s) = session {
        s.close().await;
    }
    Ok(())
}

/// Path of the daemon's lockfile (re-exported for the CLI / tests).
#[must_use]
pub fn lock_path() -> PathBuf {
    default_lock_path()
}

/// Convenience: whether a lockfile exists at `path` (CLI status helper).
#[must_use]
pub fn lock_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "tests")]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_tool_yields_error_reply_without_launching() {
        let mut session: Option<BrowserSession> = None;
        let cmd = Command {
            cmd_id: "c-1".into(),
            tool: "frobnicate".into(),
            args: Value::Null,
        };
        let reply = handle_command(&mut session, cmd).await;
        assert!(!reply.ok);
        assert_eq!(reply.cmd_id, "c-1");
        assert!(reply.error.unwrap().contains("unknown tool"));
        // No browser was launched for an unknown tool.
        assert!(session.is_none());
    }

    #[test]
    fn str_arg_extracts_and_errors() {
        let args = serde_json::json!({ "url": "https://example.com" });
        assert_eq!(str_arg(&args, "url").unwrap(), "https://example.com");
        assert!(str_arg(&args, "missing").is_err());
    }

    #[test]
    fn bus_socket_honours_override() {
        // Without the env var set we just get a non-empty default path.
        let p = bus_socket();
        assert!(!p.as_os_str().is_empty());
    }

    #[test]
    fn lock_exists_reflects_filesystem() {
        let path = std::env::temp_dir().join(format!("wm-browser-test-lock-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(!lock_exists(&path));
        std::fs::write(&path, "1").unwrap();
        assert!(lock_exists(&path));
        let _ = std::fs::remove_file(&path);
    }
}
