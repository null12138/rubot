use anyhow::Result;
use std::path::Path;

use super::index::MemoryIndex;
use super::layer::{IndexEntry, MemoryLayer};

/// Hierarchical memory search: index (L0) → layer files → content
pub struct MemorySearch {
    index: MemoryIndex,
}

impl MemorySearch {
    pub fn new(memory_path: &Path) -> Self {
        Self {
            index: MemoryIndex::new(memory_path),
        }
    }

    /// Quick search: scan L0 index for keyword matches in summaries/tags
    /// Returns matching index entries without loading file content
    pub async fn quick_search(&self, query: &str) -> Result<Vec<IndexEntry>> {
        let entries = self.index.get_entries().await?;
        let query_lower = query.to_lowercase();
        let keywords: Vec<&str> = query_lower.split_whitespace().collect();

        let mut matches: Vec<(IndexEntry, u8)> = entries
            .into_iter()
            .filter_map(|entry| {
                let summary_lower = entry.summary.to_lowercase();
                let tags_lower: Vec<String> =
                    entry.tags.iter().map(|t| t.to_lowercase()).collect();

                let matched = keywords.iter().any(|kw| {
                    summary_lower.contains(kw)
                        || tags_lower.iter().any(|t| t.contains(kw))
                        || entry.filename.to_lowercase().contains(kw)
                });

                if matched {
                    Some((entry.clone(), entry.layer.priority()))
                } else {
                    None
                }
            })
            .collect();

        // Sort by layer priority (working > episodic > semantic)
        matches.sort_by(|a, b| b.1.cmp(&a.1));

        Ok(matches.into_iter().map(|(entry, _)| entry).collect())
    }

    /// Deep search: load matching files and search within content
    pub async fn deep_search(&self, query: &str, max_results: usize) -> Result<Vec<SearchHit>> {
        let index_matches = self.quick_search(query).await?;
        let query_lower = query.to_lowercase();
        let mut hits = Vec::new();

        for entry in index_matches.iter().take(max_results * 2) {
            let path = format!("{}/{}", entry.layer.subdir(), entry.filename);
            if let Ok(mem_entry) = self.index.load_memory(&path).await {
                let content_lower = mem_entry.content.to_lowercase();
                let relevance = compute_relevance(&query_lower, &content_lower, &entry.summary);

                hits.push(SearchHit {
                    path,
                    summary: entry.summary.clone(),
                    layer: entry.layer,
                    relevance,
                    excerpt: extract_excerpt(&mem_entry.content, &query_lower, 200),
                });
            }
        }

        hits.sort_by(|a, b| b.relevance.partial_cmp(&a.relevance).unwrap());
        hits.truncate(max_results);

        Ok(hits)
    }

    /// Get the full index text for LLM context injection
    pub async fn get_index_for_context(&self) -> Result<String> {
        self.index.get_index_text().await
    }

    /// Access the underlying index for memory operations
    pub fn index(&self) -> &MemoryIndex {
        &self.index
    }
}

#[derive(Debug)]
pub struct SearchHit {
    pub path: String,
    pub summary: String,
    pub layer: MemoryLayer,
    pub relevance: f32,
    pub excerpt: String,
}

fn compute_relevance(query: &str, content: &str, summary: &str) -> f32 {
    let keywords: Vec<&str> = query.split_whitespace().collect();
    let total = keywords.len() as f32;
    if total == 0.0 {
        return 0.0;
    }

    let mut score: f32 = 0.0;
    for kw in &keywords {
        if summary.to_lowercase().contains(kw) {
            score += 2.0; // summary matches worth more
        }
        if content.contains(kw) {
            score += 1.0;
        }
    }

    score / (total * 3.0) // normalize to 0..1
}

fn extract_excerpt(content: &str, query: &str, max_len: usize) -> String {
    let content_lower = content.to_lowercase();
    let first_keyword = query.split_whitespace().next().unwrap_or("");

    if let Some(pos) = content_lower.find(first_keyword) {
        // Find char boundary near the byte position
        let start_byte = pos.saturating_sub(50);
        let end_byte = (pos + max_len).min(content.len());
        
        let start_idx = content.char_indices().map(|(i, _)| i).filter(|&i| i <= start_byte).last().unwrap_or(0);
        let end_idx = content.char_indices().map(|(i, _)| i).filter(|&i| i >= end_byte).next().unwrap_or(content.len());
        
        let excerpt = &content[start_idx..end_idx];
        if start_idx > 0 {
            format!("...{}", excerpt)
        } else {
            excerpt.to_string()
        }
    } else if content.chars().count() > max_len {
        let truncated: String = content.chars().take(max_len).collect();
        format!("{}...", truncated)
    } else {
        content.to_string()
    }
}
