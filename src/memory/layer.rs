use serde::{Deserialize, Serialize};

/// Memory layers: L1 working, L2 episodic, L3 semantic
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MemoryLayer {
    Working,  // L1: current session, ephemeral
    Episodic, // L2: past interactions, medium-term
    Semantic, // L3: distilled knowledge, permanent
}

impl MemoryLayer {
    pub fn subdir(&self) -> &str {
        match self {
            MemoryLayer::Working => "working",
            MemoryLayer::Episodic => "episodic",
            MemoryLayer::Semantic => "semantic",
        }
    }

    pub fn priority(&self) -> u8 {
        match self {
            MemoryLayer::Working => 3,  // highest priority, search first
            MemoryLayer::Episodic => 2, // medium
            MemoryLayer::Semantic => 1, // lowest priority, searched last
        }
    }
}

/// A single entry in the L0 memory index
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub filename: String,
    pub layer: MemoryLayer,
    pub summary: String,
    pub tags: Vec<String>,
}

impl IndexEntry {
    pub fn to_index_line(&self) -> String {
        let tag_str = if self.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", self.tags.join(", "))
        };
        format!(
            "- `{}/{}` — {}{}",
            self.layer.subdir(),
            self.filename,
            self.summary,
            tag_str
        )
    }
}
