//! Wire types for the `wm-browser` agorabus interface and the pure
//! accessibility-snapshot data model.
//!
//! Everything in this module is plain data + pure functions: no browser,
//! no I/O. That keeps the command/reply envelopes and the snapshot-cap
//! logic (PRD AC7) unit-testable without launching Chrome.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum number of nodes returned in a single `read` snapshot.
///
/// PRD §3 / AC7: a search-results page can carry 10k+ refs, which would
/// blow the brain's context window. We cap the returned snapshot at this
/// many nodes and set [`Snapshot::truncated`] so the brain falls back to
/// `find` rather than paging the whole tree.
pub const SNAPSHOT_CAP: usize = 2000;

/// The set of browser actions the daemon understands. Mirrors the PRD
/// §2.2 tool table; `from_str` parses the wire `tool` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Navigate to a URL.
    Open,
    /// Return an accessibility snapshot of the current page.
    Read,
    /// Click the element identified by a snapshot `ref`.
    Click,
    /// Type text into the element identified by a snapshot `ref`.
    Type,
    /// Navigate back in history.
    Back,
    /// Filter the latest snapshot by a text/role query.
    Find,
    /// Capture a PNG screenshot of the current page.
    Screenshot,
}

impl Tool {
    /// Parse the wire `tool` string into a [`Tool`].
    ///
    /// Returns `None` for an unknown tool name so the daemon can reply
    /// with a structured error instead of panicking.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "read" => Some(Self::Read),
            "click" => Some(Self::Click),
            "type" => Some(Self::Type),
            "back" => Some(Self::Back),
            "find" => Some(Self::Find),
            "screenshot" => Some(Self::Screenshot),
            _ => None,
        }
    }

    /// The canonical wire name for this tool.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::Click => "click",
            Self::Type => "type",
            Self::Back => "back",
            Self::Find => "find",
            Self::Screenshot => "screenshot",
        }
    }
}

/// Incoming command envelope on topic `wm.browser.cmd`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Command {
    /// Opaque correlation id echoed back on the reply.
    pub cmd_id: String,
    /// Tool name (`open`, `read`, …). Validated via [`Tool::parse`].
    pub tool: String,
    /// Tool-specific arguments. Shape depends on `tool`.
    #[serde(default)]
    pub args: Value,
}

/// Outgoing reply envelope on topic `wm.browser.reply`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reply {
    /// Echoed `cmd_id` from the originating [`Command`].
    pub cmd_id: String,
    /// Whether the tool ran successfully.
    pub ok: bool,
    /// Tool result payload on success; `null` on failure.
    pub result: Option<Value>,
    /// Human-readable error on failure; `null` on success.
    pub error: Option<String>,
}

