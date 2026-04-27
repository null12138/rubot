//! WeChat (个人微信) iLink channel — official personal WeChat Bot API.
//!
//! Protocol: pure HTTP/JSON at `ilinkai.weixin.qq.com`
//!   1. GET /ilink/bot/get_bot_qrcode → QR code URL
//!   2. Poll GET /ilink/bot/get_qrcode_status → bot_token + baseurl
//!   3. POST /ilink/bot/getupdates (long-poll, 35s hold) → messages
//!   4. POST /ilink/bot/sendmessage → reply
//!   5. POST /ilink/bot/getconfig + POST /ilink/bot/sendtyping → typing indicator
//!   6. POST /ilink/bot/getuploadurl → CDN pre-signed upload URL (for images/files)
//!
//! Every request carries X-WECHAT-UIN: base64(random_uint32) for anti-replay.
//! Images and files are stored on CDN encrypted with AES-128-ECB.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{anyhow, Result};
use data_encoding::BASE64;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::agent;

const ILINK_DEFAULT_BASE: &str = "https://ilinkai.weixin.qq.com";
const CDN_BASE: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const CHANNEL_VERSION: &str = "1.0.2";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// QR login result.
pub struct QrLoginResult {
    pub bot_token: String,
    pub base_url: String,
}

/// Perform the QR-code login flow.
pub async fn qr_login() -> Result<QrLoginResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    // 1. Get QR code
    let qr_resp: Value = client
        .get(format!(
            "{}/ilink/bot/get_bot_qrcode?bot_type=3",
            ILINK_DEFAULT_BASE
        ))
        .headers(make_headers(None))
        .send()
        .await?
        .json()
        .await?;

    let qrcode_url = qr_resp["qrcode_img_content"]
        .as_str()
        .ok_or_else(|| anyhow!("missing qrcode_img_content in response"))?;
    let qrcode_key = qr_resp["qrcode"]
        .as_str()
        .ok_or_else(|| anyhow!("missing qrcode in response"))?;

    // 2. Print QR code
    println!("\nScan this QR code with WeChat to log in:");
    if let Err(e) = qr2term::print_qr(qrcode_url) {
        tracing::warn!("failed to render QR code: {}, fallback to URL", e);
        println!("QR URL: {}", qrcode_url);
    }
    println!();

    // 3. Poll for scan status
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(120);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("QR code login timed out after 120s");
        }

        tokio::time::sleep(Duration::from_secs(2)).await;

        let status_resp: Value = client
            .get(format!(
                "{}/ilink/bot/get_qrcode_status?qrcode={}",
                ILINK_DEFAULT_BASE, qrcode_key
            ))
            .headers(make_headers(None))
            .send()
            .await?
            .json()
            .await?;

        if let Some(token) = status_resp["bot_token"].as_str() {
            let base_url = status_resp["baseurl"]
                .as_str()
                .unwrap_or(ILINK_DEFAULT_BASE)
                .to_string();
            if !token.is_empty() {
                println!("✓ QR code scanned! Bot token acquired.\n");
                return Ok(QrLoginResult {
                    bot_token: token.to_string(),
                    base_url,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Channel runtime
// ---------------------------------------------------------------------------

pub struct WeChatBot {
    bot_token: String,
    base_url: String,
    agent: Arc<Mutex<agent::Agent>>,
    cursor: String,
    client: reqwest::Client,
    workspace_path: PathBuf,
}

impl WeChatBot {
    pub fn new(
        bot_token: String,
        base_url: String,
        agent: Arc<Mutex<agent::Agent>>,
        workspace_path: PathBuf,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            bot_token,
            base_url,
            agent,
            cursor: String::new(),
            client,
            workspace_path,
        }
    }

    /// Main message polling loop.
    pub async fn run(&mut self) -> Result<()> {
        tracing::info!(
            "wechat bot: starting message poll loop (base={})",
            self.base_url
        );

        tokio::time::sleep(Duration::from_secs(2)).await;

        let mut retry_count = 0u32;

        loop {
            let body = serde_json::json!({
                "get_updates_buf": self.cursor,
                "base_info": { "channel_version": CHANNEL_VERSION },
            });

            let resp = match self
                .client
                .post(format!("{}/ilink/bot/getupdates", self.base_url))
                .headers(make_headers(Some(&self.bot_token)))
                .json(&body)
                .timeout(Duration::from_secs(45))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("wechat bot: poll error: {}, retrying in 3s", e);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            let data: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("wechat bot: poll parse error: {}", e);
                    continue;
                }
            };

            let errcode = data
                .get("errcode")
                .or(data.get("ret"))
                .and_then(|c| c.as_i64())
                .unwrap_or(0);
            if errcode != 0 {
                let errmsg = data["errmsg"].as_str().unwrap_or("");
                tracing::warn!(
                    "wechat bot: api error: errcode={} errmsg={}, full={:?}",
                    errcode,
                    errmsg,
                    data,
                );
                if errcode == -14 {
                    retry_count += 1;
                    if retry_count >= 10 {
                        tracing::error!("wechat bot: giving up after {} retries", retry_count);
                        anyhow::bail!("session expired");
                    }
                    let delay = Duration::from_secs(10 * retry_count as u64);
                    tracing::warn!(
                        "wechat bot: session error, retry {}/10 in {:?}",
                        retry_count,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            retry_count = 0;

            if let Some(buf) = data["get_updates_buf"].as_str() {
                if !buf.is_empty() {
                    self.cursor = buf.to_string();
                }
            }

            if let Some(msgs) = data["msgs"].as_array() {
                for msg in msgs {
                    if msg["message_type"].as_i64() == Some(2) {
                        continue;
                    }
                    self.handle_msg(msg).await;
                }
            }
        }
    }

    async fn handle_msg(&self, msg: &Value) {
        let from_user = msg["from_user_id"].as_str().unwrap_or("");
        let from_name = msg["from_user_name"].as_str().unwrap_or("");
        let context_token = msg["context_token"].as_str().unwrap_or("");

        if from_user.is_empty() || context_token.is_empty() {
            return;
        }

        let items = msg["item_list"].as_array().cloned().unwrap_or_default();

        // Extract text (type 1)
        let text = items
            .iter()
            .filter(|item| item["type"].as_i64() == Some(1))
            .filter_map(|item| item["text_item"]["text"].as_str())
            .collect::<Vec<_>>()
            .join("");

        // Download media items (images/files) from CDN
        let media_refs = self.download_media_items(&items).await;

        // Compose agent input: media refs + user text
        let agent_input = if !media_refs.is_empty() {
            let refs_text: Vec<&str> = media_refs.iter().map(|r| r.label.as_str()).collect();
            let combined = refs_text.join("\n");
            if text.is_empty() {
                combined
            } else {
                format!("{}\n\n{}", combined, text)
            }
        } else {
            text.to_string()
        };

        if agent_input.is_empty() {
            return;
        }

        tracing::info!(
            "wechat msg from={}({}) text={:?}",
            from_name,
            from_user,
            agent_input
        );

        let bot_token = self.bot_token.clone();
        let base_url = self.base_url.clone();
        let agent = self.agent.clone();
        let to_user = from_user.to_string();
        let ctx = context_token.to_string();
        let display_name = from_name.to_string();
        let files_dir = self.workspace_path.join("files");

        tokio::spawn(async move {
            // Send typing indicator
            if let Err(e) = send_typing_sequence(&base_url, &bot_token, &to_user, &ctx).await {
                tracing::debug!("wechat bot: typing error: {}", e);
            }

            // Snapshot files before processing to detect new ones
            let pre_files = snapshot_files(&files_dir);

            let response = match agent.lock().await.process(&agent_input).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("wechat bot: agent.process error: {:#}", e);
                    return;
                }
            };

            // Collect outgoing files from all sources
            let mut outgoing = agent.lock().await.take_channel_send_queue().await;

            // Snapshot-based detection for files created by tools
            for f in snapshot_files(&files_dir) {
                if !pre_files.contains(&f) && !outgoing.contains(&f) {
                    outgoing.push(f);
                }
            }
            // Response text detection for mentioned files
            for f in detect_media_files(&response, &files_dir) {
                if !outgoing.contains(&f) {
                    outgoing.push(f);
                }
            }

            if outgoing.is_empty() {
                if let Err(e) = send_message(&base_url, &bot_token, &to_user, &ctx, &response).await
                {
                    tracing::warn!("wechat bot: send error: {:#}", e);
                } else {
                    tracing::info!(
                        "wechat bot: replied to {} ({} chars)",
                        display_name,
                        response.len()
                    );
                }
            } else {
                match send_rich_message(&base_url, &bot_token, &to_user, &ctx, &response, &outgoing)
                    .await
                {
                    Ok(()) => tracing::info!(
                        "wechat bot: rich message sent to {} ({} files)",
                        display_name,
                        outgoing.len()
                    ),
                    Err(e) => tracing::warn!("wechat bot: rich message error: {:#}", e),
                }
            }
        });
    }

    /// Download media items (images/files) from CDN and save to workspace.
    async fn download_media_items(&self, items: &[Value]) -> Vec<MediaRef> {
        let files_dir = self.workspace_path.join("files");
        let _ = std::fs::create_dir_all(&files_dir);

        let mut refs = Vec::new();
        for item in items {
            let item_type = item["type"].as_i64().unwrap_or(0);
            match item_type {
                2 => {
                    if let Some(img) = item.get("image_item") {
                        let file_key = img["file_key"].as_str().unwrap_or("");
                        let aes_key = img["aes_key"].as_str().unwrap_or("");
                        let query = img["encrypt_query_param"].as_str().unwrap_or("");
                        if file_key.is_empty() {
                            continue;
                        }
                        let save_name = format!("wechat_img_{}.jpg", timestamp_id());
                        let save_path = files_dir.join(&save_name);
                        match download_cdn_file(&self.client, file_key, aes_key, query, &save_path)
                            .await
                        {
                            Ok(()) => {
                                let label = format!("📷 [Received image: files/{}]", save_name);
                                refs.push(MediaRef { label });
                            }
                            Err(e) => tracing::warn!("wechat bot: failed to download image: {}", e),
                        }
                    }
                }
                4 => {
                    if let Some(f) = item.get("file_item") {
                        let file_key = f["file_key"].as_str().unwrap_or("");
                        let aes_key = f["aes_key"].as_str().unwrap_or("");
                        let query = f["encrypt_query_param"].as_str().unwrap_or("");
                        let file_name = f["file_name"].as_str().unwrap_or("unknown");
                        if file_key.is_empty() {
                            continue;
                        }
                        let save_path = files_dir.join(file_name);
                        match download_cdn_file(&self.client, file_key, aes_key, query, &save_path)
                            .await
                        {
                            Ok(()) => {
                                let label = format!("📎 [Received file: files/{}]", file_name);
                                refs.push(MediaRef { label });
                            }
                            Err(e) => tracing::warn!("wechat bot: failed to download file: {}", e),
                        }
                    }
                }
                _ => {}
            }
        }
        refs
    }
}

// ---------------------------------------------------------------------------
// Incoming CDN file download + AES-128-ECB decrypt
// ---------------------------------------------------------------------------

struct MediaRef {
    label: String,
}

/// Download an encrypted file from WeChat CDN, AES-128-ECB decrypt, save to disk.
async fn download_cdn_file(
    client: &reqwest::Client,
    file_key: &str,
    aes_key_b64: &str,
    encrypt_query_param: &str,
    save_path: &Path,
) -> Result<()> {
    let url = format!(
        "{}?file_key={}&encrypt_query_param={}",
        CDN_BASE, file_key, encrypt_query_param
    );

    let encrypted = client.get(&url).send().await?.bytes().await?;
    let decrypted = aes128_ecb_decrypt(&encrypted, aes_key_b64)?;

    if let Some(parent) = save_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(save_path, &decrypted)?;
    tracing::info!(
        "wechat bot: saved {} ({} bytes)",
        save_path.display(),
        decrypted.len()
    );

    Ok(())
}

fn aes128_ecb_decrypt(encrypted: &[u8], aes_key_b64: &str) -> Result<Vec<u8>> {
    let hex_bytes = BASE64
        .decode(aes_key_b64.as_bytes())
        .map_err(|e| anyhow!("base64 decode of aes_key: {}", e))?;
    let hex_str =
        std::str::from_utf8(&hex_bytes).map_err(|e| anyhow!("aes_key not valid utf-8: {}", e))?;
    let key_bytes = hex_decode(hex_str)?;

    let key = GenericArray::from_slice(&key_bytes);
    let cipher = Aes128::new(key);

    let mut data = encrypted.to_vec();
    for chunk in data.chunks_exact_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }

    // Remove PKCS7 padding
    let pad_len = data.last().copied().unwrap_or(0) as usize;
    if pad_len > 0 && pad_len <= 16 && pad_len <= data.len() {
        data.truncate(data.len() - pad_len);
    }

    Ok(data)
}

// ---------------------------------------------------------------------------
// Outgoing file upload helpers (AES-128-ECB encrypt + CDN upload)
// ---------------------------------------------------------------------------

struct UploadedMedia {
    aes_key_b64: String,
    encrypt_query_param: String,
    size: u64,
    is_image: bool,
    file_name: String,
}

async fn upload_media_file(
    base_url: &str,
    bot_token: &str,
    to_user: &str,
    file_path: &Path,
) -> Result<UploadedMedia> {
    let raw_data = std::fs::read(file_path)?;
    let raw_size = raw_data.len() as u64;

    let is_image = matches!(
        file_path.extension().and_then(|e| e.to_str()).unwrap_or(""),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    );
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();

    let key_hex = random_hex_key();
    let encrypted = aes128_ecb_encrypt(&raw_data, &key_hex)?;
    let encrypted_size = encrypted.len() as u64;
    let raw_md5 = md5_hex(&raw_data);
    let file_key = timestamp_id();
    let media_type = if is_image { 1 } else { 3 }; // 1=IMAGE, 3=FILE (per types.ts)

    let upload_url = get_upload_url(
        base_url,
        bot_token,
        to_user,
        &file_key,
        media_type,
        raw_size,
        &raw_md5,
        encrypted_size,
        &key_hex,
    )
    .await?;

    let download_param = upload_to_cdn(&upload_url, &encrypted).await?;

    let aes_key_ascii = BASE64.encode(key_hex.as_bytes());

    Ok(UploadedMedia {
        aes_key_b64: aes_key_ascii,
        encrypt_query_param: download_param,
        size: raw_size,
        is_image,
        file_name,
    })
}

/// Get CDN pre-signed upload URL. Returns the upload_full_url.
async fn get_upload_url(
    base_url: &str,
    bot_token: &str,
    to_user: &str,
    file_key: &str,
    media_type: i64,
    raw_size: u64,
    raw_md5: &str,
    encrypted_size: u64,
    aes_key_hex: &str,
) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let body = serde_json::json!({
        "filekey": file_key,
        "media_type": media_type,
        "to_user_id": to_user,
        "rawsize": raw_size,
        "rawfilemd5": raw_md5,
        "filesize": encrypted_size,
        "no_need_thumb": true,
        "aeskey": aes_key_hex,
    });

    let resp: Value = client
        .post(format!("{}/ilink/bot/getuploadurl", base_url))
        .headers(make_headers(Some(bot_token)))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    tracing::debug!("getuploadurl response: {:?}", resp);

    if let Some(code) = resp
        .get("errcode")
        .or(resp.get("ret"))
        .and_then(|c| c.as_i64())
    {
        if code != 0 {
            anyhow::bail!("getuploadurl failed: errcode={} response={:?}", code, resp);
        }
    }

    let upload_url = resp["upload_full_url"]
        .as_str()
        .or_else(|| resp["upload_url"].as_str())
        .or_else(|| resp.get("upload_param").and_then(|p| p["url"].as_str()))
        .ok_or_else(|| {
            anyhow!(
                "no upload_url found; response keys: {:?}",
                resp.as_object().map(|o| o.keys().collect::<Vec<_>>())
            )
        })?;

    Ok(upload_url.to_string())
}

