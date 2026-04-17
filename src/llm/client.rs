use anyhow::{bail, Context, Result};
use reqwest::Client;
use tracing::{debug, warn};

use super::types::*;

pub struct LlmClient {
    client: Client,
    api_base_url: String,
    api_key: String,
    model: String,
    fast_model: String,
    max_retries: u32,
}

impl LlmClient {
    pub fn new(
        api_base_url: &str,
        api_key: &str,
        model: &str,
        fast_model: &str,
        max_retries: u32,
    ) -> Self {
        Self {
            client: Client::new(),
            api_base_url: api_base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            fast_model: fast_model.to_string(),
            max_retries,
        }
    }

    /// Update the main (heavy) model.
    pub fn update_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    /// Update the fast model.
    pub fn update_fast_model(&mut self, model: &str) {
        self.fast_model = model.to_string();
    }

    /// Send a chat completion request using the main (heavy) model.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        temperature: Option<f32>,
    ) -> Result<ChatResponse> {
        self.chat_with_model(&self.model, messages, tools, temperature)
            .await
    }

    /// Send a chat completion request using the fast (light) model.
    /// Use this for intermediate tool-calling rounds where deep reasoning is unnecessary.
    pub async fn chat_fast(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        temperature: Option<f32>,
    ) -> Result<ChatResponse> {
        self.chat_with_model(&self.fast_model, messages, tools, temperature)
            .await
    }

    /// Core request method with model override.
    async fn chat_with_model(
        &self,
        model: &str,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        temperature: Option<f32>,
    ) -> Result<ChatResponse> {
        let request = ChatRequest {
            model: model.to_string(),
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            tool_choice: tools.map(|_| serde_json::json!("auto")),
            temperature,
            max_tokens: Some(4096),
        };

        let mut last_error = None;

        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt - 1));
                warn!("Retry attempt {} after {:?}", attempt, delay);
                tokio::time::sleep(delay).await;
            }

            match self.send_request(&request).await {
                Ok(resp) => {
                    debug!("LLM response received (model={}), usage: {:?}", model, resp.usage);
                    return Ok(resp);
                }
                Err(e) => {
                    let err_str = format!("{:#}", e);
                    if is_retryable(&err_str) {
                        warn!("Retryable error: {}", err_str);
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        bail!(
            "All {} retries exhausted. Last error: {:#}",
            self.max_retries,
            last_error.unwrap()
        )
    }

    async fn send_request(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.api_base_url);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .await
            .context("Failed to send request to LLM API")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("Failed to read response body")?;

        if !status.is_success() {
            if let Ok(api_error) = serde_json::from_str::<ApiError>(&body) {
                bail!(
                    "API error ({}): {}",
                    status.as_u16(),
                    api_error.error.message
                );
            }
            bail!("API error ({}): {}", status.as_u16(), body);
        }

        serde_json::from_str(&body).context("Failed to parse LLM response")
    }
}

fn is_retryable(error: &str) -> bool {
    error.contains("429")
        || error.contains("500")
        || error.contains("502")
        || error.contains("503")
        || error.contains("timeout")
        || error.contains("connection")
}
