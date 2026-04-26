use super::code_exec::scan_new_files;
use super::registry::{Tool, ToolResult};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const IDLE_TIMEOUT_SECS: u64 = 120;

pub struct BrowserTool {
    files_dir: PathBuf,
    timeout: u64,
    state: Arc<Mutex<BrowserState>>,
}

struct BrowserState {
    browser: Option<Arc<Browser>>,
    handler: Option<JoinHandle<()>>,
    reaper: Option<JoinHandle<()>>,
    last_used: Instant,
}

impl BrowserTool {
    pub fn new(workspace: &Path, timeout: u64) -> Self {
        let files_dir = workspace.join("files");
        let _ = std::fs::create_dir_all(&files_dir);
        Self {
            files_dir: files_dir.canonicalize().unwrap_or(files_dir),
            timeout,
            state: Arc::new(Mutex::new(BrowserState {
                browser: None,
                handler: None,
                reaper: None,
                last_used: Instant::now(),
            })),
        }
    }

    async fn ensure_browser(state: &Arc<Mutex<BrowserState>>) -> Result<Arc<Browser>> {
        {
            let s = state.lock().await;
            if let Some(ref b) = s.browser {
                let b = Arc::clone(b);
                drop(s);
                let mut s = state.lock().await;
                s.last_used = Instant::now();
                return Ok(b);
            }
        }

        let (browser, mut handler) = Browser::launch(
            BrowserConfig::builder()
                .window_size(1280, 720)
                .arg("--disable-gpu")
                .arg("--disable-extensions")
                .arg("--disable-dev-shm-usage")
                .arg("--disable-background-networking")
                .arg("--disable-sync")
                .arg("--no-first-run")
                .arg("--safebrowsing-disable-auto-update")
                .arg("--disable-default-apps")
                .arg("--disable-component-update")
                .build()
                .map_err(|e| anyhow!("failed to build browser config: {}", e))?,
        )
        .await
        .map_err(|e| anyhow!("failed to launch browser: {}. Is Chrome/Chromium installed?", e))?;

        let browser = Arc::new(browser);
        let arc = Arc::clone(&browser);
        let h = tokio::spawn(async move {
            let _ = arc;
            while let Some(_event) = handler.next().await {}
        });

        let reaper_state = Arc::clone(state);
        let reaper = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let mut s = reaper_state.lock().await;
                if s.browser.is_none() {
                    break;
                }
                if s.last_used.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECS) {
                    tracing::info!("browser idle for {}s, shutting down", IDLE_TIMEOUT_SECS);
                    s.browser = None;
                    if let Some(h) = s.handler.take() {
                        h.abort();
                    }
                    break;
                }
            }
        });

        let mut s = state.lock().await;
        s.browser = Some(Arc::clone(&browser));
        s.handler = Some(h);
        s.reaper = Some(reaper);
        s.last_used = Instant::now();
        Ok(browser)
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Control a headless Chromium browser. Supports `inspect` (returns structured snapshot of interactive elements), `click`, `fill`, `press`, `back`, `forward`, `wait`, `text`, `html`, `links`, `evaluate`, `screenshot`, and `close`. Prefer `inspect` first to discover elements by `target_index` instead of guessing selectors. The browser launches on first use and auto-closes after 2 minutes idle. Do not use as a generic search engine. Stops retrying if it hits anti-bot pages."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "action": {"type": "string", "enum": ["inspect", "click", "fill", "press", "back", "forward", "wait", "close", "text", "html", "links", "evaluate", "screenshot"]},
                "selector": {"type": "string"},
                "target_index": {"type": "integer", "minimum": 1},
                "value": {"type": "string"},
                "delay_ms": {"type": "integer", "minimum": 0},
                "max_elements": {"type": "integer", "minimum": 1, "maximum": 100},
                "script": {"type": "string"},
                "screenshot_path": {"type": "string"},
                "full_page": {"type": "boolean"},
                "timeout_secs": {"type": "integer", "minimum": 1}
            },
            "required": []
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let started_at = SystemTime::now();
        let action = params["action"].as_str().unwrap_or("text").to_lowercase();
        let timeout_secs = params["timeout_secs"]
            .as_u64()
            .unwrap_or(self.timeout.max(20))
            .max(1);

        if action == "close" {
            let mut s = self.state.lock().await;
            s.browser = None;
            if let Some(h) = s.handler.take() {
                h.abort();
            }
            if let Some(r) = s.reaper.take() {
                r.abort();
            }
            return Ok(ToolResult::ok("Browser closed.".into()));
        }

        let browser = match Self::ensure_browser(&self.state).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolResult::err(e.to_string())),
        };

        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs + 5),
            run_action(&browser, &self.files_dir, &action, &params),
        )
        .await;

        {
            let mut s = self.state.lock().await;
            s.last_used = Instant::now();
        }

        let output = match result {
            Ok(Ok(body)) => body,
            Ok(Err(e)) => return Ok(ToolResult::err(format!("Browser error: {}", e))),
            Err(_) => return Ok(ToolResult::err("Browser tool timed out".into())),
        };

        let mut body = output;
        let generated = scan_new_files(&self.files_dir, started_at);
        if !generated.is_empty() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("[Generated files]\n");
            for (path, size) in generated {
                body.push_str(&format!("- {} ({} bytes)\n", path.display(), size));
            }
        }

        if let Some(reason) = anti_bot_interstitial_message(&body) {
            return Ok(ToolResult::err(reason));
        }
        Ok(ToolResult::ok(body))
    }
}

