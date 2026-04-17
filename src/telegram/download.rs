use std::path::{Path, PathBuf};
use teloxide::prelude::*;
use teloxide::types::FileId;
use teloxide::net::Download;
use chrono::Utc;

/// Download a file from Telegram by file_id.
/// Returns the local path where the file was saved.
pub async fn download_file(
    bot: &Bot,
    file_id: FileId,
    upload_dir: &Path,
    base_name: &str,
) -> anyhow::Result<PathBuf> {
    let tg_file = bot.get_file(file_id).await?;

    // Generate unique filename: stem_YYYYMMDD_HHMMSS.ext
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let path = Path::new(base_name);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("bin");
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let filename = format!("{}_{}.{}", stem, timestamp, ext);
    let save_path = upload_dir.join(&filename);

    // Download using teloxide's Download trait
    // download_file takes (&str file_path, &mut AsyncWrite destination)
    let mut dest = tokio::fs::File::create(&save_path).await?;
    bot.download_file(&tg_file.path, &mut dest).await?;

    tracing::info!("Downloaded TG file to: {}", save_path.display());
    Ok(save_path)
}
