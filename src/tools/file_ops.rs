use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use super::registry::{Tool, ToolResult};

pub struct FileOps { dir: PathBuf }
impl FileOps {
    pub fn new(ws: &Path) -> Self { 
        let d = ws.join("files");
        let _ = std::fs::create_dir_all(&d);
        Self { dir: d.canonicalize().unwrap_or(d) } 
    }
    fn p(&self, s: &str) -> Result<PathBuf> {
        // 强制将所有路径视为相对于 self.dir，并移除 ../ 等危险组件
        let mut p = self.dir.clone();
        for component in Path::new(s).components() {
            match component {
                std::path::Component::Normal(_c) => p.push(component),
                _ => {} // 忽略绝对根路径、当前路径(.)和父路径(..)
            }
        }
        Ok(p)
    }
}

#[async_trait]
impl Tool for FileOps {
    fn name(&self) -> &str { "file_ops" }
    fn description(&self) -> &str { "Read, write, append, or list files in the sandbox." }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"act": {"type": "string", "enum": ["read", "write", "list", "append"]}, "path": {"type": "string"}, "content": {"type": "string"}}, "required": ["act", "path"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let act = params["act"].as_str().unwrap_or("");
        let path = self.p(params["path"].as_str().unwrap_or(""))?;
        match act {
            "read" => tokio::fs::read_to_string(path).await.map(ToolResult::ok).or_else(|e| Ok(ToolResult::err(e.to_string()))),
            "write" | "append" => {
                if let Some(p) = path.parent() { let _ = tokio::fs::create_dir_all(p).await; }
                let c = params["content"].as_str().unwrap_or("");
                let res = if act == "write" { tokio::fs::write(&path, c).await }
                    else { use tokio::io::AsyncWriteExt; tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await?.write_all(c.as_bytes()).await };
                res.map(|_| ToolResult::ok(format!("Done: {}", params["path"].as_str().unwrap_or("")))).or_else(|e| Ok(ToolResult::err(e.to_string())))
            }
            "list" => {
                let mut fs = vec![];
                let mut rd = tokio::fs::read_dir(if path.is_dir() { &path } else { &self.dir }).await?;
                while let Ok(Some(e)) = rd.next_entry().await { fs.push(format!("{}{}", e.file_name().to_string_lossy(), if e.file_type().await?.is_dir() { "/" } else { "" })); }
                fs.sort();
                Ok(ToolResult::ok(if fs.is_empty() { "(empty)".into() } else { fs.join("\n") }))
            }
            _ => Ok(ToolResult::err("Bad act".into()))
        }
    }
}