async fn run_action(
    browser: &Browser,
    files_dir: &Path,
    action: &str,
    params: &serde_json::Value,
) -> Result<String> {
    let url = params["url"].as_str().unwrap_or("").trim();
    let selector = params["selector"].as_str().unwrap_or("").trim();
    let target_index = params["target_index"].as_u64();
    let value = params["value"].as_str().unwrap_or("");
    let script = params["script"].as_str().unwrap_or("");
    let delay_ms = params["delay_ms"].as_u64().unwrap_or(0);
    let max_elements = params["max_elements"]
        .as_u64()
        .map(|v| v.clamp(1, 100))
        .unwrap_or(25);
    let screenshot_path = params["screenshot_path"].as_str().unwrap_or("").trim();
    let full_page = params["full_page"].as_bool().unwrap_or(true);

    let page = if !url.is_empty() {
        let pages = browser.pages().await.map_err(|e| anyhow!("{}", e))?;
        if let Some(existing) = pages.into_iter().next() {
            let current = existing.url().await.ok().flatten().unwrap_or_default();
            if current.is_empty()
                || current == "about:blank"
                || current == "chrome://newtab/"
            {
                existing
                    .goto(url)
                    .await
                    .map_err(|e| anyhow!("navigation failed: {}", e))?;
                existing
            } else {
                browser.new_page(url).await.map_err(|e| anyhow!("{}", e))?
            }
        } else {
            browser.new_page(url).await.map_err(|e| anyhow!("{}", e))?
        }
    } else {
        let pages = browser.pages().await.map_err(|e| anyhow!("{}", e))?;
        if let Some(p) = pages.into_iter().next() {
            p
        } else {
            browser
                .new_page("about:blank")
                .await
                .map_err(|e| anyhow!("{}", e))?
        }
    };

    match action {
        "inspect" | "click" | "fill" | "press" | "back" | "forward" | "wait" => {}
        _ if url.is_empty() => {
            let current = page.url().await.ok().flatten().unwrap_or_default();
            if current.is_empty() || current == "about:blank" {
                return Err(anyhow!("missing url: provide `url` for a new browser session"));
            }
        }
        _ => {}
    }

    let action_label = match action {
        "click" => {
            let sel = resolve_selector(selector, target_index);
            page.find_element(&sel)
                .await
                .map_err(|e| anyhow!("element not found ({}): {}", sel, e))?
                .click()
                .await
                .map_err(|e| anyhow!("click failed: {}", e))?;
            Some(format!("click {}", sel))
        }
        "fill" => {
            if value.is_empty() {
                return Err(anyhow!("`value` is required when action=`fill`"));
            }
            let sel = resolve_selector(selector, target_index);
            let el = page
                .find_element(&sel)
                .await
                .map_err(|e| anyhow!("element not found ({}): {}", sel, e))?;
            el.click().await.map_err(|e| anyhow!("{}", e))?;
            el.type_str(value)
                .await
                .map_err(|e| anyhow!("fill failed: {}", e))?;
            Some(format!("fill {}", sel))
        }
        "press" => {
            if value.is_empty() {
                return Err(anyhow!(
                    "`value` is required when action=`press` (key name, e.g. Enter)"
                ));
            }
            if !selector.is_empty() || target_index.is_some() {
                let sel = resolve_selector(selector, target_index);
                page.find_element(&sel)
                    .await
                    .map_err(|e| anyhow!("{}", e))?
                    .press_key(value)
                    .await
                    .map_err(|e| anyhow!("{}", e))?;
            } else {
                page.evaluate_expression(&format!(
                    "document.dispatchEvent(new KeyboardEvent('keydown',{{key:'{}'}}))",
                    value
                ))
                .await
                .map_err(|e| anyhow!("{}", e))?;
            }
            Some(format!("press {}", value))
        }
        "back" => {
            page.evaluate_expression("history.back()")
                .await
                .map_err(|e| anyhow!("{}", e))?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            Some("back".into())
        }
        "forward" => {
            page.evaluate_expression("history.forward()")
                .await
                .map_err(|e| anyhow!("{}", e))?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            Some("forward".into())
        }
        "wait" => {
            tokio::time::sleep(Duration::from_millis(delay_ms.max(1))).await;
            Some(format!("wait {}ms", delay_ms))
        }
        _ => None,
    };

    if delay_ms > 0 && action != "wait" {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }

    let title = page
        .get_title()
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let current_url = page.url().await.ok().flatten().unwrap_or_default();

    match action {
        "text" => {
            let el = if !selector.is_empty() {
                page.find_element(selector)
                    .await
                    .map_err(|e| anyhow!("{}", e))?
            } else {
                page.find_element("body")
                    .await
                    .map_err(|e| anyhow!("{}", e))?
            };
            let text = el
                .inner_text()
                .await
                .map_err(|e| anyhow!("{}", e))?
                .unwrap_or_default();
            return Ok(format!("Title: {}\nURL: {}\n\n{}", title, current_url, text.trim()));
        }
        "html" => {
            let html = if !selector.is_empty() {
                page.find_element(selector)
                    .await
                    .map_err(|e| anyhow!("{}", e))?
                    .outer_html()
                    .await
                    .map_err(|e| anyhow!("{}", e))?
                    .unwrap_or_default()
            } else {
                page.content().await.map_err(|e| anyhow!("{}", e))?
            };
            return Ok(format!("Title: {}\nURL: {}\n\n{}", title, current_url, html));
        }
        "links" => {
            let result = page
                .evaluate_expression(
                    r#"Array.from(document.querySelectorAll('a')).slice(0, 50).map(a => ({
                        text: (a.innerText || '').trim().substring(0, 120),
                        href: a.href || ''
                    }))"#,
                )
                .await
                .map_err(|e| anyhow!("{}", e))?;
            let links: Vec<serde_json::Value> = result.into_value().unwrap_or_default();
            let mut out = format!("Title: {}\nURL: {}\n\n", title, current_url);
            for l in &links {
                let text = l["text"].as_str().unwrap_or("(no text)");
                let href = l["href"].as_str().unwrap_or("(no href)");
                out.push_str(&format!("- {} -> {}\n", text, href));
            }
            return Ok(out);
        }
        "evaluate" => {
            if script.is_empty() {
                return Err(anyhow!("`script` is required when action=`evaluate`"));
            }
            let result = page
                .evaluate_expression(script)
                .await
                .map_err(|e| anyhow!("{}", e))?;
            let val = result
                .value()
                .map(|v: &serde_json::Value| {
                    if v.is_string() {
                        v.as_str().unwrap_or_default().to_string()
                    } else {
                        serde_json::to_string_pretty(v).unwrap_or_default()
                    }
                })
                .unwrap_or_default();
            return Ok(format!(
                "Title: {}\nURL: {}\n\n{}",
                title, current_url, val
            ));
        }
        _ => {}
    }

    let preview: String = page
        .evaluate_expression("document.body ? document.body.innerText.substring(0, 1200) : ''")
        .await
        .ok()
        .and_then(|r| r.into_value().ok())
        .and_then(|v: serde_json::Value| v.as_str().map(String::from))
        .unwrap_or_default();

    let snapshot = if matches!(
        action,
        "inspect" | "click" | "fill" | "press" | "back" | "forward" | "wait"
    ) {
        Some(run_inspect(&page, max_elements).await)
    } else {
        None
    };

    let mut output = vec![
        format!("Title: {}", title),
        format!("URL: {}", current_url),
    ];

    if let Some(label) = action_label {
        output.push(format!("Action: {}", label));
    }

    if let Some(snap) = &snapshot {
        if let Some(headings) = snap.get("headings").and_then(|h| h.as_array()) {
            output.push(String::new());
            output.push("Headings:".into());
            for h in headings {
                if let Some(t) = h.as_str() {
                    output.push(format!("- {}", t));
                }
            }
            if headings.is_empty() {
                output.push("- (none)".into());
            }
        }

        if let Some(elements) = snap.get("elements").and_then(|e| e.as_array()) {
            output.push(String::new());
            output.push("Interactive elements:".into());
            for (idx, el) in elements.iter().enumerate() {
                let i = idx + 1;
                let tag = el["tag"].as_str().unwrap_or("element");
                let mut parts = vec![format!("[{}] {}", i, tag)];
                if let Some(r) = el["role"].as_str() {
                    parts.push(format!("role={}", r));
                }
                if let Some(t) = el["type"].as_str() {
                    parts.push(format!("type={}", t));
                }
                if let Some(t) = el["text"].as_str() {
                    parts.push(format!("text={}", &t[..t.len().min(120)]));
                }
                if let Some(l) = el["label"].as_str() {
                    parts.push(format!("label={}", &l[..l.len().min(120)]));
                }
                if let Some(h) = el["href"].as_str() {
                    parts.push(format!("href={}", &h[..h.len().min(160)]));
                }
                if let Some(s) = el["selector"].as_str() {
                    parts.push(format!("selector={}", &s[..s.len().min(200)]));
                }
                output.push(format!(" {}", parts.join(" | ")));
            }
            if elements.is_empty() {
                output.push(" (none)".into());
            }
        }
    }

    output.push(String::new());
    output.push("Preview:".into());
    output.push(if preview.is_empty() {
        "(empty)".into()
    } else {
        preview
    });

    if !screenshot_path.is_empty() {
        let dest = normalize_path(files_dir, screenshot_path);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let params_builder = ScreenshotParams::builder().full_page(full_page);
        if let Ok(bytes) = page.screenshot(params_builder.build()).await {
            std::fs::write(&dest, &bytes)?;
            output.push(String::new());
            output.push(format!("Screenshot: {}", dest.display()));
        }
    }

    Ok(output.join("\n"))
}

