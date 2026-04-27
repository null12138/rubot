//! Telegram Bot channel — simple HTTP polling via the Telegram Bot API.
//!
//! Uses long-polling (getUpdates with timeout) to receive messages.
//! Supports text messages, images, and file delivery.
//! No additional dependencies beyond reqwest.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::agent;

const TG_API_BASE: &str = "https://api.telegram.org/bot";

// ── Telegram Bot ──

pub struct TelegramBot {
    api_base: String,
    agent: Arc<Mutex<agent::Agent>>,
    offset: i64,
    client: reqwest::Client,
    workspace_path: PathBuf,
}

impl TelegramBot {
    pub fn new(token: String, agent: Arc<Mutex<agent::Agent>>, workspace_path: PathBuf) -> Self {
        let api_base = format!("{}{}", TG_API_BASE, token);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            api_base,
            agent,
            offset: 0,
            client,
            workspace_path,
        }
    }

    /// Main polling loop.
    pub async fn run(&mut self) -> Result<()> {
        tracing::info!("telegram bot: starting poll loop");

        tokio::time::sleep(Duration::from_secs(2)).await;

        loop {
            let url = format!(
                "{}/getUpdates?timeout=30&allowed_updates={}",
                self.api_base,
                urlencoding::encode(r#"["message"]"#)
            );
            let url = if self.offset > 0 {
                format!("{}&offset={}", url, self.offset)
            } else {
                url
            };

            let resp = match self.client.get(&url).timeout(Duration::from_secs(35)).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("telegram bot: getUpdates error: {}, retrying in 3s", e);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            let data: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("telegram bot: parse error: {}", e);
                    continue;
                }
            };

            if !data["ok"].as_bool().unwrap_or(false) {
                tracing::warn!("telegram bot: API error: {:?}", data);
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }

            let updates = data["result"].as_array().cloned().unwrap_or_default();
            for update in updates {
                let update_id = update["update_id"].as_i64().unwrap_or(0);
                self.offset = update_id.max(self.offset) + 1;

                if let Some(msg) = update.get("message") {
                    self.handle_message(msg).await;
                }
            }
        }
    }

    async fn handle_message(&self, msg: &Value) {
        let chat_id = msg["chat"]["id"].as_i64().unwrap_or(0);
        if chat_id == 0 {
            return;
        }

        let text = msg["text"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            return;
        }

        // Skip bot commands we don't support
        if text.starts_with('/') && text != "/start" {
            return;
        }

        tracing::info!("telegram msg from chat={}: {:?}", chat_id, text);

        let agent = self.agent.clone();
        let api_base = self.api_base.clone();
        let files_dir = self.workspace_path.join("files");

        tokio::spawn(async move {
            let pre_files = snapshot_files(&files_dir);
            let response = match agent.lock().await.process(&text).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("telegram bot: agent.process error: {:#}", e);
                    let _ = send_message(&api_base, chat_id, "Sorry, I encountered an error.").await;
                    return;
                }
            };

            // Collect outgoing files
            let mut outgoing = agent.lock().await.take_channel_send_queue().await;

            // Snapshot-based detection for files created by tools
            for f in snapshot_files(&files_dir) {
                if !pre_files.contains(&f) && !outgoing.contains(&f) {
                    outgoing.push(f);
                }
            }

            if outgoing.is_empty() {
                let _ = send_message(&api_base, chat_id, &response).await;
            } else {
                let _ = send_rich_message(&api_base, chat_id, &response, &outgoing).await;
            }
        });
    }
}

// ── API helpers ──

async fn send_message(api_base: &str, chat_id: i64, text: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "MarkdownV2",
    });

    let resp: Value = client
        .post(format!("{}/sendMessage", api_base))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    if !resp["ok"].as_bool().unwrap_or(false) {
        // Fallback: try without markdown parsing
        if !text.contains('*') && !text.contains('_') && !text.contains('`') {
            anyhow::bail!("sendMessage failed: {:?}", resp["description"]);
        }
        let fallback_body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        });
        let resp2: Value = client
            .post(format!("{}/sendMessage", api_base))
            .json(&fallback_body)
            .send()
            .await?
            .json()
            .await?;
        if !resp2["ok"].as_bool().unwrap_or(false) {
            anyhow::bail!("sendMessage failed: {:?}", resp2["description"]);
        }
    }

    Ok(())
}

async fn send_rich_message(
    api_base: &str,
    chat_id: i64,
    text: &str,
    media_list: &[PathBuf],
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    // Send text first
    if !text.is_empty() {
        let _ = send_message(api_base, chat_id, text).await;
    }

    // Send each file
    for file_path in media_list {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let is_image = matches!(
            file_path.extension().and_then(|e| e.to_str()).unwrap_or(""),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
        );

        let file_data = match tokio::fs::read(file_path).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("telegram bot: failed to read {}: {}", file_name, e);
                continue;
            }
        };
        let part = reqwest::multipart::Part::bytes(file_data)
            .file_name(file_name.to_string());
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(file_name.to_string(), part);

        let endpoint = if is_image { "sendPhoto" } else { "sendDocument" };
        let url = format!("{}/{}", api_base, endpoint);

        match client.post(&url).multipart(form).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    tracing::warn!("telegram bot: failed to send {}: {}", file_name, resp.status());
                }
            }
            Err(e) => {
                tracing::warn!("telegram bot: failed to upload {}: {}", file_name, e);
            }
        }
    }

    Ok(())
}

// ── Entry point ──

/// Start the Telegram bot channel with auto-reconnect.
pub async fn start(agent: Arc<Mutex<agent::Agent>>, cfg: crate::config::Config) -> Result<()> {
    if cfg.telegram_bot_token.is_empty() {
        tracing::info!("telegram bot: disabled (no bot_token)");
        return Ok(());
    }

    let workspace_path = cfg.workspace_path.clone();

    loop {
        let mut bot = TelegramBot::new(
            cfg.telegram_bot_token.clone(),
            agent.clone(),
            workspace_path.clone(),
        );

        match bot.run().await {
            Ok(()) => {
                tracing::info!("telegram bot: poll loop ended, reconnecting...");
            }
            Err(e) => {
                tracing::warn!("telegram bot: error: {:#}, reconnecting in 30s...", e);
            }
        }

        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

// ── Helpers ──

fn snapshot_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}
