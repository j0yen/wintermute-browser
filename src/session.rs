//! Live browser session backed by a headed Chromium over CDP.
//!
//! [`BrowserSession`] owns a [`chromiumoxide::Browser`], the task driving
//! its handler stream, and the currently active [`chromiumoxide::Page`].
//! Each tool method returns the JSON result shape from the PRD §2.2
//! table.
//!
//! ## A11y snapshot + ref resolution
//!
//! `read`/`open` build a flat accessibility snapshot by running a DOM
//! walk *in the page* (see [`SNAPSHOT_JS`]). The walk stamps each
//! interesting element with a `data-wmref="nN"` attribute and returns a
//! flat array of `{ref, role, name, value, children_refs}`. Because the
//! ref lives on the element as an attribute, `click`/`type`/`find`
//! resolve a ref back to a live element with the CSS selector
//! `[data-wmref="nN"]` — no separate selector map to drift out of sync.
//! The latest snapshot is cached so `find` can run without a re-read.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::protocol::{cap_snapshot, find_matches, AxNode, Snapshot};
use crate::recovery::is_connection_lost;

/// Default Chrome binary on this host (PRD / environment spec).
pub const DEFAULT_CHROME: &str = "/usr/sbin/google-chrome-stable";

/// Directory screenshots are written to (PRD §2.2).
pub const SHOT_DIR: &str = "/tmp/wm-browser-shots";

