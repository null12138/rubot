use super::code_exec::scan_new_files;
use super::registry::{Tool, ToolResult};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct PlaywrightTool {
    files_dir: PathBuf,
    timeout: u64,
}

impl PlaywrightTool {
    pub fn new(workspace: &Path, timeout: u64) -> Self {
        let files_dir = workspace.join("files");
        let _ = std::fs::create_dir_all(&files_dir);
        Self {
            files_dir: files_dir.canonicalize().unwrap_or(files_dir),
            timeout,
        }
    }
}

#[async_trait]
impl Tool for PlaywrightTool {
    fn name(&self) -> &str {
        "playwright"
    }

    fn description(&self) -> &str {
        "Use a real browser for JS-heavy target pages. One-shot navigation/extraction/screenshot with Playwright. Supports `text`, `html`, `links`, and `evaluate` actions; relative `screenshot_path` values resolve under workspace `files/`. Do not use this as a generic search engine. If it hits anti-bot / verification pages, stop retrying the same protected source."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "action": {"type": "string", "enum": ["text", "html", "links", "evaluate"]},
                "selector": {"type": "string"},
                "wait_for": {"type": "string"},
                "click": {"type": "string"},
                "delay_ms": {"type": "integer", "minimum": 0},
                "browser": {"type": "string", "enum": ["chromium", "firefox", "webkit"]},
                "headless": {"type": "boolean"},
                "timeout_secs": {"type": "integer", "minimum": 1},
                "script": {"type": "string"},
                "screenshot_path": {"type": "string"},
                "full_page": {"type": "boolean"}
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let url = params["url"].as_str().unwrap_or("").trim();
        if url.is_empty() {
            return Ok(ToolResult::err("missing url".into()));
        }

        let started_at = SystemTime::now();
        let python = if cfg!(target_os = "windows") {
            "python"
        } else {
            "python3"
        };
        let timeout_secs = params["timeout_secs"]
            .as_u64()
            .unwrap_or(self.timeout.max(20))
            .max(1);

        let mut child = Command::new(python);
        child
            .arg("-c")
            .arg(PLAYWRIGHT_RUNNER)
            .arg(&self.files_dir)
            .current_dir(&self.files_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = child.spawn().map_err(|e| {
            anyhow!(
                "failed to launch {} for Playwright: {}. Ensure Python and the playwright package are installed.",
                python,
                e
            )
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&serde_json::to_vec(&params)?).await?;
            stdin.shutdown().await.ok();
        }

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs + 5),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Ok(ToolResult::err(e.to_string())),
            Err(_) => return Ok(ToolResult::err("Playwright tool timed out".into())),
        };

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let mut body = if stdout.is_empty() {
            String::new()
        } else {
            stdout
        };

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

        if output.status.success() {
            if let Some(reason) = anti_bot_interstitial_message(&body) {
                return Ok(ToolResult::err(reason));
            }
            Ok(ToolResult::ok(body))
        } else {
            let error = if !body.is_empty() {
                body
            } else if !stderr.is_empty() {
                stderr
            } else {
                format!(
                    "Playwright failed with exit code {:?}",
                    output.status.code()
                )
            };
            Ok(ToolResult::err(error))
        }
    }
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
        "cloudflare",
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
        "\nThis tool cannot silently solve CAPTCHA / WAF challenges. Change source, provide authenticated cookies, or continue manually in a real browser.",
    );
    Some(message)
}

fn extract_prefixed_line<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
}

const PLAYWRIGHT_RUNNER: &str = r#"
import json
import sys
from pathlib import Path

try:
    from playwright.sync_api import sync_playwright, TimeoutError as PlaywrightTimeoutError
except Exception as exc:
    print(
        "Playwright Python runtime is unavailable. Install it with "
        "`python3 -m pip install playwright && python3 -m playwright install chromium`.\n"
        f"Import error: {exc}"
    )
    sys.exit(2)


def normalize_output_path(base: Path, raw: str) -> Path:
    path = Path(raw)
    if path.is_absolute():
        return path
    out = base
    for part in path.parts:
        if part in ("", "."):
            continue
        if part == "..":
            if out != base:
                out = out.parent
        else:
            out = out / part
    return out


