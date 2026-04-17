use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};

/// Error notebook: stores known error patterns and their solutions
pub struct ErrorBook {
    path: PathBuf,
    pub(crate) entries: Vec<ErrorEntry>,
}

#[derive(Debug, Clone)]
pub struct ErrorEntry {
    pub id: String,
    pub title: String,
    pub patterns: Vec<String>, // keywords/regex to match
    pub solution: String,
    pub occurrences: Vec<String>, // dates seen
}

impl ErrorBook {
    pub async fn load(workspace: &Path) -> Result<Self> {
        let path = workspace.join("errors/error_book.md");
        let content = tokio::fs::read_to_string(&path)
            .await
            .unwrap_or_else(|_| "# Error Book\n\n".to_string());

        let entries = parse_error_book(&content);

        Ok(Self { path, entries })
    }

    /// Find a fix for an error message by matching known patterns
    pub async fn find_fix(&self, error_msg: &str) -> Option<String> {
        let error_lower = error_msg.to_lowercase();

        for entry in &self.entries {
            let matched = entry
                .patterns
                .iter()
                .any(|p| error_lower.contains(&p.to_lowercase()));
            if matched {
                return Some(entry.solution.clone());
            }
        }
        None
    }

    /// Record a new error occurrence
    pub async fn record_error(&mut self, tool: &str, error_msg: &str) -> Result<()> {
        let error_lower = error_msg.to_lowercase();
        let now = Utc::now().format("%Y-%m-%d").to_string();

        // Check if this matches an existing entry
        for entry in &mut self.entries {
            let matched = entry
                .patterns
                .iter()
                .any(|p| error_lower.contains(&p.to_lowercase()));
            if matched {
                entry.occurrences.push(now.clone());
                self.save().await?;
                return Ok(());
            }
        }

        // New error — create a placeholder entry
        let id = format!("err_{}", self.entries.len() + 1);
        let title = format!("{} error", tool);
        // Extract key phrases as patterns
        let patterns = extract_error_patterns(error_msg);

        self.entries.push(ErrorEntry {
            id: id.clone(),
            title,
            patterns,
            solution: "(pending analysis — needs LLM classification)".to_string(),
            occurrences: vec![now],
        });

        self.save().await?;
        Ok(())
    }

    /// Update the solution for an error entry
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

    /// Get all entries as formatted text
    pub fn to_text(&self) -> String {
        let mut text = String::from("# Error Book\n\n");
        for entry in &self.entries {
            text.push_str(&format!("## [{}] {}\n", entry.id, entry.title));
            text.push_str(&format!(
                "- **patterns**: {}\n",
                entry
                    .patterns
                    .iter()
                    .map(|p| format!("\"{}\"", p))
                    .collect::<Vec<_>>()
                    .join(" OR ")
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
            // Parse "## [err_1] Title"
            let rest = &line[4..]; // skip "## ["
            if let Some(bracket_end) = rest.find(']') {
                let id = rest[..bracket_end].to_string();
                let title = rest[bracket_end + 1..].trim().to_string();
                current = Some(ErrorEntry {
                    id,
                    title,
                    patterns: Vec::new(),
                    solution: String::new(),
                    occurrences: Vec::new(),
                });
            }
        } else if let Some(ref mut entry) = current {
            if let Some(rest) = line.strip_prefix("- **patterns**: ") {
                entry.patterns = rest
                    .split(" OR ")
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .collect();
            } else if let Some(rest) = line.strip_prefix("- **solution**: ") {
                entry.solution = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("- **seen**: ") {
                entry.occurrences = rest.split(", ").map(|s| s.to_string()).collect();
            }
        }
    }

    if let Some(entry) = current {
        entries.push(entry);
    }

    entries
}

fn extract_error_patterns(error_msg: &str) -> Vec<String> {
    let mut patterns = Vec::new();
    // Extract HTTP status codes
    for code in &["429", "500", "502", "503", "404", "403", "401"] {
        if error_msg.contains(code) {
            patterns.push(code.to_string());
        }
    }
    // Extract key phrases
    let key_phrases = [
        "timeout",
        "connection refused",
        "permission denied",
        "not found",
        "rate limit",
        "out of memory",
    ];
    for phrase in &key_phrases {
        if error_msg.to_lowercase().contains(phrase) {
            patterns.push(phrase.to_string());
        }
    }
    // If no patterns found, use first 50 chars as a rough pattern
    if patterns.is_empty() {
        let truncated: String = if error_msg.chars().count() > 50 {
            error_msg.chars().take(50).collect()
        } else {
            error_msg.to_string()
        };
        patterns.push(truncated.to_lowercase());
    }
    patterns
}
