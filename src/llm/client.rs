use anyhow::{bail, Context, Result};
use futures::StreamExt;
use reqwest::Client;
use tracing::{debug, warn};
use super::types::*;

pub struct LlmClient {
    client: Client,
    api_url: String,
    api_key: String,
    pub model: String,
    pub fast_model: String,
    retries: u32,
}

impl LlmClient {
    pub fn new(url: &str, key: &str, model: &str, fast: &str, retries: u32) -> Self {
        Self {
            client: Client::new(),
            api_url: format!("{}/chat/completions", url.trim_end_matches('/')),
            api_key: key.into(),
            model: model.into(),
            fast_model: fast.into(),
            retries,
        }
    }

    pub fn update_model(&mut self, model: &str) { self.model = model.into(); }
    pub fn update_fast_model(&mut self, model: &str) { self.fast_model = model.into(); }

    pub async fn chat(&self, msgs: &[Message], tools: Option<&[ToolDefinition]>, temp: Option<f32>) -> Result<ChatResponse> {
        self.exec(&self.model, msgs, tools, temp).await
    }

    pub async fn chat_fast(&self, msgs: &[Message], tools: Option<&[ToolDefinition]>, temp: Option<f32>) -> Result<ChatResponse> {
        self.exec(&self.fast_model, msgs, tools, temp).await
    }

    async fn exec(&self, model: &str, msgs: &[Message], tools: Option<&[ToolDefinition]>, temp: Option<f32>) -> Result<ChatResponse> {
        let req = ChatRequest {
            model: model.into(),
            messages: msgs.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            tool_choice: tools.map(|_| serde_json::json!("auto")),
            temperature: temp,
            max_tokens: Some(4096),
            stream: false,
        };

        let mut last_err = None;
        for i in 0..=self.retries {
            if i > 0 { tokio::time::sleep(std::time::Duration::from_millis(500 * 2u64.pow(i-1))).await; }
            match self.send(&req).await {
                Ok(r) => { debug!("LLM {} ok", model); return Ok(r); }
                Err(e) => {
                    let s = format!("{:#}", e);
                    if ["429","500","502","503","timeout","connection"].iter().any(|&x| s.contains(x)) {
                        warn!("Retry {}: {}", i, s);
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        bail!("Retries exhausted: {:#}", last_err.unwrap())
    }

    async fn send(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let res = self.client.post(&self.api_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(req).send().await.context("API request failed")?;

        let status = res.status();
        let body = res.text().await.context("Read body failed")?;
        if !status.is_success() {
            let msg = serde_json::from_str::<ApiError>(&body).map(|e| e.error.message).unwrap_or(body);
            bail!("API error ({}): {}", status.as_u16(), msg);
        }
        serde_json::from_str(&body).context("Parse JSON failed")
    }

    /// Stream a chat completion. Returns the full assembled text via a callback.
    /// `on_token` is called for each received text fragment (for live display).
    pub async fn chat_stream<F>(
        &self,
        msgs: &[Message],
        tools: Option<&[ToolDefinition]>,
        temp: Option<f32>,
        mut on_token: F,
    ) -> Result<String>
    where
        F: FnMut(&str),
    {
        let req = ChatRequest {
            model: self.model.clone(),
            messages: msgs.to_vec(),
            tools: tools.map(|t| t.to_vec()),
            tool_choice: tools.map(|_| serde_json::json!("auto")),
            temperature: temp,
            max_tokens: Some(4096),
            stream: true,
        };

        let res = self.client.post(&self.api_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&req)
            .send()
            .await
            .context("Stream request failed")?;

        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<ApiError>(&body).map(|e| e.error.message).unwrap_or(body);
            bail!("API error ({}): {}", status.as_u16(), msg);
        }

        let mut stream = res.bytes_stream();
        let mut full_text = String::new();
        let mut line_buf = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("Stream read error")?;
            line_buf.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = line_buf.find('\n') {
                let line = line_buf[..pos].trim().to_string();
                line_buf = line_buf[pos + 1..].to_string();

                if !line.starts_with("data: ") { continue; }
                let data = &line[6..];
                if data == "[DONE]" { break; }

                if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                    if let Some(choice) = chunk.choices.first() {
                        if let Some(ref content) = choice.delta.content {
                            on_token(content);
                            full_text.push_str(content);
                        }
                    }
                }
            }
        }

        Ok(full_text)
    }
}