async fn run_inspect(page: &chromiumoxide::Page, max_elements: u64) -> serde_json::Value {
    let js = format!(
        r#"(function(maxElements) {{
            var visible = function(el) {{
                if (!el || !(el instanceof Element)) return false;
                var style = window.getComputedStyle(el);
                if (!style || style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') return false;
                var rect = el.getBoundingClientRect();
                return rect.width > 0 && rect.height > 0;
            }};
            var norm = function(v) {{ return (v || '').replace(/\s+/g, ' ').trim(); }};
            var selectorFor = function(el) {{
                if (!(el instanceof Element)) return '';
                if (el.id) {{
                    var idSel = '#' + CSS.escape(el.id);
                    if (document.querySelectorAll(idSel).length === 1) return idSel;
                }}
                for (var _i = 0, attrs = ['data-testid','data-test','name','aria-label','placeholder','title']; _i < attrs.length; _i++) {{
                    var attr = attrs[_i];
                    var val = el.getAttribute(attr);
                    if (!val) continue;
                    var sel = el.tagName.toLowerCase() + '[' + attr + '="' + CSS.escape(val) + '"]';
                    if (document.querySelectorAll(sel).length === 1) return sel;
                }}
                var parts = [];
                var node = el;
                while (node && node.nodeType === 1 && node !== document.body) {{
                    var part = node.tagName.toLowerCase();
                    if (node.id) {{ part += '#' + CSS.escape(node.id); parts.unshift(part); break; }}
                    var index = 1, sib = node;
                    while ((sib = sib.previousElementSibling)) {{ if (sib.tagName === node.tagName) index++; }}
                    part += ':nth-of-type(' + index + ')';
                    parts.unshift(part);
                    node = node.parentElement;
                }}
                parts.unshift('body');
                return parts.join(' > ');
            }};
            var headlineNodes = Array.from(document.querySelectorAll('h1, h2, h3')).filter(visible).map(function(el) {{ return norm(el.innerText || ''); }}).filter(Boolean).slice(0, 8);
            var raw = Array.from(document.querySelectorAll('a[href], button, input, textarea, select, summary, [role="button"], [role="link"], [contenteditable="true"], [tabindex]'));
            var items = [], seen = new Set();
            for (var _j = 0; _j < raw.length; _j++) {{
                var el = raw[_j];
                if (!visible(el)) continue;
                var sel = selectorFor(el);
                var tag = el.tagName.toLowerCase();
                var role = norm(el.getAttribute('role'));
                var text = norm(el.innerText || '');
                var label = norm(el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.getAttribute('title') || el.getAttribute('name') || '');
                var href = norm(el.getAttribute('href'));
                var type_ = norm(el.getAttribute('type'));
                var key = sel + '|' + text + '|' + label + '|' + href + '|' + tag;
                if (seen.has(key)) continue;
                seen.add(key);
                items.push({{ tag: tag, role: role, text: text, label: label, href: href, type: type_, selector: sel }});
                if (items.length >= maxElements) break;
            }}
            return {{ headings: headlineNodes, elements: items }};
        }})({})"#,
        max_elements
    );

    page.evaluate_expression(&js)
        .await
        .ok()
        .and_then(|r| r.into_value().ok())
        .unwrap_or(serde_json::json!({"headings": [], "elements": []}))
}

