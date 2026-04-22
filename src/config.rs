use anyhow::{anyhow, bail};
use std::path::{Path, PathBuf};

const ENV_FILE_NAME: &str = ".env";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKey {
    ApiBaseUrl,
    ApiKey,
    Model,
    FastModel,
    Workspace,
    MaxRetries,
    CodeExecTimeout,
}

impl ConfigKey {
    pub fn parse(input: &str) -> Option<Self> {
        let normalized = input.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "api_base_url" | "base_url" | "url" | "rubot_api_base_url" => Some(Self::ApiBaseUrl),
            "api_key" | "key" | "rubot_api_key" => Some(Self::ApiKey),
            "model" | "rubot_model" => Some(Self::Model),
            "fast_model" | "fast" | "rubot_fast_model" => Some(Self::FastModel),
            "workspace" | "workspace_path" | "rubot_workspace" => Some(Self::Workspace),
            "max_retries" | "retries" | "rubot_max_retries" => Some(Self::MaxRetries),
            "code_exec_timeout" | "timeout" | "rubot_code_exec_timeout" => {
                Some(Self::CodeExecTimeout)
            }
            _ => None,
        }
    }

    pub fn all() -> [Self; 7] {
        [
            Self::ApiBaseUrl,
            Self::ApiKey,
            Self::Model,
            Self::FastModel,
            Self::Workspace,
            Self::MaxRetries,
            Self::CodeExecTimeout,
        ]
    }

    pub fn env_name(&self) -> &'static str {
        match self {
            Self::ApiBaseUrl => "RUBOT_API_BASE_URL",
            Self::ApiKey => "RUBOT_API_KEY",
            Self::Model => "RUBOT_MODEL",
            Self::FastModel => "RUBOT_FAST_MODEL",
            Self::Workspace => "RUBOT_WORKSPACE",
            Self::MaxRetries => "RUBOT_MAX_RETRIES",
            Self::CodeExecTimeout => "RUBOT_CODE_EXEC_TIMEOUT",
        }
    }

    pub fn cli_name(&self) -> &'static str {
        match self {
            Self::ApiBaseUrl => "api_base_url",
            Self::ApiKey => "api_key",
            Self::Model => "model",
            Self::FastModel => "fast_model",
            Self::Workspace => "workspace",
            Self::MaxRetries => "max_retries",
            Self::CodeExecTimeout => "code_exec_timeout",
        }
    }

    pub fn validate(&self, raw: &str) -> anyhow::Result<String> {
        let value = raw.trim();
        if value.is_empty() {
            bail!("value cannot be empty");
        }
        match self {
            Self::MaxRetries => value
                .parse::<u32>()
                .map(|n| n.to_string())
                .map_err(|_| anyhow!("max_retries must be a non-negative integer")),
            Self::CodeExecTimeout => value
                .parse::<u64>()
                .map(|n| n.to_string())
                .map_err(|_| anyhow!("code_exec_timeout must be a non-negative integer")),
            _ => Ok(value.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigRow {
    pub key: ConfigKey,
    pub env_name: &'static str,
    pub display_value: String,
}

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
        let api_key =
            std::env::var("RUBOT_API_KEY").unwrap_or_else(|_| "sk-placeholder".to_string());
        let model = std::env::var("RUBOT_MODEL").unwrap_or_else(|_| "gpt-4o".to_string());
        let fast_model = std::env::var("RUBOT_FAST_MODEL").unwrap_or_else(|_| model.clone());
        let workspace_path = std::env::var("RUBOT_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("workspace"));
        let workspace_path = absolutize_workspace_path(workspace_path)?;
        let max_retries = std::env::var("RUBOT_MAX_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let code_exec_timeout_secs = std::env::var("RUBOT_CODE_EXEC_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);

        Ok(Self {
            api_base_url,
            api_key,
            model,
            fast_model,
            workspace_path,
            max_retries,
            code_exec_timeout_secs,
        })
    }

    pub fn ensure_workspace_dirs(&self) -> anyhow::Result<()> {
        for d in [
            "files",
            "tools",
            "memory/working",
            "memory/episodic",
            "memory/semantic",
        ] {
            std::fs::create_dir_all(self.workspace_path.join(d))?;
        }
        ensure_gitignore(&self.workspace_path)?;
        Ok(())
    }

    pub fn rows(&self) -> Vec<ConfigRow> {
        ConfigKey::all()
            .into_iter()
            .map(|key| {
                let value = self.value_for_key(key);
                let display_value = if key == ConfigKey::ApiKey {
                    mask_secret(&value)
                } else {
                    value.clone()
                };
                ConfigRow {
                    key,
                    env_name: key.env_name(),
                    display_value,
                }
            })
            .collect()
    }

    pub fn value_for_key(&self, key: ConfigKey) -> String {
        match key {
            ConfigKey::ApiBaseUrl => self.api_base_url.clone(),
            ConfigKey::ApiKey => self.api_key.clone(),
            ConfigKey::Model => self.model.clone(),
            ConfigKey::FastModel => self.fast_model.clone(),
            ConfigKey::Workspace => self.workspace_path.display().to_string(),
            ConfigKey::MaxRetries => self.max_retries.to_string(),
            ConfigKey::CodeExecTimeout => self.code_exec_timeout_secs.to_string(),
        }
    }
}

pub fn env_file_path() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_dir()?.join(ENV_FILE_NAME))
}

