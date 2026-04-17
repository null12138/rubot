use anyhow::{Result, Context};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use chrono::Utc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MemoryLayer { Working, Episodic, Semantic }
impl MemoryLayer {
    pub fn dir(&self) -> &str { match self { Self::Working => "working", Self::Episodic => "episodic", Self::Semantic => "semantic" } }
    pub fn prio(&self) -> u8 { match self { Self::Working => 3, Self::Episodic => 2, Self::Semantic => 1 } }
}

#[derive(Debug, Clone)]
pub struct IndexEntry { pub file: String, pub layer: MemoryLayer, pub summary: String, pub tags: Vec<String> }

pub struct MemoryEntry { pub fm: HashMap<String, String>, pub content: String }

pub struct MemorySearch { base: PathBuf }

impl MemorySearch {
    pub fn new(path: &Path) -> Self { Self { base: path.to_path_buf() } }

    async fn read_raw(&self, rel: &str) -> Result<String> {
        tokio::fs::read_to_string(self.base.join(rel)).await.map_err(Into::into)
    }

    async fn write_raw(&self, rel: &str, data: &str) -> Result<()> {
        let p = self.base.join(rel);
        if let Some(parent) = p.parent() { tokio::fs::create_dir_all(parent).await?; }
        tokio::fs::write(p, data).await.map_err(Into::into)
    }

    pub async fn get_index(&self) -> Result<Vec<IndexEntry>> {
        let txt = self.read_raw("memory_index.md").await.unwrap_or_default();
        Ok(txt.lines().filter_map(|l| {
            let l = l.trim();
            if !l.starts_with("- `") { return None; }
            let rest = &l[3..];
            let end = rest.find('`')?;
            let path = &rest[..end];
            let rem = rest[end+1..].trim().trim_start_matches('—').trim();
            let (sum, tags) = if let Some(s) = rem.rfind('[') {
                (rem[..s].trim().into(), rem[s+1..rem.len()-1].split(", ").map(Into::into).collect())
            } else { (rem.into(), vec![]) };
            let (l_str, file) = path.split_once('/')?;
            let layer = match l_str { "working" => MemoryLayer::Working, "episodic" => MemoryLayer::Episodic, "semantic" => MemoryLayer::Semantic, _ => return None };
            Some(IndexEntry { file: file.into(), layer, summary: sum, tags })
        }).collect())
    }

    pub async fn get_index_text(&self) -> Result<String> {
        self.read_raw("memory_index.md").await
    }

    pub async fn add_memory(&self, layer: MemoryLayer, summary: &str, content: &str, tags: &[&str]) -> Result<String> {
        let id = &Uuid::new_v4().to_string()[..8];
        let file = format!("{}_{}.md", Utc::now().format("%Y%m%d_%H%M%S"), id);
        let rel = format!("{}/{}", layer.dir(), file);
        let mut fm_txt = format!("---\nsummary: {}\ncreated: {}\nlayer: {:?}\n", summary, Utc::now().to_rfc3339(), layer);
        if !tags.is_empty() { fm_txt.push_str(&format!("tags: {}\n", tags.join(", "))); }
        fm_txt.push_str("---\n\n");
        fm_txt.push_str(content);
        self.write_raw(&rel, &fm_txt).await?;
        let mut idx = self.read_raw("memory_index.md").await.unwrap_or_default();
        let t_str = if tags.is_empty() { "".into() } else { format!(" [{}]", tags.join(", ")) };
        idx.push_str(&format!("- `{}/{}` — {}{}\n", layer.dir(), file, summary, t_str));
        self.write_raw("memory_index.md", &idx).await?;
        Ok(rel)
    }

    pub async fn quick_search(&self, query: &str) -> Result<Vec<IndexEntry>> {
        let q = query.to_lowercase();
        let kws: Vec<_> = q.split_whitespace().collect();
        let mut ents = self.get_index().await?;
        ents.retain(|e| {
            let s = e.summary.to_lowercase();
            kws.iter().any(|&kw| s.contains(kw) || e.tags.iter().any(|t| t.to_lowercase().contains(kw)))
        });
        ents.sort_by_key(|e| std::cmp::Reverse(e.layer.prio()));
        Ok(ents)
    }

    pub async fn load(&self, rel: &str) -> Result<MemoryEntry> {
        let raw = self.read_raw(rel).await?;
        if !raw.starts_with("---\n") { return Ok(MemoryEntry { fm: HashMap::new(), content: raw }); }
        let rest = &raw[4..];
        let end = rest.find("\n---").context("No fm end")?;
        let fm_str = &rest[..end];
        let content = rest[end+4..].trim().into();
        let mut fm = HashMap::new();
        for l in fm_str.lines() { if let Some((k, v)) = l.split_once(": ") { fm.insert(k.trim().to_string(), v.trim().to_string()); } }
        Ok(MemoryEntry { fm, content })
    }

    pub async fn clear_working(&self) -> Result<()> {
        let path = self.base.join("working");
        if let Ok(mut entries) = tokio::fs::read_dir(path).await {
            while let Ok(Some(e)) = entries.next_entry().await { let _ = tokio::fs::remove_file(e.path()).await; }
        }
        self.rebuild_index().await
    }

    pub async fn rebuild_index(&self) -> Result<()> {
        let mut idx = "# Memory Index\n\n".to_string();
        for l in &[MemoryLayer::Working, MemoryLayer::Episodic, MemoryLayer::Semantic] {
            let p = self.base.join(l.dir());
            if let Ok(mut rd) = tokio::fs::read_dir(p).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.ends_with(".md") {
                        if let Ok(m) = self.load(&format!("{}/{}", l.dir(), name)).await {
                            let sum = m.fm.get("summary").map(|s| s.as_str()).unwrap_or("");
                            let tags = m.fm.get("tags").map(|t| format!(" [{}]", t)).unwrap_or_default();
                            idx.push_str(&format!("- `{}/{}` — {}{}\n", l.dir(), name, sum, tags));
                        }
                    }
                }
            }
        }
        self.write_raw("memory_index.md", &idx).await
    }
}
