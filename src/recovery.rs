//! Crash-detection predicate for the Chrome subprocess (PRD AC9).
//!
//! When `kill -9` takes out the Chromium child, chromiumoxide page
//! operations start failing with connection/transport errors and the
//! handler stream ends. The session relaunches the browser before
//! serving the next command. The decision of *whether a given error
//! warrants a relaunch* is factored here as a pure predicate so it can
//! be unit-tested without a live browser.

/// Whether an error string from a page/browser operation indicates the
/// browser connection is dead and a relaunch is warranted.
///
/// We match on the lowercased message because chromiumoxide surfaces
/// transport failures through several error variants (channel closed,
/// websocket closed, the launched process exiting) whose `Display`
/// strings share these substrings.
#[must_use]
pub fn is_connection_lost(err_msg: &str) -> bool {
    let m = err_msg.to_lowercase();
    const NEEDLES: [&str; 8] = [
        "connection closed",
        "channel closed",
        "websocket",
        "ws closed",
        "sender was dropped",
        "receiver was dropped",
        "broken pipe",
        "browser process",
    ];
    NEEDLES.iter().any(|needle| m.contains(needle))
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn connection_lost_detects_transport_failures() {
        assert!(is_connection_lost("the connection closed unexpectedly"));
        assert!(is_connection_lost("WebSocket error: closed"));
        assert!(is_connection_lost("channel closed"));
        assert!(is_connection_lost("the sender was dropped"));
        assert!(is_connection_lost("Broken pipe (os error 32)"));
        assert!(is_connection_lost("browser process exited"));
    }

    #[test]
    fn connection_lost_ignores_ordinary_errors() {
        assert!(!is_connection_lost("element not found: #search"));
        assert!(!is_connection_lost("invalid url"));
        assert!(!is_connection_lost("ref n7 not present in latest snapshot"));
        assert!(!is_connection_lost("navigation timed out"));
    }
}
