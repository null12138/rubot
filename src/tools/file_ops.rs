use super::registry::{Tool, ToolResult};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub struct FileOps {
    workspace: PathBuf,
    files_dir: PathBuf,
}
impl FileOps {
    pub fn new(ws: &Path) -> Self {
        let workspace = ws.canonicalize().unwrap_or_else(|_| ws.to_path_buf());
        let d = workspace.join("files");
        let _ = std::fs::create_dir_all(&d);
        Self {
            workspace,
            files_dir: d.canonicalize().unwrap_or(d),
        }
    }
    fn p(&self, s: &str) -> Result<PathBuf> {
        let path = Path::new(s);
        if s.trim().is_empty() {
            return Err(anyhow!("path is empty"));
        }

        if path.is_absolute() {
            let rel = path.strip_prefix(&self.workspace).map_err(|_| {
                anyhow!(
                    "path '{}' is outside workspace '{}'",
                    path.display(),
                    self.workspace.display()
                )
            })?;
            return Ok(normalize_under(&self.workspace, rel));
        }

        let base = match path.components().next() {
            Some(std::path::Component::Normal(first))
                if first == OsStr::new("files")
                    || first == OsStr::new("tools")
                    || first == OsStr::new("memory") =>
            {
                &self.workspace
            }
            _ => &self.files_dir,
        };
        Ok(normalize_under(base, path))
    }
}

fn normalize_under(base: &Path, input: &Path) -> PathBuf {
    let mut out = base.to_path_buf();
    for component in input.components() {
        match component {
            std::path::Component::Normal(c) => out.push(c),
            std::path::Component::ParentDir => {
                if out != base {
                    out.pop();
                }
            }
            std::path::Component::CurDir => {}
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {}
        }
    }
    out
}

#[async_trait]
impl Tool for FileOps {
    fn name(&self) -> &str {
        "file_ops"
    }
    fn description(&self) -> &str {
        "Read, write, append, or list files in the workspace. Bare relative paths default to workspace `files/`; paths rooted at `files/`, `tools/`, or `memory/`, plus absolute paths inside the workspace, are also allowed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"act": {"type": "string", "enum": ["read", "write", "list", "append"]}, "path": {"type": "string"}, "content": {"type": "string"}}, "required": ["act", "path"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let act = params["act"].as_str().unwrap_or("");
        let path = self.p(params["path"].as_str().unwrap_or(""))?;
        match act {
            "read" => tokio::fs::read_to_string(path)
                .await
                .map(ToolResult::ok)
                .or_else(|e| Ok(ToolResult::err(e.to_string()))),
            "write" | "append" => {
                if let Some(p) = path.parent() {
                    let _ = tokio::fs::create_dir_all(p).await;
                }
                let c = params["content"].as_str().unwrap_or("");
                let res = if act == "write" {
                    tokio::fs::write(&path, c).await
                } else {
                    use tokio::io::AsyncWriteExt;
                    tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .await?
                        .write_all(c.as_bytes())
                        .await
                };
                res.map(|_| {
                    ToolResult::ok(format!("Done: {}", params["path"].as_str().unwrap_or("")))
                })
                .or_else(|e| Ok(ToolResult::err(e.to_string())))
            }
            "list" => {
                let mut fs = vec![];
                let list_dir = if path.is_dir() {
                    &path
                } else {
                    &self.files_dir
                };
                let mut rd = tokio::fs::read_dir(list_dir).await?;
                while let Ok(Some(e)) = rd.next_entry().await {
                    fs.push(format!(
                        "{}{}",
                        e.file_name().to_string_lossy(),
                        if e.file_type().await?.is_dir() {
                            "/"
                        } else {
                            ""
                        }
                    ));
                }
                fs.sort();
                Ok(ToolResult::ok(if fs.is_empty() {
                    "(empty)".into()
                } else {
                    fs.join("\n")
                }))
            }
            _ => Ok(ToolResult::err("Bad act".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FileOps;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!(
            "rubot-file-ops-test-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(dir.join("files")).unwrap();
        dir
    }

    #[test]
    fn tools_paths_resolve_under_workspace_root() {
        let workspace = temp_workspace();
        let ops = FileOps::new(&workspace);
        let path = ops.p("tools/example.md").unwrap();
        assert_eq!(path, ops.workspace.join("tools/example.md"));
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn absolute_paths_outside_workspace_are_rejected() {
        let workspace = temp_workspace();
        let ops = FileOps::new(&workspace);
        let err = ops.p("/tmp/not-in-workspace.txt").unwrap_err().to_string();
        assert!(err.contains("outside workspace"));
        let _ = std::fs::remove_dir_all(workspace);
    }
}
