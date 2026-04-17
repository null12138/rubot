use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};

pub struct ErrorBook {
    path: PathBuf,
    pub(crate) entries: Vec<ErrorEntry>,
}

#[derive(Debug, Clone)]
pub struct ErrorEntry {
    pub id: String,
    pub title: String,
    pub patterns: Vec<String>,
    pub solution: String,
    pub occurrences: Vec<String>,
}

impl ErrorBook {
    pub async fn load(workspace: &Path) -> Result<Self> {
        let path = workspace.join("errors/error_book.md");
        let content = tokio::fs::read_to_string(&path)
            .await
            .unwrap_or_else(|_| "# Error Book\n\n".to_string());
        Ok(Self { path, entries: parse_error_book(&content) })
    }

    pub fn is_known_error(&self, error_msg: &str) -> bool {
        let lower = error_msg.to_lowercase();
        self.entries.iter().any(|e| {
            e.patterns.iter().any(|p| lower.contains(&p.to_lowercase()))
        })
    }

    pub async fn find_fix(&self, error_msg: &str) -> Option<String> {
        let lower = error_msg.to_lowercase();
        for entry in &self.entries {
            if entry.patterns.iter().any(|p| lower.contains(&p.to_lowercase())) {
                return Some(entry.solution.clone());
            }
        }
        None
    }

    pub async fn record_error(&mut self, tool: &str, error_msg: &str) -> Result<()> {
        let lower = error_msg.to_lowercase();
        let now = Utc::now().format("%Y-%m-%d").to_string();

        // Update existing entry if pattern matches
        for entry in &mut self.entries {
            if entry.patterns.iter().any(|p| lower.contains(&p.to_lowercase())) {
                entry.occurrences.push(now.clone());
                self.save().await?;
                return Ok(());
            }
        }

        // New error
        self.entries.push(ErrorEntry {
            id: format!("err_{}", self.entries.len() + 1),
            title: format!("{} error", tool),
            patterns: extract_error_patterns(error_msg),
            solution: "(pending analysis — needs LLM classification)".to_string(),
            occurrences: vec![now],
        });
        self.save().await?;
        Ok(())
    }

    pub async fn update_solution(&mut self, error_id: &str, solution: &str) -> Result<()> {
        for entry in &mut self.entries {
            if entry.id == error_id {
                entry.solution = solution.to_string();
                self.save().await?;
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn to_text(&self) -> String {
        let mut text = String::from("# Error Book\n\n");
        for entry in &self.entries {
            text.push_str(&format!("## [{}] {}\n", entry.id, entry.title));
            text.push_str(&format!(
                "- **patterns**: {}\n",
                entry.patterns.iter().map(|p| format!("\"{}\"", p)).collect::<Vec<_>>().join(" OR ")
            ));
            text.push_str(&format!("- **solution**: {}\n", entry.solution));
            text.push_str(&format!("- **seen**: {}\n\n", entry.occurrences.join(", ")));
        }
        text
    }

    async fn save(&self) -> Result<()> {
        tokio::fs::write(&self.path, self.to_text()).await?;
        Ok(())
    }
}

fn parse_error_book(content: &str) -> Vec<ErrorEntry> {
    let mut entries = Vec::new();
    let mut current: Option<ErrorEntry> = None;

    for line in content.lines() {
        if line.starts_with("## [") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let rest = &line[4..];
            if let Some(bracket_end) = rest.find(']') {
                current = Some(ErrorEntry {
                    id: rest[..bracket_end].to_string(),
                    title: rest[bracket_end + 1..].trim().to_string(),
                    patterns: Vec::new(),
                    solution: String::new(),
                    occurrences: Vec::new(),
                });
            }
        } else if let Some(ref mut entry) = current {
            if let Some(rest) = line.strip_prefix("- **patterns**: ") {
                entry.patterns = rest.split(" OR ").map(|s| s.trim().trim_matches('"').to_string()).collect();
            } else if let Some(rest) = line.strip_prefix("- **solution**: ") {
                entry.solution = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("- **seen**: ") {
                entry.occurrences = rest.split(", ").map(|s| s.to_string()).collect();
            }
        }
    }
    if let Some(entry) = current { entries.push(entry); }
    entries
}

fn extract_error_patterns(error_msg: &str) -> Vec<String> {
    let lower = error_msg.to_lowercase();
    let mut patterns: Vec<String> = ["429", "500", "502", "503", "404", "403", "401"]
        .iter()
        .filter(|code| error_msg.contains(**code))
        .map(|s| s.to_string())
        .collect();
    patterns.extend(
        ["timeout", "connection refused", "permission denied", "not found", "rate limit", "out of memory"]
            .iter()
            .filter(|phrase| lower.contains(**phrase))
            .map(|s| s.to_string()),
    );
    if patterns.is_empty() {
        patterns.push(error_msg.chars().take(50).collect::<String>().to_lowercase());
    }
    patterns
}