fn resolve_selector(selector: &str, target_index: Option<u64>) -> String {
    if !selector.is_empty() {
        return selector.to_string();
    }
    if let Some(idx) = target_index {
        return format!("body >> nth={}", idx - 1);
    }
    selector.to_string()
}

fn normalize_path(base: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return path;
    }
    let mut out = base.to_path_buf();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(p) => out.push(p),
            _ => {}
        }
    }
    out
}

fn anti_bot_interstitial_message(output: &str) -> Option<String> {
    let lower = output.to_ascii_lowercase();
    let suspicious = [
        "title: just a moment",
        "title: 请稍候",
        "title: attention required",
        "verify you are human",
        "verification successful waiting",
        "checking if the site connection is secure",
        "enable javascript and cookies to continue",
        "sorry, but your computer or network may be sending automated queries",
        "unusual traffic from your computer network",
        "/cdn-cgi/challenge-platform",
        "cf-chl",
    ]
    .iter()
    .find(|needle| lower.contains(**needle))?;

    let title = extract_prefixed_line(output, "Title:");
    let url = extract_prefixed_line(output, "URL:");
    let screenshot = extract_prefixed_line(output, "Screenshot:");

    let mut message = String::from(
        "Browser hit an anti-bot / human-verification page instead of the target content.",
    );
    if let Some(title) = title {
        message.push_str(&format!("\nTitle: {}", title));
    }
    if let Some(url) = url {
        message.push_str(&format!("\nURL: {}", url));
    }
    message.push_str(&format!("\nDetected marker: {}", suspicious));
    if let Some(path) = screenshot {
        message.push_str(&format!("\nScreenshot: {}", path));
    }
    message.push_str(
        "\nThis tool cannot silently solve CAPTCHA / WAF challenges. Change source or provide authenticated cookies.",
    );
    Some(message)
}

