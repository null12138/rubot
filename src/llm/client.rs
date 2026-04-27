use super::types::*;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use std::time::Duration;
use tracing::{debug, warn};

const CONNECT_TIMEOUT_SECS: u64 = 20;
const REQUEST_TIMEOUT_SECS: u64 = 180;
const MIN_SILENT_LLM_RETRIES: u32 = 10;
const MAX_RETRY_DELAY_MS: u64 = 8_000;

/// Turn an HTTP error body into a short, terminal-friendly message.
/// - JSON `{error: {message}}` → just the message.
/// - HTML (Cloudflare blocks, error pages) → extracts `<title>` or headline, drops the markup.
/// - Anything else → first 200 chars, single-line.
fn summarize_error_body(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<ApiError>(body) {
        return parsed.error.message;
    }
    let trimmed = body.trim();
    let looks_like_html = trimmed.starts_with('<') || trimmed.to_lowercase().contains("<html");
    if looks_like_html {
        let lower = trimmed.to_lowercase();
        let pick = |open: &str, close: &str| {
            lower.find(open).and_then(|s| {
                let after = s + open.len();
                lower[after..]
                    .find(close)
                    .map(|e| trimmed[after..after + e].trim().to_string())
            })
        };
        let headline = pick("<title>", "</title>")
            .or_else(|| pick("<h1", "</h1>").map(|s| s.trim_start_matches('>').trim().into()))
            .unwrap_or_else(|| "HTML response (likely Cloudflare/WAF block)".into());
        return format!("{} — check your provider endpoint/IP", headline);
    }
    let one_line: String = trimmed
        .chars()
        .filter(|c| *c != '\n' && *c != '\r')
        .take(200)
        .collect();
    if one_line.is_empty() {
        "(empty body)".into()
    } else {
        one_line
    }
}

pub struct LlmClient {
    api_url: String,
    api_key: String,
    pub model: String,
    pub fast_model: String,
    retries: u32,
}

impl LlmClient {
    pub fn new(url: &str, key: &str, model: &str, fast: &str, retries: u32) -> Self {
        Self {
            api_url: format!("{}/chat/completions", url.trim_end_matches('/')),
            api_key: key.into(),
            model: model.into(),
            fast_model: fast.into(),
            retries,
        }
    }

    /// Build a client for sleep/dream consolidation. Uses OpenRouter free models
    /// when `orkey` is set; otherwise falls back to the configured fast model.
    pub fn new_sleep(
        orkey: &str,
        fast_api_url: &str,
        fast_api_key: &str,
        fast_model: &str,
    ) -> Self {
        if orkey.is_empty() {
            Self {
                api_url: format!("{}/chat/completions", fast_api_url.trim_end_matches('/')),
                api_key: fast_api_key.into(),
                model: fast_model.into(),
                fast_model: fast_model.into(),
                retries: 2,
            }
        } else {
            Self {
                api_url: "https://openrouter.ai/api/v1/chat/completions".into(),
                api_key: orkey.into(),
                model: "google/gemini-2.0-flash-exp:free".into(),
                fast_model: "google/gemini-2.0-flash-exp:free".into(),
                retries: 2,
            }
        }
    }

    pub fn update_model(&mut self, model: &str) {
        self.model = model.into();
    }

    /// Single chat call with a specific model, no tools. Used for sleep consolidation.
    pub async fn chat_with_model(&self, model: &str, msgs: &[Message]) -> Result<ChatResponse> {
        self.exec(model, msgs, None, Some(0.3)).await
    }

    pub async fn chat(
        &self,
        msgs: &[Message],
        tools: Option<&[ToolDefinition]>,
        temp: Option<f32>,
    ) -> Result<ChatResponse> {
        self.exec(&self.model, msgs, tools, temp).await
    }

    pub async fn chat_fast(
        &self,
        msgs: &[Message],
        tools: Option<&[ToolDefinition]>,
        temp: Option<f32>,
    ) -> Result<ChatResponse> {
        self.exec(&self.fast_model, msgs, tools, temp).await
    }

