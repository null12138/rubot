use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Read/write .md files with YAML-like frontmatter
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub frontmatter: HashMap<String, String>,
    pub content: String,
    pub file_path: PathBuf,
}

impl MemoryEntry {
    pub fn new(file_path: PathBuf, frontmatter: HashMap<String, String>, content: String) -> Self {
        Self {
            frontmatter,
            content,
            file_path,
        }
    }

    pub fn get_field(&self, key: &str) -> Option<&str> {
        self.frontmatter.get(key).map(|s| s.as_str())
    }

    pub fn to_markdown(&self) -> String {
        let mut md = String::from("---\n");
        for (k, v) in &self.frontmatter {
            md.push_str(&format!("{}: {}\n", k, v));
        }
        md.push_str("---\n\n");
        md.push_str(&self.content);
        md
    }
}

pub struct MemoryStore {
    base_path: PathBuf,
}

impl MemoryStore {
    pub fn new(base_path: &Path) -> Self {
        Self {
            base_path: base_path.to_path_buf(),
        }
    }

    /// Read a .md file and parse frontmatter
    pub async fn read(&self, relative_path: &str) -> Result<MemoryEntry> {
        let path = self.base_path.join(relative_path);
        let raw = tokio::fs::read_to_string(&path).await?;
        let (frontmatter, content) = parse_frontmatter(&raw);
        Ok(MemoryEntry::new(path, frontmatter, content))
    }

    /// Write a memory entry to a .md file
    pub async fn write(&self, relative_path: &str, entry: &MemoryEntry) -> Result<()> {
        let path = self.base_path.join(relative_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, entry.to_markdown()).await?;
        Ok(())
    }

    /// Write raw content without frontmatter
    pub async fn write_raw(&self, relative_path: &str, content: &str) -> Result<()> {
        let path = self.base_path.join(relative_path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await?;
        Ok(())
    }

    /// List all .md files in a subdirectory
    pub async fn list(&self, subdir: &str) -> Result<Vec<String>> {
        let path = self.base_path.join(subdir);
        let mut files = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&path).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    files.push(name);
                }
            }
        }
        files.sort();
        Ok(files)
    }

    /// Delete a file
    pub async fn delete(&self, relative_path: &str) -> Result<()> {
        let path = self.base_path.join(relative_path);
        tokio::fs::remove_file(&path).await?;
        Ok(())
    }

    /// Check if a file exists
    pub async fn exists(&self, relative_path: &str) -> bool {
        self.base_path.join(relative_path).exists()
    }

    /// Read raw content
    pub async fn read_raw(&self, relative_path: &str) -> Result<String> {
        let path = self.base_path.join(relative_path);
        Ok(tokio::fs::read_to_string(&path).await?)
    }
}

fn parse_frontmatter(raw: &str) -> (HashMap<String, String>, String) {
    let mut frontmatter = HashMap::new();

    if !raw.starts_with("---\n") {
        return (frontmatter, raw.to_string());
    }

    let rest = &raw[4..]; // Skip "---\n"
    if let Some(end) = rest.find("\n---") {
        let fm_str = &rest[..end];
        let content = rest[end + 4..].trim_start_matches('\n').to_string();

        for line in fm_str.lines() {
            if let Some((key, value)) = line.split_once(": ") {
                frontmatter.insert(key.trim().to_string(), value.trim().to_string());
            }
        }

        (frontmatter, content)
    } else {
        (frontmatter, raw.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter() {
        let raw = "---\ntitle: Test\ntags: a,b,c\n---\n\n# Hello\n\nContent here.";
        let (fm, content) = parse_frontmatter(raw);
        assert_eq!(fm.get("title").unwrap(), "Test");
        assert_eq!(fm.get("tags").unwrap(), "a,b,c");
        assert!(content.starts_with("# Hello"));
    }

    #[test]
    fn test_no_frontmatter() {
        let raw = "# Just content";
        let (fm, content) = parse_frontmatter(raw);
        assert!(fm.is_empty());
        assert_eq!(content, "# Just content");
    }
}