def format_value(value):
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False, indent=2)


def run_evaluate(page, script):
    try:
        return page.evaluate(script)
    except Exception as exc:
        msg = str(exc).lower()
        if not any(flag in msg for flag in ("syntaxerror", "illegal return statement", "unexpected token")):
            raise

    wrapped_variants = [
        f"() => {{ {script} }}",
        f"() => ({script})",
    ]
    last_exc = None
    for candidate in wrapped_variants:
        try:
            return page.evaluate(candidate)
        except Exception as exc:
            last_exc = exc
    raise last_exc


def collect_links(page, selector):
    locator = page.locator(selector if selector else "a")
    count = min(locator.count(), 50)
    rows = []
    for idx in range(count):
        item = locator.nth(idx)
        href = item.get_attribute("href") or ""
        text = (item.inner_text() or "").strip()
        if href or text:
            rows.append(f"- {text or '(no text)'} -> {href or '(no href)'}")
    return "\n".join(rows)


params = json.load(sys.stdin)
base_dir = Path(sys.argv[1]).resolve()
url = params.get("url", "").strip()
if not url:
    print("missing url")
    sys.exit(2)

action = (params.get("action") or "text").strip().lower()
selector = (params.get("selector") or "").strip()
wait_for = (params.get("wait_for") or "").strip()
click = (params.get("click") or "").strip()
script = params.get("script") or ""
browser_name = (params.get("browser") or "chromium").strip().lower()
headless = bool(params.get("headless", True))
timeout_ms = max(int(params.get("timeout_secs") or 30), 1) * 1000
delay_ms = max(int(params.get("delay_ms") or 0), 0)
screenshot_path = (params.get("screenshot_path") or "").strip()
full_page = bool(params.get("full_page", True))

if action not in {"text", "html", "links", "evaluate"}:
    print(f"unsupported action: {action}")
    sys.exit(2)
if action == "evaluate" and not script.strip():
    print("`script` is required when action=`evaluate`")
    sys.exit(2)
if browser_name not in {"chromium", "firefox", "webkit"}:
    print(f"unsupported browser: {browser_name}")
    sys.exit(2)

try:
    with sync_playwright() as p:
        browser_launcher = getattr(p, browser_name)
        browser = browser_launcher.launch(headless=headless)
        page = browser.new_page()
        page.goto(url, wait_until="domcontentloaded", timeout=timeout_ms)
        if wait_for:
            page.wait_for_selector(wait_for, timeout=timeout_ms)
        if click:
            page.click(click, timeout=timeout_ms)
        if delay_ms:
            page.wait_for_timeout(delay_ms)

        title = page.title()
        current_url = page.url

        if action == "text":
            target = page.locator(selector).first if selector else page.locator("body")
            content = target.inner_text(timeout=timeout_ms).strip()
        elif action == "html":
            if selector:
                content = page.locator(selector).first.inner_html(timeout=timeout_ms)
            else:
                content = page.content()
        elif action == "links":
            content = collect_links(page, selector)
        else:
            content = format_value(run_evaluate(page, script))

        output_lines = [
            f"Title: {title}",
            f"URL: {current_url}",
            "",
            content,
        ]

        if screenshot_path:
            destination = normalize_output_path(base_dir, screenshot_path)
            destination.parent.mkdir(parents=True, exist_ok=True)
            page.screenshot(path=str(destination), full_page=full_page)
            output_lines.extend(["", f"Screenshot: {destination}"])

        browser.close()
        print("\n".join(output_lines).strip())
except PlaywrightTimeoutError as exc:
    print(f"Playwright timeout: {exc}")
    sys.exit(3)
except Exception as exc:
    print(f"Playwright error: {exc}")
    sys.exit(1)
"#;

#[cfg(test)]
mod tests {
    use super::{anti_bot_interstitial_message, PlaywrightTool};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!(
            "rubot-playwright-test-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(dir.join("files")).unwrap();
        dir
    }

    #[test]
    fn playwright_tool_uses_workspace_files_dir() {
        let workspace = temp_workspace();
        let tool = PlaywrightTool::new(&workspace, 30);
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
}
