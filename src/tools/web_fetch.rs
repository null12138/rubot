use anyhow::Result;
use async_trait::async_trait;

use super::registry::{Tool, ToolResult};

const REALISTIC_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const DEFAULT_MAX_LENGTH: usize = 8000;
const FETCH_TIMEOUT_SECS: u64 = 30;

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its content as readable text/markdown. \
         Handles HTML, JSON, XML, RSS, and plain text. \
         Automatically adds https:// if missing, follows redirects, \
         and retries with fallback strategies on failure."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch (https:// will be auto-added if missing)"
                },
                "max_length": {
                    "type": "integer",
                    "description": "Maximum character length of output (default: 8000)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let url = match params["url"].as_str() {
            Some(u) if !u.is_empty() => u,
            _ => return Ok(ToolResult::err("Missing 'url' parameter".to_string())),
        };
        let max_length = params["max_length"].as_u64().unwrap_or(DEFAULT_MAX_LENGTH as u64) as usize;

        let normalized = normalize_url(url);

        match fetch_with_retries(&normalized, max_length).await {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult::err(format!("Fetch failed for {}: {}", url, e))),
        }
    }
}

/// Normalize a URL: add scheme if missing, handle common typos.
fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim();

    // Already has a scheme
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }

    // Common prefixes that indicate a scheme was intended
    if trimmed.starts_with("//") {
        return format!("https:{}", trimmed);
    }

    // Default to https
    format!("https://{}", trimmed)
}

/// Build a reqwest client with browser-like settings.
fn build_client(accept_invalid_certs: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(REALISTIC_UA)
        .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(10))
        .gzip(true)
        .brotli(true)
        .cookie_store(true)
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,\
                 application/json,text/plain,*/*;q=0.8"
                    .parse()
                    .unwrap(),
            );
            headers.insert(
                reqwest::header::ACCEPT_LANGUAGE,
                "en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7".parse().unwrap(),
            );
            headers.insert(
                reqwest::header::CACHE_CONTROL,
                "no-cache".parse().unwrap(),
            );
            headers
        });

    if accept_invalid_certs {
        builder = builder.danger_accept_invalid_certs(true);
    }

    Ok(builder.build()?)
}

/// Attempt to fetch with multiple fallback strategies.
async fn fetch_with_retries(url: &str, max_length: usize) -> Result<String> {
    let original_url = url.to_string();

    // Strategy 1: Normal fetch with valid certs
    match do_fetch(url, false).await {
        Ok(result) => return Ok(truncate_result(result, &original_url, max_length)),
        Err(e) => {
            let err_str = format!("{}", e);
            // If it's an SSL/certificate error, try with cert validation disabled
            if is_ssl_error(&err_str) {
                tracing::warn!("SSL error for {}, retrying with cert validation disabled", url);
                match do_fetch(url, true).await {
                    Ok(result) => return Ok(truncate_result(result, &original_url, max_length)),
                    Err(_) => {}
                }
            }
            // If https failed, try http as fallback
            if url.starts_with("https://") {
                let http_url = url.replacen("https://", "http://", 1);
                tracing::warn!("HTTPS failed for {}, trying HTTP: {}", url, http_url);
                match do_fetch(&http_url, false).await {
                    Ok(result) => return Ok(truncate_result(result, &original_url, max_length)),
                    Err(_) => {}
                }
                // Try http with invalid certs too
                match do_fetch(&http_url, true).await {
                    Ok(result) => return Ok(truncate_result(result, &original_url, max_length)),
                    Err(_) => {}
                }
            }
            // All strategies failed
            Err(e)
        }
    }
}

/// Determine if an error is SSL/TLS related.
fn is_ssl_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("certificate")
        || lower.contains("ssl")
        || lower.contains("tls")
        || lower.contains("handshake")
}

/// Perform a single fetch attempt.
async fn do_fetch(url: &str, accept_invalid_certs: bool) -> Result<FetchResult> {
    let client = build_client(accept_invalid_certs)?;
    let response = client.get(url).send().await?;

    let status = response.status();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Read body as bytes first to handle encoding issues
    let bytes = response.bytes().await?;
    let body = String::from_utf8_lossy(&bytes).into_owned();

    let text = convert_body(&body, &content_type);

    Ok(FetchResult {
        status: status.as_u16(),
        final_url,
        content_type,
        text,
    })
}

struct FetchResult {
    status: u16,
    final_url: String,
    content_type: String,
    text: String,
}

/// Convert response body based on content type.
fn convert_body(body: &str, content_type: &str) -> String {
    let ct_lower = content_type.to_lowercase();

    if ct_lower.contains("text/html") || ct_lower.contains("application/xhtml") {
        // Use html2text with wider width for better readability
        html2text::from_read(body.as_bytes(), 120).unwrap_or_else(|_| body.to_string())
    } else if ct_lower.contains("application/json") {
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| body.to_string()),
            Err(_) => body.to_string(),
        }
    } else if ct_lower.contains("application/xml")
        || ct_lower.contains("text/xml")
        || ct_lower.contains("application/rss")
        || ct_lower.contains("application/atom")
    {
        // Basic XML formatting - just return as-is, it's already readable
        body.to_string()
    } else {
        // Plain text or unknown — return as-is
        body.to_string()
    }
}

/// Format and truncate the final result.
fn truncate_result(result: FetchResult, original_url: &str, max_length: usize) -> String {
    let mut header = String::new();

    // Show final URL if it differs from the original (redirect detected)
    if result.final_url != original_url {
        header.push_str(&format!("Final URL: {}\n", result.final_url));
    }
    if result.status != 200 {
        header.push_str(&format!("Status: {}\n", result.status));
    }
    if !header.is_empty() {
        header.push('\n');
    }

    let text = result.text;

    if header.chars().count() + text.chars().count() > max_length {
        let available = max_length.saturating_sub(header.chars().count());
        let truncated: String = text.chars().take(available).collect();
        format!(
            "{}{}\n\n[...truncated at {} chars, total {}]",
            header,
            truncated,
            available,
            text.chars().count()
        )
    } else {
        format!("{}{}", header, text)
    }
}