/// POST encrypted data to CDN. Returns download `encrypt_query_param` from `x-encrypted-param` header.
async fn upload_to_cdn(cdn_url: &str, encrypted_data: &[u8]) -> Result<String> {
    tracing::debug!("upload_to_cdn: size={}", encrypted_data.len());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;

    let resp = client
        .post(cdn_url)
        .header("Content-Type", "application/octet-stream")
        .body(encrypted_data.to_vec())
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("CDN upload failed: HTTP {} body={}", status, body);
    }

    let download_param = resp
        .headers()
        .get("x-encrypted-param")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("CDN response missing x-encrypted-param header"))?
        .to_string();

    tracing::debug!("upload_to_cdn: success, download_param={}", download_param);
    Ok(download_param)
}

fn aes128_ecb_encrypt(plaintext: &[u8], key_hex: &str) -> Result<Vec<u8>> {
    let key_bytes = hex_decode(key_hex)?;
    let key = GenericArray::from_slice(&key_bytes);
    let cipher = Aes128::new(key);

    // PKCS7 padding
    let block_size = 16;
    let pad_len = block_size - (plaintext.len() % block_size);
    let mut data = plaintext.to_vec();
    data.extend(std::iter::repeat(pad_len as u8).take(pad_len));

    for chunk in data.chunks_exact_mut(block_size) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }

    Ok(data)
}

