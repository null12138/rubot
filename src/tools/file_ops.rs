use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

use super::registry::{Tool, ToolResult};

pub struct FileOps {
    files_dir: PathBuf,
}

impl FileOps {
    pub fn new(workspace: &Path) -> Self {
        Self {
            files_dir: workspace.join("files"),
        }
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf> {
        let resolved = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.files_dir.join(path)
        };
        // Security: ensure path is within files dir (or workspace for absolute)
        let canonical_base = self.files_dir.canonicalize().unwrap_or(self.files_dir.clone());
        let canonical = resolved
            .canonicalize()
            .unwrap_or_else(|_| resolved.clone());
        if !canonical.starts_with(&canonical_base) && !resolved.starts_with(&self.files_dir) {
            anyhow::bail!("Path escapes workspace: {}", path);
        }
        Ok(resolved)
    }
}

#[async_trait]
impl Tool for FileOps {
    fn name(&self) -> &str {
        "file_ops"
    }

    fn description(&self) -> &str {
        "Read, write, append, or list files in your workspace. All paths are relative to a private files directory — you do not need to worry about where files are stored."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["read", "write", "list", "append"],
                    "description": "The file operation to perform"
                },
                "path": {
                    "type": "string",
                    "description": "File path relative to workspace files directory"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write (for write/append actions)"
                }
            },
            "required": ["action", "path"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let action = params["action"].as_str().unwrap_or("");
        let path_str = params["path"].as_str().unwrap_or("");

        if path_str.is_empty() {
            return Ok(ToolResult::err("Missing 'path' parameter".to_string()));
        }

        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::err(format!("Invalid path: {}", e))),
        };

        match action {
            "read" => match tokio::fs::read_to_string(&path).await {
                Ok(content) => Ok(ToolResult::ok(content)),
                Err(e) => Ok(ToolResult::err(format!("Read failed: {}", e))),
            },
            "write" => {
                let content = params["content"].as_str().unwrap_or("");
                if let Some(parent) = path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                match tokio::fs::write(&path, content).await {
                    Ok(()) => Ok(ToolResult::ok(format!("Written to {}", path_str))),
                    Err(e) => Ok(ToolResult::err(format!("Write failed: {}", e))),
                }
            }
            "append" => {
                let content = params["content"].as_str().unwrap_or("");
                use tokio::io::AsyncWriteExt;
                match tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
                {
                    Ok(mut f) => match f.write_all(content.as_bytes()).await {
                        Ok(()) => Ok(ToolResult::ok(format!("Appended to {}", path_str))),
                        Err(e) => Ok(ToolResult::err(format!("Append failed: {}", e))),
                    },
                    Err(e) => Ok(ToolResult::err(format!("Open failed: {}", e))),
                }
            }
            "list" => {
                let target = if path.is_dir() { &path } else { &self.files_dir };
                match tokio::fs::read_dir(target).await {
                    Ok(mut entries) => {
                        let mut files = Vec::new();
                        while let Ok(Some(entry)) = entries.next_entry().await {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let ft = entry.file_type().await.ok();
                            let marker = if ft.map_or(false, |f| f.is_dir()) {
                                "/"
                            } else {
                                ""
                            };
                            files.push(format!("{}{}", name, marker));
                        }
                        files.sort();
                        Ok(ToolResult::ok(files.join("\n")))
                    }
                    Err(e) => Ok(ToolResult::err(format!("List failed: {}", e))),
                }
            }
            _ => Ok(ToolResult::err(format!("Unknown action: {}", action))),
        }
    }
}
