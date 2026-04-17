use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

use super::layer::{IndexEntry, MemoryLayer};
use super::store::{MemoryEntry, MemoryStore};

/// Manages the L0 memory index (memory_index.md)
pub struct MemoryIndex {
    store: MemoryStore,
}

impl MemoryIndex {
    pub fn new(memory_path: &Path) -> Self {
        Self {
            store: MemoryStore::new(memory_path),
        }
    }

    /// Get the full index as a string (for LLM context)
    pub async fn get_index_text(&self) -> Result<String> {
        self.store.read_raw("memory_index.md").await
    }

    /// Parse index entries from the index file
    pub async fn get_entries(&self) -> Result<Vec<IndexEntry>> {
        let text = self.store.read_raw("memory_index.md").await?;
        Ok(parse_index_entries(&text))
    }

    /// Add a new memory and update the index
    pub async fn add_memory(
        &self,
        layer: MemoryLayer,
        summary: &str,
        content: &str,
        tags: &[&str],
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string()[..8].to_string();
        let filename = format!("{}_{}.md", Utc::now().format("%Y%m%d_%H%M%S"), id);
        let relative_path = format!("{}/{}", layer.subdir(), filename);

        // Create the memory file
        let mut frontmatter = HashMap::new();
        frontmatter.insert("summary".to_string(), summary.to_string());
        frontmatter.insert("created".to_string(), Utc::now().to_rfc3339());
        frontmatter.insert("layer".to_string(), format!("{:?}", layer));
        if !tags.is_empty() {
            frontmatter.insert("tags".to_string(), tags.join(", "));
        }

        let entry = MemoryEntry::new(
            self.store_path().join(&relative_path),
            frontmatter,
            content.to_string(),
        );
        self.store.write(&relative_path, &entry).await?;

        // Update the index
        let index_entry = IndexEntry {
            filename: filename.clone(),
            layer,
            summary: summary.to_string(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        };
        self.append_to_index(&index_entry).await?;

        Ok(relative_path)
    }

    /// Remove a memory from both file and index
    pub async fn remove_memory(&self, relative_path: &str) -> Result<()> {
        self.store.delete(relative_path).await?;
        self.rebuild_index().await?;
        Ok(())
    }

    /// Load a specific memory file's content
    pub async fn load_memory(&self, relative_path: &str) -> Result<MemoryEntry> {
        self.store.read(relative_path).await
    }

    /// Rebuild the index by scanning all layer directories
    pub async fn rebuild_index(&self) -> Result<()> {
        let mut entries = Vec::new();

        for layer in &[
            MemoryLayer::Working,
            MemoryLayer::Episodic,
            MemoryLayer::Semantic,
        ] {
            let files = self.store.list(layer.subdir()).await?;
            for filename in files {
                let path = format!("{}/{}", layer.subdir(), filename);
                if let Ok(mem_entry) = self.store.read(&path).await {
                    let summary = mem_entry
                        .get_field("summary")
                        .unwrap_or("(no summary)")
                        .to_string();
                    let tags = mem_entry
                        .get_field("tags")
                        .map(|t| t.split(", ").map(|s| s.to_string()).collect())
                        .unwrap_or_default();
                    entries.push(IndexEntry {
                        filename,
                        layer: *layer,
                        summary,
                        tags,
                    });
                }
            }
        }

        let mut content = String::from("# Memory Index\n\n");
        for entry in &entries {
            content.push_str(&entry.to_index_line());
            content.push('\n');
        }

        self.store.write_raw("memory_index.md", &content).await?;
        Ok(())
    }

    async fn append_to_index(&self, entry: &IndexEntry) -> Result<()> {
        let mut current = self.store.read_raw("memory_index.md").await?;
        current.push_str(&entry.to_index_line());
        current.push('\n');
        self.store.write_raw("memory_index.md", &current).await?;
        Ok(())
    }

    fn store_path(&self) -> &Path {
        // Access the base path through store's base_path
        // We need the memory base dir for constructing full paths
        Path::new(".")
    }

    /// Clear all working memory (L1) — called on session end
    pub async fn clear_working(&self) -> Result<()> {
        let files = self.store.list("working").await?;
        for f in files {
            let _ = self.store.delete(&format!("working/{}", f)).await;
        }
        self.rebuild_index().await?;
        Ok(())
    }
}

fn parse_index_entries(text: &str) -> Vec<IndexEntry> {
    let mut entries = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("- `") {
            continue;
        }
        // Format: - `layer/filename` — summary [tags]
        let rest = &line[3..]; // skip "- `"
        if let Some(backtick_end) = rest.find('`') {
            let path = &rest[..backtick_end];
            let remaining = rest[backtick_end + 1..].trim().trim_start_matches("—").trim();

            let (summary, tags) = if let Some(bracket_start) = remaining.rfind('[') {
                let summary = remaining[..bracket_start].trim().to_string();
                let tags_str = &remaining[bracket_start + 1..remaining.len() - 1];
                let tags: Vec<String> = tags_str.split(", ").map(|s| s.to_string()).collect();
                (summary, tags)
            } else {
                (remaining.to_string(), Vec::new())
            };

            if let Some((layer_str, filename)) = path.split_once('/') {
                let layer = match layer_str {
                    "working" => MemoryLayer::Working,
                    "episodic" => MemoryLayer::Episodic,
                    "semantic" => MemoryLayer::Semantic,
                    _ => continue,
                };
                entries.push(IndexEntry {
                    filename: filename.to_string(),
                    layer,
                    summary,
                    tags,
                });
            }
        }
    }

    entries
}