impl Reply {
    /// Build a success reply for `cmd_id` carrying `result`.
    #[must_use]
    pub fn ok(cmd_id: impl Into<String>, result: Value) -> Self {
        Self {
            cmd_id: cmd_id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error reply for `cmd_id` carrying `error`.
    #[must_use]
    pub fn err(cmd_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            cmd_id: cmd_id.into(),
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}

/// A single accessibility node in a flat snapshot.
///
/// `ref` is an opaque per-snapshot id (`"n0"`, `"n1"`, …); the brain
/// passes it back unchanged to `click`/`type`. Internally the session
/// keeps a `ref -> selector` map (see [`crate::session`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AxNode {
    /// Opaque per-snapshot stable id.
    #[serde(rename = "ref")]
    pub node_ref: String,
    /// ARIA-ish role (`heading`, `link`, `button`, `textbox`, …).
    pub role: String,
    /// Accessible name (visible text / aria-label / alt).
    pub name: String,
    /// Form value where applicable (input value); empty otherwise.
    pub value: String,
    /// Refs of this node's children within the same snapshot.
    pub children_refs: Vec<String>,
}

/// A capped, flat accessibility snapshot of a page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snapshot {
    /// The (possibly capped) node list.
    pub nodes: Vec<AxNode>,
    /// `true` when the original tree exceeded [`SNAPSHOT_CAP`] and was
    /// truncated. The brain should switch to `find` when this is set.
    pub truncated: bool,
}

/// Cap a node list at [`SNAPSHOT_CAP`], reporting whether truncation
/// occurred. Pure function — the backbone of PRD AC7's unit test.
///
/// Returns a [`Snapshot`] whose `nodes.len() <= SNAPSHOT_CAP` and whose
/// `truncated` flag is `true` iff the input exceeded the cap.
#[must_use]
pub fn cap_snapshot(mut nodes: Vec<AxNode>) -> Snapshot {
    let truncated = nodes.len() > SNAPSHOT_CAP;
    if truncated {
        nodes.truncate(SNAPSHOT_CAP);
    }
    Snapshot { nodes, truncated }
}

/// Case-insensitive substring match of `query` against a node's role or
/// name. Pure helper shared by the live `find` tool and its tests.
#[must_use]
pub fn node_matches(node: &AxNode, query: &str) -> bool {
    let q = query.to_lowercase();
    node.name.to_lowercase().contains(&q) || node.role.to_lowercase().contains(&q)
}

/// Filter `nodes` to those matching `query`, mapping each to the
/// `{ref, role, name}` shape the `find` tool returns.
#[must_use]
pub fn find_matches(nodes: &[AxNode], query: &str) -> Vec<Value> {
    nodes
        .iter()
        .filter(|n| node_matches(n, query))
        .map(|n| {
            serde_json::json!({
                "ref": n.node_ref,
                "role": n.role,
                "name": n.name,
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests"
)]
mod tests {
    use super::*;

    fn node(i: usize, role: &str, name: &str) -> AxNode {
        AxNode {
            node_ref: format!("n{i}"),
            role: role.to_string(),
            name: name.to_string(),
            value: String::new(),
            children_refs: vec![],
        }
    }

    #[test]
    fn tool_parse_roundtrips_every_variant() {
        for t in [
            Tool::Open,
            Tool::Read,
            Tool::Click,
            Tool::Type,
            Tool::Back,
            Tool::Find,
            Tool::Screenshot,
        ] {
            assert_eq!(Tool::parse(t.as_str()), Some(t));
        }
        assert_eq!(Tool::parse("nope"), None);
    }

    #[test]
    fn command_serde_roundtrip() {
        let cmd = Command {
            cmd_id: "c-1".into(),
            tool: "open".into(),
            args: serde_json::json!({ "url": "https://example.com" }),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cmd);
    }

    #[test]
    fn command_defaults_args_to_null_when_absent() {
        let back: Command = serde_json::from_str(r#"{"cmd_id":"c","tool":"read"}"#).unwrap();
        assert_eq!(back.args, Value::Null);
    }

    #[test]
    fn reply_ok_and_err_shapes() {
        let ok = Reply::ok("c-2", serde_json::json!({"ok": true}));
        assert!(ok.ok);
        assert!(ok.error.is_none());
        assert_eq!(ok.result.unwrap()["ok"], true);

        let err = Reply::err("c-3", "boom");
        assert!(!err.ok);
        assert!(err.result.is_none());
        assert_eq!(err.error.unwrap(), "boom");
    }

    #[test]
    fn reply_serde_roundtrip() {
        let r = Reply::ok("c-4", serde_json::json!({"title": "Example Domain"}));
        let json = serde_json::to_string(&r).unwrap();
        let back: Reply = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn axnode_serde_uses_ref_key() {
        let n = node(0, "heading", "Example Domain");
        let json = serde_json::to_string(&n).unwrap();
        assert!(json.contains("\"ref\":\"n0\""), "json was {json}");
        let back: AxNode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
    }

    // AC7: cap logic — over the cap truncates and flags; under does not.
    #[test]
    fn cap_snapshot_truncates_over_cap_and_sets_flag() {
        let nodes: Vec<AxNode> = (0..SNAPSHOT_CAP + 500)
            .map(|i| node(i, "generic", "x"))
            .collect();
        let snap = cap_snapshot(nodes);
        assert_eq!(snap.nodes.len(), SNAPSHOT_CAP);
        assert!(snap.truncated);
    }

    #[test]
    fn cap_snapshot_under_cap_leaves_untruncated() {
        let nodes: Vec<AxNode> = (0..SNAPSHOT_CAP - 1)
            .map(|i| node(i, "generic", "x"))
            .collect();
        let snap = cap_snapshot(nodes);
        assert_eq!(snap.nodes.len(), SNAPSHOT_CAP - 1);
        assert!(!snap.truncated);
    }

    #[test]
    fn cap_snapshot_exactly_at_cap_not_truncated() {
        let nodes: Vec<AxNode> = (0..SNAPSHOT_CAP).map(|i| node(i, "generic", "x")).collect();
        let snap = cap_snapshot(nodes);
        assert_eq!(snap.nodes.len(), SNAPSHOT_CAP);
        assert!(!snap.truncated);
    }

    #[test]
    fn node_matches_is_case_insensitive_over_role_and_name() {
        let n = node(0, "Link", "More Information");
        assert!(node_matches(&n, "more information"));
        assert!(node_matches(&n, "INFORMATION"));
        assert!(node_matches(&n, "link"));
        assert!(!node_matches(&n, "checkout"));
    }

    #[test]
    fn find_matches_projects_ref_role_name() {
        let nodes = vec![
            node(0, "link", "More information..."),
            node(1, "heading", "Example Domain"),
            node(2, "link", "Contact us"),
        ];
        let hits = find_matches(&nodes, "more");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["ref"], "n0");
        assert_eq!(hits[0]["role"], "link");
        assert_eq!(hits[0]["name"], "More information...");
    }
}
