# wintermute-browser

A daemon that lets the wintermute brain browse the web by description — `open`, `read`, `find`, `click`, `type`, `back`, `screenshot` — driving a real Chromium over CDP and handing the brain a bounded accessibility view of the page.

## Why it exists

An LLM browsing the web has two bad options: scrape raw HTML and drown in markup, or take a screenshot and guess at coordinates. Both burn context and miss interactive elements. The third option is the accessibility tree — the same structured view a screen reader uses — where every link, button, and textbox already carries a role and a name. That is the read mode here. The brain works on a flat list of `{ref, role, name}` nodes and refers back to elements by an opaque `ref`; it never sees a pixel or a CSS selector.

The remaining problem is size. A search-results page can carry ten thousand nodes, enough to blow the context window on a single `read`. So the snapshot is capped at 2000 refs and flagged `truncated` when it overflows, and the brain switches to `find` — a server-side filter over the same tree — instead of paging the whole thing.

## How it works

The daemon owns the browser. It launches a headed Chromium (`chromiumoxide`, system `google-chrome-stable`) on the first tool call and keeps it warm. A `read` or `open` runs a DOM walk *inside the page* that stamps each interesting element with a `data-wmref="nN"` attribute and returns the flat node array. Because the ref lives on the element, `click`/`type`/`find` resolve a ref back to a live element by the selector `[data-wmref="nN"]` — there is no separate ref→selector map to drift out of sync.

Tools

| Tool | Args | Returns |
|---|---|---|
| `open` | `{url}` | page title and URL, plus a fresh snapshot |
| `read` | — | the capped a11y snapshot + `truncated` flag |
| `find` | `{query}` | snapshot nodes whose role or name matches (case-insensitive) |
| `click` | `{ref}` | navigates / acts on the element at `ref` |
| `type` | `{ref, text, submit?}` | types into a textbox; `submit:true` presses Enter |
| `back` | — | navigates back in history |
| `screenshot` | — | writes a PNG into `/tmp/wm-browser-shots/` |

## Two surfaces, one dispatcher

The same tool logic runs two ways:

- **Daemon.** `wm-browser daemon` subscribes to `wm.browser.cmd` on agorabus, dispatches each command, and publishes a reply on `wm.browser.reply` with the originating `cmd_id` echoed. After `--idle-secs` with no traffic (default 300) it exits cleanly and removes its lockfile.
- **One-shot CLI.** `wm-browser open <url>`, `wm-browser read`, and the rest launch a browser, run a single tool, print the JSON reply, and close. Useful for testing a tool without the bus.

Both paths route through the same dispatcher, so argument handling and result shapes match exactly. Replies — on the bus and on stdout — share one envelope: `{ok, result}` on success, `{ok:false, error}` on failure.

If the Chromium child dies (a `kill -9`, a crash), the session detects the dropped CDP connection on the next tool call, relaunches the browser, and retries — so a single crash costs one extra round-trip, not the daemon.

## Install

Needs the system `google-chrome-stable` and a running `agorabus` daemon.

```bash
cargo build --release
install -Dm755 target/release/wm-browser ~/.local/bin/wm-browser
```

Run the daemon:

```bash
wm-browser daemon            # default 5-minute idle timeout
wm-browser daemon --idle-secs 600
```

Or drive a single tool without the bus:

```bash
wm-browser open https://example.com
wm-browser find "More information"
wm-browser click n7
```

## Where it fits

Part of the wintermute fleet's action layer — the daemons the voice brain calls as tools, talking to each other over agorabus. This one is the brain's hands on the web.

## Status

The pure pieces — snapshot capping, `find` filtering, idle/lockfile bookkeeping, crash-detection predicate, wire envelopes — have offline unit tests (see `tests/acceptance.rs`), each mapped to the acceptance criterion it covers. The criteria that need a live Chromium and a running brain — the full open→find→click→read round-trip, tool registration in the brain — are exercised at runtime, not in CI.

## License

MIT or Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
