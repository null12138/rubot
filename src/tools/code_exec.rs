use super::registry::{RiskLevel, Tool, ToolResult};
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::process::Command;

pub struct CodeExec {
    timeout: u64,
    dir: PathBuf,
    files_dir: PathBuf,
}
impl CodeExec {
    pub fn new(t: u64, cwd: &Path, ws: &Path) -> Self {
        let files_dir = ws.join("files");
        let _ = std::fs::create_dir_all(&files_dir);
        Self {
            timeout: t,
            dir: cwd.to_path_buf(),
            files_dir: files_dir.canonicalize().unwrap_or(files_dir),
        }
    }
}

#[async_trait]
impl Tool for CodeExec {
    fn is_concurrency_safe(&self) -> bool { false }
    fn risk_level(&self) -> RiskLevel { RiskLevel::High }
    fn name(&self) -> &str {
        "code_exec"
    }
    fn description(&self) -> &str {
        "Run bash or python in the directory where rubot was launched. If files are created, reference the returned absolute paths directly and never base64-encode them."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"lang": {"type": "string", "enum": ["bash", "python"]}, "code": {"type": "string"}}, "required": ["lang", "code"]})
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let lang = params["lang"]
            .as_str()
            .or_else(|| params["language"].as_str())
            .unwrap_or("bash");
        let code = params["code"].as_str().unwrap_or("");
        let (cmd, args) = match lang {
            "python" | "py" | "python3" => {
                let py = if cfg!(target_os = "windows") {
                    "python"
                } else {
                    "python3"
                };
                (py, vec!["-c", code])
            }
            _ => {
                if cfg!(target_os = "windows") {
                    ("powershell", vec!["-Command", code])
                } else {
                    ("bash", vec!["-c", code])
                }
            }
        };

        let started_at = SystemTime::now();

        let mut child = Command::new(cmd);
        child.args(&args).current_dir(&self.dir);

        let exec_res =
            tokio::time::timeout(std::time::Duration::from_secs(self.timeout), child.output())
                .await;

        let (ok, mut res) = match exec_res {
            Ok(Ok(o)) => {
                let mut res = String::from_utf8_lossy(&o.stdout).to_string();
                let err = String::from_utf8_lossy(&o.stderr).to_string();
                if !err.is_empty() {
                    res.push_str(&format!("\n[stderr] {}", err));
                }
                if o.status.success() {
                    (true, res)
                } else {
                    (
                        false,
                        format!("Exit {}: {}", o.status.code().unwrap_or(-1), res),
                    )
                }
            }
            Ok(Err(e)) => return Ok(ToolResult::err(e.to_string())),
            Err(_) => return Ok(ToolResult::err("Timeout".into())),
        };

        // Snapshot files created or modified during this run — surface them so
        // the LLM knows where its artefacts actually live (prevents the
        // "here's a base64 dump" anti-pattern).
        // Scan both CWD and workspace/files; deduplicate by path.
        let mut generated = scan_new_files(&self.dir, started_at);
        if self.dir != self.files_dir {
            let ws_gen = scan_new_files(&self.files_dir, started_at);
            for (path, size) in ws_gen {
                if !generated.iter().any(|(p, _)| p == &path) {
                    generated.push((path, size));
                }
            }
        }
        generated.sort_by(|a, b| a.0.cmp(&b.0));
        if !generated.is_empty() {
            res.push_str("\n\n[Generated files — absolute paths are directly accessible to the user. Reference these paths in your reply; use `[FILE: /abs/path]` markers for auto-attachment on Telegram. Never base64-encode.]\n");
            for (path, size) in &generated {
                res.push_str(&format!("- {} ({})\n", path.display(), human_size(*size)));
            }
        }

        if ok {
            Ok(ToolResult::ok(res))
        } else {
            Ok(ToolResult::err(res))
        }
    }
}

/// Walk `dir` (one level deep, plus immediate subdirs) and return files
/// whose mtime is at or after `since`. Skips dotfiles and anything larger
/// than 1 GiB (protective cap).
pub(crate) fn scan_new_files(dir: &Path, since: SystemTime) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for ent in entries.flatten() {
            let p = ent.path();
            let name = ent.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            let Ok(meta) = ent.metadata() else { continue };
            if meta.is_dir() {
                // Only descend one level into workspace/files to avoid blowing up.
                if d == dir {
                    stack.push(p);
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let size = meta.len();
            if size > 1024 * 1024 * 1024 {
                continue;
            }
            let Ok(mt) = meta.modified() else { continue };
            if mt >= since {
                out.push((p, size));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub(crate) fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