pub fn save_config_value(key: ConfigKey, raw_value: &str) -> anyhow::Result<PathBuf> {
    let value = key.validate(raw_value)?;
    std::env::set_var(key.env_name(), &value);

    let path = env_file_path()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = upsert_env_value(&existing, key.env_name(), &value);
    std::fs::write(&path, updated)?;
    Ok(path)
}

fn upsert_env_value(existing: &str, env_name: &str, value: &str) -> String {
    let encoded = encode_env_value(value);
    let replacement = format!("{}={}", env_name, encoded);

    let mut replaced = false;
    let mut lines = Vec::new();
    for line in existing.lines() {
        if matches_env_assignment(line, env_name) {
            lines.push(replacement.clone());
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !replaced {
        lines.push(replacement);
    }

    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn matches_env_assignment(line: &str, env_name: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.is_empty() {
        return false;
    }
    let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let Some((key, _)) = trimmed.split_once('=') else {
        return false;
    };
    key.trim() == env_name
}

fn encode_env_value(value: &str) -> String {
    if value.is_empty() {
        return "\"\"".into();
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '+'))
    {
        return value.to_string();
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{}\"", escaped)
}

fn mask_secret(value: &str) -> String {
    if value.is_empty() {
        return "(empty)".into();
    }
    if value.len() <= 8 {
        return "********".into();
    }
    format!("{}***{}", &value[..4], &value[value.len() - 4..])
}

fn absolutize_workspace_path(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    Ok(std::env::current_dir()?.join(path))
}

fn ensure_gitignore(workspace: &Path) -> anyhow::Result<()> {
    let path = workspace.join(".gitignore");
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    for line in [".DS_Store"] {
        if !existing.lines().any(|l| l.trim() == line) {
            if !existing.is_empty() && !existing.ends_with('\n') {
                existing.push('\n');
            }
            existing.push_str(line);
            existing.push('\n');
        }
    }
    std::fs::write(&path, existing)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{upsert_env_value, ConfigKey};

    #[test]
    fn upsert_updates_existing_assignment() {
        let src = "RUBOT_MODEL=gpt-4o\nRUBOT_API_KEY=abc\n";
        let out = upsert_env_value(src, "RUBOT_MODEL", "gpt-5");
        assert!(out.contains("RUBOT_MODEL=gpt-5"));
        assert!(out.contains("RUBOT_API_KEY=abc"));
        assert_eq!(out.matches("RUBOT_MODEL=").count(), 1);
    }

    #[test]
    fn upsert_appends_missing_assignment() {
        let out = upsert_env_value("", "RUBOT_MODEL", "gpt-5");
        assert_eq!(out, "RUBOT_MODEL=gpt-5\n");
    }

    #[test]
    fn parse_key_accepts_cli_aliases() {
        assert_eq!(ConfigKey::parse("model"), Some(ConfigKey::Model));
        assert_eq!(
            ConfigKey::parse("RUBOT_WORKSPACE"),
            Some(ConfigKey::Workspace)
        );
        assert_eq!(
            ConfigKey::parse("timeout"),
            Some(ConfigKey::CodeExecTimeout)
        );
    }
}
