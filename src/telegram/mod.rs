pub mod download;
pub mod formatting;
pub mod handler;

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use teloxide::prelude::*;

use crate::agent::Agent;
use crate::config::Config;

/// Start the Telegram bot. Returns a JoinHandle so main can track it.
/// If the bot token is not configured, returns None.
pub async fn start_bot(
    config: &Config,
    agent: Arc<Mutex<Agent>>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    let token = match &config.telegram_bot_token {
        Some(t) if !t.is_empty() => t.clone(),
        _ => {
            tracing::info!("RUBOT_TELEGRAM_BOT_TOKEN not set, skipping Telegram bot");
            return Ok(None);
        }
    };

    let bot = Bot::new(token);
    let upload_dir: PathBuf = config.workspace_path.join("tg_uploads");
    tokio::fs::create_dir_all(&upload_dir).await?;

    let mut dispatcher = handler::build_handler(bot, agent, upload_dir);

    let handle = tokio::spawn(async move {
        dispatcher.dispatch().await;
    });

    tracing::info!("Telegram bot started");
    Ok(Some(handle))
}