// ---------------------------------------------------------------------------
// Send rich message (text + optional images/files)
// ---------------------------------------------------------------------------

async fn send_rich_message(
    base_url: &str,
    bot_token: &str,
    to_user: &str,
    context_token: &str,
    text: &str,
    media_list: &[PathBuf],
) -> Result<()> {
    // Official openclaw-weixin pattern: each item (text, image, file) is sent
    // as a SEPARATE sendmessage API call with a single item in item_list.

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    // 1. Send text as its own message
    if !text.is_empty() {
        send_single_item(
            &client,
            base_url,
            bot_token,
            to_user,
            context_token,
            serde_json::json!({ "type": 1, "text_item": { "text": text } }),
        )
        .await?;
    }

    // 2. Upload and send each media file as its own message
    for file_path in media_list {
        match upload_media_file(base_url, bot_token, to_user, file_path).await {
            Ok(media) => {
                let item = if media.is_image {
                    serde_json::json!({
                        "type": 2,
                        "image_item": {
                            "media": {
                                "encrypt_query_param": media.encrypt_query_param,
                                "aes_key": media.aes_key_b64,
                                "encrypt_type": 1,
                            },
                            "mid_size": media.size,
                        }
                    })
                } else {
                    serde_json::json!({
                        "type": 4,
                        "file_item": {
                            "media": {
                                "encrypt_query_param": media.encrypt_query_param,
                                "aes_key": media.aes_key_b64,
                                "encrypt_type": 1,
                            },
                            "file_name": media.file_name,
                            "len": media.size.to_string(),
                        }
                    })
                };
                send_single_item(&client, base_url, bot_token, to_user, context_token, item)
                    .await?;
            }
            Err(e) => {
                tracing::warn!(
                    "wechat bot: failed to upload {}: {:#}",
                    file_path.display(),
                    e
                );
            }
        }
    }

    Ok(())
}

