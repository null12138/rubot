use anyhow::Result;
use async_trait::async_trait;
use super::registry::{Tool, ToolResult};
use std::time::Duration;

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str { "web_fetch" }
    fn description(&self) -> &str { "Fast URL fetcher. Returns text/markdown." }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"url": {"type": "string"}, "max": {"type": "integer"}}, "required": ["url"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let mut url = params["url"].as_str().unwrap_or("").trim().to_string();
        if !url.starts_with("http") { url = format!("https://{}", url); }
        let max = params["max"].as_u64().unwrap_or(10000) as usize;

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0")
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()?;

        let res = match client.get(&url).send().await {
            Ok(r) => r,
            Err(_) => client.get(url.replace("https://", "http://")).send().await?,
        };

        let ct = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = res.text().await?;
        
        let text = if ct.contains("html") { 
            html2text::from_read(body.as_bytes(), 100).unwrap_or(body) 
        } else if ct.contains("json") { 
            serde_json::from_str::<serde_json::Value>(&body).map(|v| serde_json::to_string_pretty(&v).unwrap_or(body.clone())).unwrap_or(body) 
        } else { body };

        let truncated: String = text.chars().take(max).collect();
        Ok(ToolResult::ok(if text.chars().count() > max { format!("{}\n\n[TRUNCATED]", truncated) } else { truncated }))
    }
}
