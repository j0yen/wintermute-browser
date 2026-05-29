//! Idle-timeout bookkeeping and lockfile management.
//!
//! Both pieces are pure / filesystem-only so PRD AC8 ("no tool call for
//! 5 min → exit rc=0 and remove the lockfile") is unit-testable without
//! launching Chrome. [`IdleTimer`] is a deadline tracker driven by a
//! monotonic [`Instant`] clock; the daemon ticks it and exits when
//! [`IdleTimer::is_expired`] returns `true`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Default idle timeout: 5 minutes (PRD AC8).
pub const DEFAULT_IDLE_SECS: u64 = 300;

/// Tracks time since the last activity and reports when the idle window
/// has elapsed.
///
/// Pure logic over an injectable [`Instant`] "now" so tests can drive it
/// with a tiny window and a hand-rolled clock instead of sleeping.
#[derive(Debug, Clone)]
pub struct IdleTimer {
    /// How long without activity before [`is_expired`](Self::is_expired)
    /// flips to `true`.
    timeout: Duration,
    /// Instant of the most recent [`touch`](Self::touch).
    last_activity: Instant,
}

impl IdleTimer {
    /// Construct a timer with `timeout`, starting the clock at `now`.
    #[must_use]
    pub const fn new(timeout: Duration, now: Instant) -> Self {
        Self {
            timeout,
            last_activity: now,
        }
    }

    /// Convenience constructor from a seconds count, anchored at `now`.
    #[must_use]
    pub fn from_secs(secs: u64, now: Instant) -> Self {
        Self::new(Duration::from_secs(secs), now)
    }

    /// Record activity at `now`, resetting the idle window.
    pub fn touch(&mut self, now: Instant) {
        self.last_activity = now;
    }

    /// Whether the idle window has elapsed as of `now`.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_activity) >= self.timeout
    }

    /// Remaining time before expiry as of `now` (zero once expired).
    /// The daemon uses this to size its `select!` sleep so it wakes up
    /// exactly at the deadline rather than busy-polling.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Duration {
        self.timeout.saturating_sub(now.duration_since(self.last_activity))
    }

    /// The configured timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Resolve the lockfile path: `$XDG_RUNTIME_DIR/wm-browser/wm-browser.lock`
/// with a `/tmp/wm-browser-<uid>/wm-browser.lock` fallback when no
/// runtime dir is set.
#[must_use]
pub fn default_lock_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg)
                .join("wm-browser")
                .join("wm-browser.lock");
        }
    }
    let uid = uid_from_proc();
    Path::new("/tmp")
        .join(format!("wm-browser-{uid}"))
        .join("wm-browser.lock")
}

/// Read this process's UID from `/proc/self/status` without pulling in
/// `libc` (Linux-only target, same idiom as `wintermute-dialog`).
fn uid_from_proc() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            if let Some(first) = rest.split_whitespace().next() {
                if let Ok(uid) = first.parse::<u32>() {
                    return uid;
                }
            }
        }
    }
    0
}

/// Create the lockfile at `path`, writing the current PID. Creates the
/// parent directory if missing.
///
/// # Errors
/// Propagates filesystem failures (permission, ENOSPC, …).
pub fn write_lock(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create lock dir {}", parent.display()))?;
    }
    std::fs::write(path, format!("{pid}\n"))
        .with_context(|| format!("write lockfile {}", path.display()))?;
    Ok(())
}

/// Remove the lockfile at `path`. Absent file is treated as success so
/// clean-exit teardown is idempotent (PRD AC8).
///
/// # Errors
/// Propagates filesystem failures other than `NotFound`.
pub fn remove_lock(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove lockfile {}", path.display())),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod tests {
    use super::*;

    // AC8: a tiny idle window fires once the clock advances past it.
    #[test]
    fn idle_timer_expires_after_window() {
        let t0 = Instant::now();
        let timer = IdleTimer::new(Duration::from_millis(50), t0);
        assert!(!timer.is_expired(t0));
        assert!(!timer.is_expired(t0 + Duration::from_millis(49)));
        assert!(timer.is_expired(t0 + Duration::from_millis(50)));
        assert!(timer.is_expired(t0 + Duration::from_millis(200)));
    }

    #[test]
    fn idle_timer_touch_resets_window() {
        let t0 = Instant::now();
        let mut timer = IdleTimer::new(Duration::from_millis(50), t0);
        let t1 = t0 + Duration::from_millis(40);
        timer.touch(t1);
        // 40ms after touch is still inside the window.
        assert!(!timer.is_expired(t1 + Duration::from_millis(40)));
        // 50ms after touch trips it.
        assert!(timer.is_expired(t1 + Duration::from_millis(50)));
    }

    #[test]
    fn idle_timer_remaining_counts_down_to_zero() {
        let t0 = Instant::now();
        let timer = IdleTimer::new(Duration::from_secs(10), t0);
        assert_eq!(timer.remaining(t0), Duration::from_secs(10));
        assert_eq!(
            timer.remaining(t0 + Duration::from_secs(3)),
            Duration::from_secs(7)
        );
        assert_eq!(
            timer.remaining(t0 + Duration::from_secs(20)),
            Duration::ZERO
        );
    }

    #[test]
    fn from_secs_matches_new() {
        let t0 = Instant::now();
        let a = IdleTimer::from_secs(300, t0);
        assert_eq!(a.timeout(), Duration::from_secs(300));
    }

    // AC8: lockfile write/remove round-trip + idempotent removal.
    #[test]
    fn lockfile_write_then_remove_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "wm-browser-lock-test-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let path = dir.join("nested").join("wm-browser.lock");
        write_lock(&path, 4242).expect("write lock");
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).expect("read lock");
        assert_eq!(body.trim(), "4242");

        remove_lock(&path).expect("remove lock");
        assert!(!path.exists());
        // Idempotent: removing again is fine.
        remove_lock(&path).expect("remove absent lock");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_lock_path_ends_in_wm_browser_lock() {
        let p = default_lock_path();
        assert_eq!(
            p.file_name().and_then(|f| f.to_str()),
            Some("wm-browser.lock")
        );
        assert!(p.parent().is_some());
    }
}
