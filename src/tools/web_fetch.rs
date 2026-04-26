use super::registry::{Tool, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::StatusCode;
use std::time::Duration;

static UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a URL as text."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"url": {"type": "string"}, "max": {"type": "integer"}}, "required": ["url"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let url = normalize_url(params["url"].as_str().unwrap_or(""));
        let max = params["max"].as_u64().unwrap_or(10000) as usize;

        let client = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()?;

        let res = match client.get(&url).send().await {
            Ok(res) => res,
            Err(err) => return Ok(ToolResult::err(summarize_transport_error(&url, &err))),
        };
        let status = res.status();
        let body = res.text().await?;
        if let Some(error) = summarize_http_error(&url, status, body.clone()) {
            return Ok(ToolResult::err(error));
        }

        let text = if body.trim_start().starts_with('{') || body.trim_start().starts_with('[') {
            serde_json::from_str::<serde_json::Value>(&body)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or(body.clone()))
                .unwrap_or(body)
        } else {
            html2text::from_read(body.as_bytes(), 100).unwrap_or(body)
        };

        let truncated: String = text.chars().take(max).collect();
        Ok(ToolResult::ok(if text.chars().count() > max {
            format!("{}\n\n[TRUNCATED]", truncated)
        } else {
            truncated
        }))
    }
}

fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{}", trimmed)
    }
}

fn summarize_http_error(url: &str, status: StatusCode, body: String) -> Option<String> {
    if status.is_success() {
        return None;
    }

    let preview = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect::<String>();
    Some(if preview.is_empty() {
        format!("HTTP {} for {}", status, url)
    } else {
        format!("HTTP {} for {}: {}", status, url, preview)
    })
}

fn summarize_transport_error(url: &str, err: &reqwest::Error) -> String {
    let detail = format!("{:#}", err);
    summarize_transport_error_detail(url, &detail)
}

fn summarize_transport_error_detail(url: &str, detail: &str) -> String {
    let lower = detail.to_ascii_lowercase();
    if [
        "ssl_error_syscall",
        "unexpected eof",
        "eof occurred in violation of protocol",
        "tls handshake eof",
        "connection closed",
        "unexpected eof while reading",
        "peer closed connection",
        "connection reset",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return format!(
            "TLS / connection handshake failed for {}. The remote site closed the connection before completing HTTPS setup. This usually means site-side blocking, WAF / anti-bot protection, or regional network restrictions. Raw error: {}",
            url, detail
        );
    }
    format!("Request failed for {}: {}", url, detail)
}

#[cfg(test)]
mod tests {
    use super::{normalize_url, summarize_http_error, summarize_transport_error_detail};

    #[test]
    fn normalize_url_defaults_to_https() {
        assert_eq!(
            normalize_url("example.com/test"),
            "https://example.com/test"
        );
        assert_eq!(normalize_url("http://example.com"), "http://example.com");
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn summarize_http_error_includes_status_and_url() {
        let msg = summarize_http_error(
            "https://example.com/blocked",
            reqwest::StatusCode::FORBIDDEN,
            "<html>access denied</html>".into(),
        )
        .unwrap();
        assert!(msg.contains("403 Forbidden"));
        assert!(msg.contains("https://example.com/blocked"));
        assert!(msg.contains("access denied"));
    }

    #[test]
    fn summarize_transport_error_classifies_tls_shutdowns() {
        let msg = summarize_transport_error_detail(
            "https://www.fenbi.com",
            "client error (Connect): unexpected eof while reading",
        );
        assert!(msg.contains("remote site closed the connection"));
        assert!(msg.contains("https://www.fenbi.com"));
    }
}
