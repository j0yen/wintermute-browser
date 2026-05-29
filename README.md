# wintermute-browser

A long-running daemon `wm-browser` that exposes a small JSON-over-agorabus
tool interface for the brain: `open`, `read`, `click`, `type`, `back`,
`find`, `screenshot`. Driven by a headed Chromium instance via CDP
(`chromiumoxide` crate). Accessibility snapshot is the canonical read mode;
snapshot is capped at 2000 refs so the brain gets a bounded view of any page.

Part of the wintermute Fleet 2 action layer — lets the brain browse the web
by voice description.

## Tools

| Tool | Args | Returns |
|---|---|---|
| `open` | `{url}` | `{ok, title, url, snapshot_id}` |
| `read` | `{}` | `{snapshot: <a11y-tree-json>, snapshot_id}` |
| `click` | `{ref}` | `{ok}` — `ref` from snapshot |
| `type` | `{ref, text, submit?}` | `{ok}` |
| `back` | `{}` | `{ok, url}` |
| `find` | `{query}` | `{matches: [{ref,role,name}]}` |
| `screenshot` | `{}` | `{path}` — PNG into `/tmp/wm-browser-shots/` |

Requests arrive on agorabus topic `wm.browser.cmd`; replies go on
`wm.browser.reply` with the `cmd_id` echoed.

## Acceptance criteria

1. `wm-browser open https://example.com` returns `{ok:true, title:"Example Domain"}` within 5 s on a warm browser.
2. `wm-browser read` returns a snapshot containing at least the H1 text "Example Domain" with role `heading` and a resolvable ref.
3. `wm-browser find {query:"More information"}` on example.com returns at least one match with role `link` and a usable ref.
4. `wm-browser click {ref}` on the matched link navigates to the target page; subsequent `read` returns the new page's title.
5. `wm-browser type {ref, text, submit:true}` on Google's search box submits and lands on a results page.
6. Brain (wmd) registers `browser.open` etc. as Claude tools — a `recall` log entry confirms the tool was called from a brain turn.
7. Snapshot capped at 2000 refs; on a large page `read` returns `truncated:true` and brain falls back to `find`.
8. Daemon idle timeout: no tool call for 5 min → exits with rc=0 and removes its lockfile.
9. Crash recovery: kill -9 the Chromium subprocess → daemon detects within 2 s, restarts the browser, next tool call succeeds.
10. **[live]** Real user round-trip: jsy says "find me a recipe for chicken soup", brain plans `open → find → click → read`, dialog speaks the summary.

ACs 1–5, 7–9 have passing offline unit tests. AC6 and AC10 require real Chromium + wmd (runtime/live-gated).

## Install

Prerequisites: `chromium` from the Arch repo (system CDP target), `agorabus` daemon running.

```bash
cargo build --release
install -Dm755 target/release/wm-browser ~/.local/bin/wm-browser
```

Start the daemon:
```bash
wm-browser daemon
```

## License

Licensed under either of MIT or Apache-2.0 at your option.
See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
