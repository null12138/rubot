use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use teloxide::prelude::*;
use teloxide::types::ParseMode;

use crate::agent::Agent;
use super::download;
use super::formatting;

pub fn build_handler(
    bot: Bot,
    agent: Arc<Mutex<Agent>>,
    upload_dir: PathBuf,
) -> Dispatcher<Bot, anyhow::Error, teloxide::dispatching::DefaultKey> {
    let handler = dptree::entry().branch(
        Update::filter_message().endpoint(move |bot: Bot, msg: Message| {
            let agent = agent.clone();
            let upload_dir = upload_dir.clone();
            async move { handle_message(bot, msg, agent, upload_dir).await }
        }),
    );

    Dispatcher::builder(bot, handler)
        .error_handler(LoggingErrorHandler::new())
        .build()
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    agent: Arc<Mutex<Agent>>,
    upload_dir: PathBuf,
) -> anyhow::Result<()> {
    let chat_id = msg.chat.id;

    // Extract text and file/image context from the message
    let (text, context) = match extract_message_content(&bot, &msg, &upload_dir).await {
        Ok(content) => content,
        Err(e) => {
            tracing::error!("Error extracting TG message content: {}", e);
            bot.send_message(chat_id, format!("Error processing attachment: {}", e))
                .await?;
            return Ok(());
        }
    };

    // Need some text or context to process
    let text = match text {
        Some(t) if !t.is_empty() => t,
        Some(_) if !context.is_empty() => "(user sent an attachment)".to_string(),
        _ => return Ok(()), // No content (sticker, etc.) — ignore
    };

    // Acquire lock, process, release
    let response = {
        let mut ag = agent.lock().await;
        ag.process_with_context(&text, &context).await
    };

    match response {
        Ok(reply) => {
            let formatted = formatting::format_for_telegram(&reply);
            send_long_message(&bot, chat_id, &formatted).await?;
        }
        Err(e) => {
            tracing::error!("Agent error for TG message: {:#}", e);
            let err_text = formatting::format_for_telegram(&format!("Agent error: {}", e));
            bot.send_message(chat_id, err_text)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }
    }

    Ok(())
}

/// Extract text and attachment context from a Telegram message.
/// Returns (text, context_string).
async fn extract_message_content(
    bot: &Bot,
    msg: &Message,
    upload_dir: &PathBuf,
) -> anyhow::Result<(Option<String>, String)> {
    // Text from message body or caption
    let text = msg
        .text()
        .map(|s| s.to_string())
        .or_else(|| msg.caption().map(|s| s.to_string()));

    let mut context_parts: Vec<String> = Vec::new();

    // Handle photo attachments
    if let Some(photos) = msg.photo() {
        // photos is a Vec<PhotoSize>, last is highest resolution
        if let Some(largest) = photos.last() {
            let file_id = largest.file.id.clone();
            let width = largest.width;
            let height = largest.height;
            let path = download::download_file(bot, file_id, upload_dir, "photo.jpg").await?;
            context_parts.push(format!(
                "[Image attached: {} (resolution: {}x{})]",
                path.display(),
                width,
                height
            ));
        }
    }

    // Handle document/file attachments
    if let Some(doc) = msg.document() {
        let file_id = doc.file.id.clone();
        let filename = doc.file_name.as_deref().unwrap_or("unnamed_file");
        let file_size = doc.file.size;
        let path = download::download_file(bot, file_id, upload_dir, filename).await?;
        context_parts.push(format!(
            "[File attached: {} (name: {}, size: {} bytes)]",
            path.display(),
            filename,
            file_size
        ));

        // For text files, include a preview
        let ext = std::path::Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let text_exts = [
            "txt", "md", "json", "csv", "rs", "py", "js", "ts", "toml", "yaml", "yml",
            "xml", "html", "css", "sh", "sql", "log", "cfg", "ini", "env",
        ];
        if text_exts.contains(&ext) {
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                let preview: String = content.chars().take(2000).collect();
                context_parts.push(format!("[File preview of {}]:\n{}", filename, preview));
            }
        }
    }

    Ok((text, context_parts.join("\n\n")))
}

/// Send a message, splitting into chunks if it exceeds Telegram's 4096 char limit.
async fn send_long_message(bot: &Bot, chat_id: ChatId, text: &str) -> anyhow::Result<()> {
    const MAX_LEN: usize = 4000; // leave margin for formatting

    if text.len() <= MAX_LEN {
        bot.send_message(chat_id, text)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
    } else {
        let mut remaining = text;
        while !remaining.is_empty() {
            let split_at = find_split_point(remaining, MAX_LEN);
            let chunk = &remaining[..split_at];
            bot.send_message(chat_id, chunk)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            remaining = remaining[split_at..].trim_start();
        }
    }
    Ok(())
}

fn find_split_point(text: &str, max_len: usize) -> usize {
    if text.len() <= max_len {
        return text.len();
    }
    
    // Ensure we are at a char boundary for max_len
    let mut boundary = max_len;
    while !text.is_char_boundary(boundary) && boundary > 0 {
        boundary -= 1;
    }
    
    if boundary == 0 {
        // Fallback to first char boundary from start
        return text.char_indices().nth(1).map(|(i, _)| i).unwrap_or(text.len());
    }

    // Find last newline before boundary
    if let Some(pos) = text[..boundary].rfind('\n') {
        pos + 1
    } else if let Some(pos) = text[..boundary].rfind(' ') {
        pos + 1
    } else {
        boundary
    }
}
