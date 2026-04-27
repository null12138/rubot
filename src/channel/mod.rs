pub mod tg;
pub mod wechat;

use crate::agent;
use std::sync::Arc;
use tokio::sync::Mutex;

// Re-export WeChat's public API for backwards compatibility.
pub use wechat::qr_login;

/// Start all configured channels. Spawns background tasks for WeChat and/or Telegram.
/// This replaces the old WeChat-only start().
pub async fn start(agent: Arc<Mutex<agent::Agent>>, cfg: crate::config::Config) -> anyhow::Result<()> {
    let wechat_enabled = !cfg.wechat_bot_token.is_empty();
    let telegram_enabled = !cfg.telegram_bot_token.is_empty();

    if !wechat_enabled && !telegram_enabled {
        tracing::info!("channel: no channels configured (set RUBOT_WECHAT_BOT_TOKEN or RUBOT_TELEGRAM_BOT_TOKEN)");
        return Ok(());
    }

    if wechat_enabled {
        let agent_wc = agent.clone();
        let cfg_wc = cfg.clone();
        tokio::spawn(async move {
            let mut bot = wechat::WeChatBot::new(
                cfg_wc.wechat_bot_token.clone(),
                cfg_wc.wechat_base_url.clone(),
                agent_wc,
                cfg_wc.workspace_path.clone(),
            );
            loop {
                if let Err(e) = bot.run().await {
                    tracing::warn!("wechat bot: error: {:#}, reconnecting in 30s...", e);
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    if telegram_enabled {
        let agent_tg = agent.clone();
        let cfg_tg = cfg.clone();
        tokio::spawn(async move {
            tg::start(agent_tg, cfg_tg).await.ok();
        });
    }

    // Wait forever — channels run in background tasks.
    futures::future::pending::<()>().await;
    Ok(())
}