/// In-page DOM walk. Returns a flat array of nodes; stamps each with a
/// `data-wmref` so the Rust side can resolve a ref to a live element.
///
/// Role is derived from an explicit ARIA `role`, else a coarse mapping
/// from tag name (PRD §2.4 — a DOM-walk fallback emitting role/name from
/// tag+text is acceptable). Name is aria-label / alt / trimmed text.
const SNAPSHOT_JS: &str = r#"
() => {
  const roleFor = (el) => {
    const explicit = el.getAttribute('role');
    if (explicit) return explicit;
    const tag = el.tagName.toLowerCase();
    const map = {
      a: 'link', button: 'button', h1: 'heading', h2: 'heading',
      h3: 'heading', h4: 'heading', h5: 'heading', h6: 'heading',
      input: 'textbox', textarea: 'textbox', select: 'combobox',
      img: 'image', nav: 'navigation', main: 'main', form: 'form',
      ul: 'list', ol: 'list', li: 'listitem', p: 'paragraph',
      table: 'table', label: 'label'
    };
    if (tag === 'input') {
      const t = (el.getAttribute('type') || 'text').toLowerCase();
      if (t === 'submit' || t === 'button') return 'button';
      if (t === 'checkbox') return 'checkbox';
      if (t === 'radio') return 'radio';
      return 'textbox';
    }
    return map[tag] || 'generic';
  };
  const nameFor = (el) => {
    const al = el.getAttribute('aria-label');
    if (al) return al.trim();
    if (el.tagName.toLowerCase() === 'img') return (el.getAttribute('alt') || '').trim();
    if (el.tagName.toLowerCase() === 'input') {
      return (el.getAttribute('placeholder') || el.getAttribute('name') || el.value || '').trim();
    }
    const t = (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ');
    return t.slice(0, 200);
  };
  const valueFor = (el) => {
    const tag = el.tagName.toLowerCase();
    if (tag === 'input' || tag === 'textarea' || tag === 'select') {
      return (el.value || '').toString().slice(0, 200);
    }
    return '';
  };
  const interesting = (el) => {
    const tag = el.tagName.toLowerCase();
    if (['script','style','noscript','meta','link','head','template'].includes(tag)) return false;
    if (el.getAttribute('aria-hidden') === 'true') return false;
    return true;
  };
  const nodes = [];
  let counter = 0;
  const walk = (el, depth) => {
    if (!interesting(el)) return null;
    const ref = 'n' + (counter++);
    el.setAttribute('data-wmref', ref);
    const node = { ref, role: roleFor(el), name: nameFor(el), value: valueFor(el), children_refs: [] };
    const idx = nodes.length;
    nodes.push(node);
    if (depth < 40) {
      for (const child of el.children) {
        const cref = walk(child, depth + 1);
        if (cref) nodes[idx].children_refs.push(cref);
      }
    }
    return ref;
  };
  if (document.documentElement) walk(document.documentElement, 0);
  return nodes;
}
"#;

/// Raw node shape as returned by [`SNAPSHOT_JS`] before capping.
#[derive(Debug, Deserialize)]
struct RawNode {
    #[serde(rename = "ref")]
    node_ref: String,
    role: String,
    name: String,
    value: String,
    children_refs: Vec<String>,
}

impl From<RawNode> for AxNode {
    fn from(r: RawNode) -> Self {
        Self {
            node_ref: r.node_ref,
            role: r.role,
            name: r.name,
            value: r.value,
            children_refs: r.children_refs,
        }
    }
}

/// An owned, live browser session.
pub struct BrowserSession {
    browser: Browser,
    handler_task: JoinHandle<()>,
    page: Page,
    /// Latest snapshot, cached for `find` and ref resolution.
    last_snapshot: Vec<AxNode>,
    /// Monotonic snapshot id, surfaced to the brain as `snapshot_id`.
    snapshot_id: Arc<AtomicU64>,
    /// Chrome executable path (used on relaunch).
    chrome_path: String,
}

impl BrowserSession {
    /// Launch a fresh headed Chromium and open `about:blank`.
    ///
    /// # Errors
    /// Propagates browser-launch and initial-page failures.
    pub async fn launch() -> Result<Self> {
        Self::launch_with(DEFAULT_CHROME).await
    }

    /// Launch using an explicit chrome executable path.
    ///
    /// # Errors
    /// Propagates browser-launch and initial-page failures.
    pub async fn launch_with(chrome_path: &str) -> Result<Self> {
        let (browser, page, handler_task) = Self::spawn_browser(chrome_path).await?;
        Ok(Self {
            browser,
            handler_task,
            page,
            last_snapshot: Vec::new(),
            snapshot_id: Arc::new(AtomicU64::new(0)),
            chrome_path: chrome_path.to_string(),
        })
    }

    /// Build the browser config, launch, spawn the handler driver, and
    /// open a blank page. Shared by [`launch_with`](Self::launch_with)
    /// and [`relaunch`](Self::relaunch).
    async fn spawn_browser(chrome_path: &str) -> Result<(Browser, Page, JoinHandle<()>)> {
        let config = BrowserConfig::builder()
            .chrome_executable(chrome_path)
            .with_head()
            .no_sandbox()
            .build()
            .map_err(|e| anyhow!("browser config: {e}"))?;
        let (browser, mut handler) = Browser::launch(config)
            .await
            .context("launch chromium")?;
        let handler_task = tokio::spawn(async move {
            while let Some(ev) = handler.next().await {
                if let Err(e) = ev {
                    debug!(error = %e, "chromiumoxide handler event error");
                }
            }
        });
        let page = browser
            .new_page("about:blank")
            .await
            .context("open initial page")?;
        Ok((browser, page, handler_task))
    }

    /// Relaunch the browser after a detected crash (PRD AC9). Aborts the
    /// stale handler task and swaps in a fresh browser+page; the cached
    /// snapshot is cleared because old refs no longer resolve.
    ///
    /// # Errors
    /// Propagates relaunch failures.
    pub async fn relaunch(&mut self) -> Result<()> {
        warn!("wm-browser: chromium connection lost; relaunching");
        self.handler_task.abort();
        let _ = self.browser.kill().await;
        let (browser, page, handler_task) = Self::spawn_browser(&self.chrome_path).await?;
        self.browser = browser;
        self.page = page;
        self.handler_task = handler_task;
        self.last_snapshot.clear();
        Ok(())
    }

    /// Run `op`; if it fails with a connection-lost error, relaunch and
    /// retry once. Centralises AC9 crash recovery for the hot tools.
    async fn with_recovery<F, T>(&mut self, mut op: F) -> Result<T>
    where
        F: FnMut(&Page) -> futures::future::BoxFuture<'_, Result<T>>,
    {
        match op(&self.page).await {
            Ok(v) => Ok(v),
            Err(e) if is_connection_lost(&e.to_string()) => {
                self.relaunch().await?;
                op(&self.page).await
            }
            Err(e) => Err(e),
        }
    }

    /// Allocate the next snapshot id as a string (`"s0"`, `"s1"`, …).
    fn next_snapshot_id(&self) -> String {
        let n = self.snapshot_id.fetch_add(1, Ordering::SeqCst);
        format!("s{n}")
    }

    /// Build, cache, and return a capped snapshot of the current page.
    async fn capture_snapshot(&mut self) -> Result<Snapshot> {
        let page = &self.page;
        let raw: Vec<RawNode> = page
            .evaluate(SNAPSHOT_JS)
            .await
            .context("evaluate snapshot js")?
            .into_value()
            .context("decode snapshot nodes")?;
        let nodes: Vec<AxNode> = raw.into_iter().map(AxNode::from).collect();
        self.last_snapshot = nodes.clone();
        Ok(cap_snapshot(nodes))
    }

    /// `open` tool: navigate to `url`, wait for load, return title+url+
    /// a fresh snapshot id.
    ///
    /// # Errors
    /// Propagates navigation failures (after one crash-recovery retry).
    pub async fn open(&mut self, url: &str) -> Result<Value> {
        let owned = url.to_string();
        self.with_recovery(|page| {
            let url = owned.clone();
            Box::pin(async move {
                page.goto(url).await.context("navigate")?;
                page.wait_for_navigation().await.context("await navigation")?;
                Ok(())
            })
        })
        .await?;
        let title = self.page.get_title().await.context("title")?.unwrap_or_default();
        let cur = self.page.url().await.context("url")?.unwrap_or_default();
        let _snap = self.capture_snapshot().await?;
        let snapshot_id = self.next_snapshot_id();
        Ok(json!({
            "ok": true,
            "title": title,
            "url": cur,
            "snapshot_id": snapshot_id,
        }))
    }

    /// `read` tool: capture and return the (capped) a11y snapshot.
    ///
    /// # Errors
    /// Propagates snapshot-evaluation failures.
    pub async fn read(&mut self) -> Result<Value> {
        let snap = self.capture_snapshot().await?;
        let snapshot_id = self.next_snapshot_id();
        Ok(json!({
            "snapshot": snap.nodes,
            "snapshot_id": snapshot_id,
            "truncated": snap.truncated,
        }))
    }

    /// `find` tool: filter the latest snapshot (capturing one first if
    /// none cached) by `query`.
    ///
    /// # Errors
    /// Propagates snapshot-evaluation failures when a fresh capture is
    /// needed.
    pub async fn find(&mut self, query: &str) -> Result<Value> {
        if self.last_snapshot.is_empty() {
            let _ = self.capture_snapshot().await?;
        }
        let matches = find_matches(&self.last_snapshot, query);
        Ok(json!({ "matches": matches }))
    }

    /// Resolve a snapshot ref to a CSS selector targeting its element.
    fn selector_for(&self, node_ref: &str) -> Result<String> {
        if self.last_snapshot.iter().any(|n| n.node_ref == node_ref) {
            Ok(format!("[data-wmref=\"{node_ref}\"]"))
        } else {
            Err(anyhow!("ref {node_ref} not present in latest snapshot"))
        }
    }

    /// `click` tool: click the element identified by `node_ref`.
    ///
    /// # Errors
    /// Returns an error if the ref is unknown or the click fails (after
    /// one crash-recovery retry).
    pub async fn click(&mut self, node_ref: &str) -> Result<Value> {
        let selector = self.selector_for(node_ref)?;
        self.with_recovery(|page| {
            let selector = selector.clone();
            Box::pin(async move {
                let el = page.find_element(selector).await.context("resolve ref")?;
                el.scroll_into_view().await.context("scroll into view")?;
                el.click().await.context("click")?;
                Ok(())
            })
        })
        .await?;
        Ok(json!({ "ok": true }))
    }

    /// `type` tool: focus the element identified by `node_ref`, type
    /// `text`, optionally pressing Enter when `submit` is set.
    ///
    /// # Errors
    /// Returns an error if the ref is unknown or typing fails (after one
    /// crash-recovery retry).
    pub async fn type_text(&mut self, node_ref: &str, text: &str, submit: bool) -> Result<Value> {
        let selector = self.selector_for(node_ref)?;
        let text = text.to_string();
        self.with_recovery(|page| {
            let selector = selector.clone();
            let text = text.clone();
            Box::pin(async move {
                let el = page.find_element(selector).await.context("resolve ref")?;
                el.click().await.context("focus")?;
                el.type_str(&text).await.context("type")?;
                if submit {
                    el.press_key("Enter").await.context("submit")?;
                }
                Ok(())
            })
        })
        .await?;
        if submit {
            // Best-effort: let the submit navigation settle so a
            // following `read` sees the destination page.
            let _ = self.page.wait_for_navigation().await;
            let _ = self.capture_snapshot().await;
        }
        Ok(json!({ "ok": true }))
    }

    /// `back` tool: navigate back in history, returning the new URL.
    ///
    /// # Errors
    /// Propagates evaluation / URL failures.
    pub async fn back(&mut self) -> Result<Value> {
        self.page
            .evaluate("window.history.back()")
            .await
            .context("history.back")?;
        let _ = self.page.wait_for_navigation().await;
        let _ = self.capture_snapshot().await;
        let cur = self.page.url().await.context("url")?.unwrap_or_default();
        Ok(json!({ "ok": true, "url": cur }))
    }

    /// `screenshot` tool: write a PNG into [`SHOT_DIR`] and return its
    /// path.
    ///
    /// # Errors
    /// Propagates capture / filesystem failures.
    pub async fn screenshot(&mut self) -> Result<Value> {
        std::fs::create_dir_all(SHOT_DIR).context("create shot dir")?;
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .build();
        let bytes = self.page.screenshot(params).await.context("screenshot")?;
        let path = format!("{SHOT_DIR}/shot-{}.png", self.next_snapshot_id());
        std::fs::write(&path, bytes).context("write screenshot")?;
        Ok(json!({ "path": path }))
    }

    /// Close the browser and stop its handler task. Best-effort; called
    /// on clean shutdown.
    pub async fn close(mut self) {
        let _ = self.browser.close().await;
        self.handler_task.abort();
    }
}