    async fn exec(
        &self,
        model: &str,
        msgs: &[Message],
        tools: Option<&[ToolDefinition]>,
        temp: Option<f32>,
    ) -> Result<ChatResponse> {
        let req = ChatRequest {
            model: model.into(),
            messages: msgs.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            tool_choice: tools.map(|_| serde_json::json!("auto")),
            temperature: temp,
            max_tokens: Some(4096),
        };

        let mut last_err = None;
        let retry_budget = self.retries.max(MIN_SILENT_LLM_RETRIES);
        for i in 0..=retry_budget {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms(i))).await;
            }
            let client = build_http_client();
            match self.send(&client, &req).await {
                Ok(r) => {
                    debug!("LLM {} ok", model);
                    return Ok(r);
                }
                Err(e) => {
                    let s = format!("{:#}", e);
                    if is_retryable_llm_error(&e) {
                        warn!("Retry {}/{} for {}: {}", i + 1, retry_budget + 1, model, s);
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        let err = last_err.unwrap();
        let detail = format!("{:#}", err);
        if looks_like_connectivity_timeout(&detail) {
            bail!(
                "Retries exhausted after repeated connection/setup failures to {}: {}",
                self.api_url,
                detail
            );
        }
        bail!("Retries exhausted: {}", detail)
    }

    async fn send(&self, client: &Client, req: &ChatRequest) -> Result<ChatResponse> {
        let res = client
            .post(&self.api_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(req)
            .send()
            .await
            .context("API request failed")?;

        let status = res.status();
        let body = res.text().await.context("Read body failed")?;
        if !status.is_success() {
            bail!(
                "API error ({}): {}",
                status.as_u16(),
                summarize_error_body(&body)
            );
        }
        serde_json::from_str(&body).context("Parse JSON failed")
    }
}

fn retry_delay_ms(attempt: u32) -> u64 {
    let pow = attempt.saturating_sub(1).min(6);
    (500 * 2u64.pow(pow)).min(MAX_RETRY_DELAY_MS)
}

fn build_http_client() -> Client {
    let builder = Client::builder()
        // Some proxy/CDN frontends close pooled TLS connections abruptly, which
        // shows up in reqwest/rustls as unexpected EOF on reuse.
        .pool_max_idle_per_host(0)
        .http1_only()
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .user_agent("rubot/0.1");

    let builder = builder.use_rustls_tls();

    builder.build().unwrap_or_else(|_| Client::new())
}

fn is_retryable_llm_error(err: &anyhow::Error) -> bool {
    let text = format!("{:#}", err).to_ascii_lowercase();
    [
        "429",
        "500",
        "502",
        "503",
        "504",
        "timeout",
        "timed out",
        "connection",
        "connect",
        "tls handshake eof",
        "unexpected eof",
        "connection reset",
        "connection closed",
        "temporarily unavailable",
        "dns error",
        "name or service not known",
        "no route to host",
        "network is unreachable",
        "broken pipe",
        "parse json failed",
        "error decoding response body",
        "eof while parsing",
        "incomplete message",
        "channel closed",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn looks_like_connectivity_timeout(detail: &str) -> bool {
    let text = detail.to_ascii_lowercase();
    (text.contains("timed out") || text.contains("timeout"))
        && (text.contains("connect")
            || text.contains("connection")
            || text.contains("tls")
            || text.contains("handshake"))
}

#[cfg(test)]
mod tests {
    use super::{is_retryable_llm_error, looks_like_connectivity_timeout, retry_delay_ms};

    #[test]
    fn timed_out_connect_errors_are_retryable() {
        let err = anyhow::anyhow!(
            "API request failed: error sending request: client error (Connect): operation timed out"
        );
        assert!(is_retryable_llm_error(&err));
        assert!(looks_like_connectivity_timeout(&format!("{:#}", err)));
    }

    #[test]
    fn non_network_errors_are_not_marked_retryable() {
        let err = anyhow::anyhow!("API error (400): bad request");
        assert!(!is_retryable_llm_error(&err));
        assert!(!looks_like_connectivity_timeout(&format!("{:#}", err)));
    }

    #[test]
    fn malformed_provider_payloads_are_retryable() {
        let err = anyhow::anyhow!("Parse JSON failed: EOF while parsing a value");
        assert!(is_retryable_llm_error(&err));
    }

    #[test]
    fn retry_delay_is_capped() {
        assert_eq!(retry_delay_ms(1), 500);
        assert_eq!(retry_delay_ms(2), 1000);
        assert_eq!(retry_delay_ms(10), 8000);
    }
}
