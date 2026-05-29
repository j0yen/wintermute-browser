//! `wintermute-browser` — voice-driven web browsing for the wintermute
//! fleet.
//!
//! The crate exposes a long-running daemon ([`daemon::run`]) that speaks a
//! small JSON tool protocol over agorabus (`open`, `read`, `click`,
//! `type`, `back`, `find`, `screenshot`) and drives a headed Chromium via
//! CDP ([`session::BrowserSession`]). The wire envelopes and the
//! accessibility-snapshot model are pure data in [`protocol`]; crash
//! recovery (PRD AC9) and the idle timeout (PRD AC8) are factored into
//! [`recovery`] and [`idle`] so they unit-test without a live browser.
//!
//! The `wm-browser` binary (`src/main.rs`) is a thin CLI over these
//! modules: `wm-browser daemon` runs the long-lived service, while the
//! per-tool subcommands (`wm-browser open <url>`, …) launch a one-shot
//! browser and route through the *same* [`daemon::run_tool`] dispatcher
//! the daemon uses, so the two surfaces share identical semantics.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![warn(missing_docs)]

pub mod daemon;
pub mod idle;
pub mod protocol;
pub mod recovery;
pub mod session;