fn extract_prefixed_line<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{anti_bot_interstitial_message, BrowserTool};
    use crate::tools::registry::Tool;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!(
            "rubot-browser-test-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(dir.join("files")).unwrap();
        dir
    }

    #[test]
    fn browser_tool_uses_workspace_files_dir() {
        let workspace = temp_workspace();
        let tool = BrowserTool::new(&workspace, 30);
        let expected = workspace
            .join("files")
            .canonicalize()
            .unwrap_or_else(|_| workspace.join("files"));
        assert_eq!(tool.files_dir, expected);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn anti_bot_pages_are_reported_as_blocked() {
        let msg = anti_bot_interstitial_message(
            "Title: Just a moment...\nURL: https://papers.ssrn.com/\n\nChecking if the site connection is secure\nScreenshot: /tmp/x.png",
        )
        .unwrap();
        assert!(msg.contains("anti-bot"));
        assert!(msg.contains("Just a moment"));
        assert!(msg.contains("/tmp/x.png"));
    }

    #[test]
    fn normal_pages_are_not_flagged() {
        assert!(
            anti_bot_interstitial_message("Title: Example Domain\nURL: https://example.com")
                .is_none()
        );
    }

    #[test]
    fn tool_name_is_browser() {
        let workspace = temp_workspace();
        let tool = BrowserTool::new(&workspace, 30);
        assert_eq!(tool.name(), "browser");
        let _ = std::fs::remove_dir_all(workspace);
    }
}
