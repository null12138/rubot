use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub api_base_url: String,
    pub api_key: String,
    pub model: String,
    pub fast_model: String,
    pub workspace_path: PathBuf,
    pub max_retries: u32,
    pub code_exec_timeout_secs: u64,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();

        let api_base_url = std::env::var("RUBOT_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let api_key = std::env::var("RUBOT_API_KEY")
            .unwrap_or_else(|_| "sk-placeholder".to_string());
        let model = std::env::var("RUBOT_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
        let fast_model = std::env::var("RUBOT_FAST_MODEL").unwrap_or_else(|_| model.clone());
        let workspace_path = std::env::var("RUBOT_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("workspace"));
        let max_retries = std::env::var("RUBOT_MAX_RETRIES")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(3);
        let code_exec_timeout_secs = std::env::var("RUBOT_CODE_EXEC_TIMEOUT")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(30);

        Ok(Self {
            api_base_url, api_key, model, fast_model, workspace_path,
            max_retries, code_exec_timeout_secs,
        })
    }

    pub fn ensure_workspace_dirs(&self) -> anyhow::Result<()> {
        for d in ["files", "tools", "memory/working", "memory/episodic", "memory/semantic"] {
            std::fs::create_dir_all(self.workspace_path.join(d))?;
        }
        ensure_gitignore(&self.workspace_path)?;
        Ok(())
    }
}

fn ensure_gitignore(workspace: &Path) -> anyhow::Result<()> {
    let path = workspace.join(".gitignore");
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    for line in [".DS_Store"] {
        if !existing.lines().any(|l| l.trim() == line) {
            if !existing.is_empty() && !existing.ends_with('\n') { existing.push('\n'); }
            existing.push_str(line);
            existing.push('\n');
        }
    }
    std::fs::write(&path, existing)?;
    Ok(())
}