/// Send a single item as its own message.
async fn send_single_item(
    client: &reqwest::Client,
    base_url: &str,
    bot_token: &str,
    to_user: &str,
    context_token: &str,
    item: serde_json::Value,
) -> Result<()> {
    let body = serde_json::json!({
        "msg": {
            "from_user_id": "",
            "to_user_id": to_user,
            "client_id": format!("rubot-{}", timestamp_id()),
            "message_type": 2,
            "message_state": 2,
            "context_token": context_token,
            "item_list": [item],
        },
        "base_info": { "channel_version": CHANNEL_VERSION },
    });

    let resp: Value = client
        .post(format!("{}/ilink/bot/sendmessage", base_url))
        .headers(make_headers(Some(bot_token)))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    if let Some(code) = resp
        .get("errcode")
        .or(resp.get("ret"))
        .and_then(|c| c.as_i64())
    {
        if code != 0 {
            let msg = resp["errmsg"].as_str().unwrap_or("");
            anyhow::bail!("sendmessage failed: errcode={} errmsg={}", code, msg);
        }
    }

    Ok(())
}

/// Detect file paths in agent response text that exist on disk.
fn detect_media_files(response: &str, files_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for word in response.split_whitespace() {
        let clean = word
            .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '/' && c != '-');
        let path = PathBuf::from(clean);

        if path.is_absolute() && path.is_file() {
            found.push(path);
        } else {
            let full = files_dir.join(&path);
            if full.is_file() && full.starts_with(files_dir) {
                found.push(full);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

// ---------------------------------------------------------------------------
// Send text reply
// ---------------------------------------------------------------------------

pub async fn send_message(
    base_url: &str,
    bot_token: &str,
    to_user: &str,
    context_token: &str,
    text: &str,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;

    let body = serde_json::json!({
        "msg": {
            "from_user_id": "",
            "to_user_id": to_user,
            "client_id": format!("rubot-{}", timestamp_id()),
            "message_type": 2,
            "message_state": 2,
            "context_token": context_token,
            "item_list": [{ "type": 1, "text_item": { "text": text } }],
        },
        "base_info": { "channel_version": CHANNEL_VERSION },
    });

    let resp: Value = client
        .post(format!("{}/ilink/bot/sendmessage", base_url))
        .headers(make_headers(Some(bot_token)))
        .json(&body)
        .send()
        .await?
        .json()
        .await?;

    if let Some(errcode) = resp["errcode"].as_i64() {
        if errcode != 0 {
            anyhow::bail!(
                "sendmessage failed: errcode={} errmsg={:?}",
                errcode,
                resp["errmsg"]
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Typing indicator
// ---------------------------------------------------------------------------

async fn send_typing_sequence(
    base_url: &str,
    bot_token: &str,
    user_id: &str,
    context_token: &str,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let config_body = serde_json::json!({
        "ilink_user_id": user_id,
        "context_token": context_token,
        "base_info": { "channel_version": CHANNEL_VERSION },
    });

    let config_resp: Value = client
        .post(format!("{}/ilink/bot/getconfig", base_url))
        .headers(make_headers(Some(bot_token)))
        .json(&config_body)
        .send()
        .await?
        .json()
        .await?;

    let ticket = match config_resp["typing_ticket"].as_str() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return Ok(()),
    };

    let typing_body = serde_json::json!({
        "ilink_user_id": user_id,
        "typing_ticket": ticket,
        "status": 1,
        "base_info": { "channel_version": CHANNEL_VERSION },
    });

    let _ = client
        .post(format!("{}/ilink/bot/sendtyping", base_url))
        .headers(make_headers(Some(bot_token)))
        .json(&typing_body)
        .send()
        .await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_headers(token: Option<&str>) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(
        reqwest::header::HeaderName::from_static("authorizationtype"),
        HeaderValue::from_static("ilink_bot_token"),
    );
    h.insert(
        reqwest::header::HeaderName::from_static("x-wechat-uin"),
        HeaderValue::from_str(&random_uin()).unwrap(),
    );
    if let Some(t) = token {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", t)) {
            h.insert(AUTHORIZATION, v);
        }
    }
    h
}

fn random_uin() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let uin = (ts & 0xFFFF_FFFF) as u32;
    BASE64.encode(&uin.to_le_bytes())
}

fn timestamp_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn random_hex_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let low = ts as u64;
    let high = (ts >> 64) as u64;
    format!("{:016x}{:016x}", high, low)
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        anyhow::bail!("invalid hex length: {}", s.len());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| anyhow!("hex decode error at byte {}: {}", i / 2, e))
        })
        .collect()
}

fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    use std::fmt::Write;
    let hash = md5::Md5::digest(data);
    let mut hex = String::with_capacity(32);
    for byte in hash.iter() {
        write!(hex, "{:02x}", byte).unwrap();
    }
    hex
}

/// List files in a directory for change detection (sorted).
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
