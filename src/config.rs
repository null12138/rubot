use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub api_base_url: String,
    pub api_key: String,
    pub model: String,
    pub fast_model: String,
    pub workspace_path: PathBuf,
    pub max_context_tokens: usize,
    pub max_retries: u32,
    pub code_exec_timeout_secs: u64,
    pub telegram_bot_token: Option<String>,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();

        let api_base_url = std::env::var("RUBOT_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let api_key =
            std::env::var("RUBOT_API_KEY").unwrap_or_else(|_| "sk-placeholder".to_string());
        let model = std::env::var("RUBOT_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
        let fast_model = std::env::var("RUBOT_FAST_MODEL")
            .unwrap_or_else(|_| model.clone());
        let workspace_path = std::env::var("RUBOT_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("workspace"));
        let max_context_tokens = std::env::var("RUBOT_MAX_CONTEXT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120_000);
        let max_retries = std::env::var("RUBOT_MAX_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let code_exec_timeout_secs = std::env::var("RUBOT_CODE_EXEC_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let telegram_bot_token = std::env::var("RUBOT_TELEGRAM_BOT_TOKEN").ok();

        Ok(Self {
            api_base_url,
            api_key,
            model,
            fast_model,
            workspace_path,
            max_context_tokens,
            max_retries,
            code_exec_timeout_secs,
            telegram_bot_token,
        })
    }

    pub fn ensure_workspace_dirs(&self) -> anyhow::Result<()> {
        let dirs = [
            "files",
            "memory/working",
            "memory/episodic",
            "memory/semantic",
            "state",
            "errors",
            "tools",
            "tg_uploads",
        ];
        for dir in &dirs {
            std::fs::create_dir_all(self.workspace_path.join(dir))?;
        }
        // Create index file if missing
        let index_path = self.workspace_path.join("memory/memory_index.md");
        if !index_path.exists() {
            std::fs::write(
                &index_path,
                "# Memory Index\n\n<!-- L0: filename → one-line summary -->\n",
            )?;
        }
        // Create error book if missing
        let error_book_path = self.workspace_path.join("errors/error_book.md");
        if !error_book_path.exists() {
            std::fs::write(
                &error_book_path,
                "# Error Book\n\n<!-- pattern → solution index -->\n",
            )?;
        }
        // Create tool manifest if missing
        let manifest_path = self.workspace_path.join("tools/manifest.json");
        if !manifest_path.exists() {
            std::fs::write(
                &manifest_path,
                r#"{"version": 1, "tools": []}"#,
            )?;
        }
        Ok(())
    }
}
