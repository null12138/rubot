use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemoryLayer {
    Working,
    Episodic,
    Semantic,
}

impl MemoryLayer {
    pub fn dir(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Episodic => "episodic",
            Self::Semantic => "semantic",
        }
    }
    pub fn prio(&self) -> u8 {
        match self {
            Self::Semantic => 3,
            Self::Episodic => 2,
            Self::Working => 1,
        }
    }
    pub fn all() -> [MemoryLayer; 3] {
        [Self::Semantic, Self::Episodic, Self::Working]
    }
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "working" => Some(Self::Working),
            "episodic" => Some(Self::Episodic),
            "semantic" => Some(Self::Semantic),
            _ => None,
        }
    }
    fn pretty(&self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Episodic => "Episodic",
            Self::Semantic => "Semantic",
        }
    }
}

/// Hours until due, indexed by strength 0..=5. Ebbinghaus-style spacing.
const DUE_HOURS: [i64; 6] = [1, 24, 72, 168, 720, 2160];
const MAX_STRENGTH: u8 = 5;

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub file: String,
    pub layer: MemoryLayer,
    pub summary: String,
    pub tags: Vec<String>,
    pub strength: u8,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct DecayReport {
    pub promoted: usize,
    pub evicted: usize,
}

#[derive(Default, Clone)]
pub(crate) struct Frontmatter {
    pub(crate) layer: Option<MemoryLayer>,
    pub(crate) summary: String,
    pub(crate) tags: Vec<String>,
    pub(crate) created: String,
    pub(crate) strength: u8,
    pub(crate) reviews: u32,
    pub(crate) last_reviewed: String,
}

impl Frontmatter {
    fn effective_last_reviewed(&self) -> &str {
        if self.last_reviewed.is_empty() {
            &self.created
        } else {
            &self.last_reviewed
        }
    }
}

pub struct MemorySearch {
    root: PathBuf,
}

impl MemorySearch {
    pub fn new(workspace: &Path) -> Self {
        Self {
            root: workspace.join("memory"),
        }
    }

    pub async fn add_memory(
        &self,
        layer: MemoryLayer,
        summary: &str,
        content: &str,
        tags: &[&str],
    ) -> Result<String> {
        let summary = summary.lines().next().unwrap_or("").trim().to_string();
        let tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
        let now = Utc::now().to_rfc3339();

        let (rel, path, fm) =
            if let Some((path, mut fm)) = self.find_existing_summary_match(layer, &summary)? {
                fm.layer = Some(layer);
                fm.summary = summary.clone();
                fm.tags = tags.clone();
                fm.last_reviewed = now.clone();
                fm.reviews = fm.reviews.saturating_add(1);
                fm.strength = fm.strength.saturating_add(1).min(MAX_STRENGTH);
                if fm.created.is_empty() {
                    fm.created = now.clone();
                }
                let rel = format!(
                    "{}/{}",
                    layer.dir(),
                    path.file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or_default()
                );
                (rel, path, fm)
            } else {
                let fname = new_filename();
                let rel = format!("{}/{}", layer.dir(), fname);
                let path = self.root.join(&rel);
                let fm = Frontmatter {
                    layer: Some(layer),
                    summary: summary.clone(),
                    tags: tags.clone(),
                    created: now.clone(),
                    strength: 0,
                    reviews: 0,
                    last_reviewed: now,
                };
                (rel, path, fm)
            };

        write_entry(&path, &fm, content)?;
        self.rebuild_index()?;
        Ok(rel)
    }

    pub async fn get_entry(&self, file: &str) -> Result<Option<String>> {
        let Some(path) = self.resolve_id(file)? else {
            return Ok(None);
        };
        let raw = fs::read_to_string(&path)?;
        let (mut fm, body) = parse_frontmatter(&raw);
        let layer = fm.layer.unwrap_or(MemoryLayer::Working);
        let fname = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("")
            .to_string();

        // Auto-touch: reading = rehearsal.
        fm.strength = fm.strength.saturating_add(1).min(MAX_STRENGTH);
        fm.reviews = fm.reviews.saturating_add(1);
        fm.last_reviewed = Utc::now().to_rfc3339();
        fm.layer = Some(layer);
        let _ = write_entry(&path, &fm, &body);
        let _ = self.rebuild_index();

        let tags = if fm.tags.is_empty() {
            "(none)".into()
        } else {
            fm.tags.join(", ")
        };
        Ok(Some(format!(
            "# {}/{}\n\n**Summary:** {}\n**Layer:** {}\n**Strength:** {}/{} (reviews: {})\n**Tags:** {}\n**Created:** {}\n**Last reviewed:** {}\n\n---\n\n{}",
            layer.dir(), fname, fm.summary, layer.pretty(),
            fm.strength, MAX_STRENGTH, fm.reviews,
            tags, fm.created, fm.last_reviewed, body.trim_end(),
        )))
    }

    pub async fn touch(&self, file: &str) -> Result<bool> {
        let Some(path) = self.resolve_id(file)? else {
            return Ok(false);
        };
        let raw = fs::read_to_string(&path)?;
        let (mut fm, body) = parse_frontmatter(&raw);
        if fm.layer.is_none() {
            fm.layer = infer_layer_from_path(&path);
        }
        fm.strength = fm.strength.saturating_add(1).min(MAX_STRENGTH);
        fm.reviews = fm.reviews.saturating_add(1);
        fm.last_reviewed = Utc::now().to_rfc3339();
        write_entry(&path, &fm, &body)?;
        self.rebuild_index()?;
        Ok(true)
    }

    pub async fn delete_entry(&self, file: &str) -> Result<bool> {
        let Some(path) = self.resolve_id(file)? else {
            return Ok(false);
        };
        fs::remove_file(&path)?;
        self.rebuild_index()?;
        Ok(true)
    }

    pub async fn clear_all(&self) -> Result<usize> {
        let mut n = 0usize;
        for layer in MemoryLayer::all() {
            let dir = self.root.join(layer.dir());
            let Ok(read) = fs::read_dir(&dir) else {
                continue;
            };
            for ent in read.flatten() {
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) == Some("md") {
                    fs::remove_file(&p)?;
                    n += 1;
                }
            }
        }
        self.rebuild_index()?;
        Ok(n)
    }

    pub async fn get_index_text(&self) -> Result<String> {
        let entries = self.collect_sorted()?;
        let mut s = String::from("# Memory Index\n\n");
        if entries.is_empty() {
            s.push_str("(empty)\n");
            return Ok(s);
        }
        for e in &entries {
            let t = if e.tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.tags.join(", "))
            };
            s.push_str(&format!(
                "- `{}` — {} (s{}){}\n",
                e.file, e.summary, e.strength, t
            ));
        }
        Ok(s)
    }

    pub async fn quick_search(&self, query: &str) -> Result<Vec<IndexEntry>> {
        let keywords: Vec<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        if keywords.is_empty() {
            return Ok(vec![]);
        }
        let mut scored: Vec<(i32, IndexEntry)> = Vec::new();
        for (path, layer, fm) in self.scan_all()? {
            let raw = fs::read_to_string(&path).unwrap_or_default();
            let (_, body) = parse_frontmatter(&raw);
            let score = score_memory_match(&keywords, &fm, &body);
            if score <= 0 {
                continue;
            }
            scored.push((score, to_index_entry(&path, layer, &fm)));
        }
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.layer.prio().cmp(&a.1.layer.prio()))
                .then_with(|| b.1.strength.cmp(&a.1.strength))
                .then_with(|| b.1.file.cmp(&a.1.file))
        });
        Ok(scored.into_iter().map(|(_, entry)| entry).collect())
    }

    /// Entries where now - last_reviewed > DUE_HOURS[strength]h, sorted most-overdue first.
    pub async fn due(&self) -> Result<Vec<IndexEntry>> {
        let now = Utc::now();
        let mut with_age: Vec<(i64, IndexEntry)> = self
            .scan_all()?
            .into_iter()
            .filter_map(|(p, l, fm)| {
                let last = parse_dt(fm.effective_last_reviewed())?;
                let age = (now - last).num_hours();
                let due = DUE_HOURS[fm.strength.min(MAX_STRENGTH) as usize];
                if age <= due {
                    return None;
                }
                Some((age - due, to_index_entry(&p, l, &fm)))
            })
            .collect();
        with_age.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(with_age.into_iter().map(|(_, e)| e).collect())
    }

    /// Expose all entries for sleep-mode consolidation.
    pub fn scan_all_for_consolidation(&self) -> Vec<(PathBuf, MemoryLayer, Frontmatter)> {
        self.scan_all().unwrap_or_default()
    }

    /// Sweep: promote strong entries up a layer; evict Working entries past 2x their window.
    pub async fn decay(&self) -> Result<DecayReport> {
        let mut report = DecayReport::default();
        let now = Utc::now();
        let entries = self.scan_all()?;

        for (path, layer, fm) in entries {
            let strength = fm.strength.min(MAX_STRENGTH);
            let due = DUE_HOURS[strength as usize];
            let last = parse_dt(fm.effective_last_reviewed()).unwrap_or(now);
            let age = (now - last).num_hours();

            let promote_to = match layer {
                MemoryLayer::Working if strength >= 2 => Some(MemoryLayer::Episodic),
                MemoryLayer::Episodic if strength >= 4 => Some(MemoryLayer::Semantic),
                _ => None,
            };

            if let Some(nl) = promote_to {
                let fname = path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or("")
                    .to_string();
                let new_path = self.root.join(nl.dir()).join(&fname);
                let raw = fs::read_to_string(&path)?;
                let (_, body) = parse_frontmatter(&raw);
                let mut new_fm = fm.clone();
                new_fm.layer = Some(nl);
                write_entry(&new_path, &new_fm, &body)?;
                let _ = fs::remove_file(&path);
                report.promoted += 1;
                continue;
            }

            if layer == MemoryLayer::Working && age > 2 * due {
                fs::remove_file(&path)?;
                report.evicted += 1;
            }
        }

        self.rebuild_index()?;
        Ok(report)
    }

    fn scan_all(&self) -> Result<Vec<(PathBuf, MemoryLayer, Frontmatter)>> {
        let mut out = Vec::new();
        for layer in MemoryLayer::all() {
            let dir = self.root.join(layer.dir());
            let Ok(read) = fs::read_dir(&dir) else {
                continue;
            };
            for ent in read.flatten() {
                let path = ent.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Ok(raw) = fs::read_to_string(&path) else {
                    continue;
                };
                let (fm, _) = parse_frontmatter(&raw);
                out.push((path, layer, fm));
            }
        }
        Ok(out)
    }

    fn collect_sorted(&self) -> Result<Vec<IndexEntry>> {
        let mut entries: Vec<IndexEntry> = self
            .scan_all()?
            .into_iter()
            .map(|(p, l, fm)| to_index_entry(&p, l, &fm))
            .collect();
        sort_entries(&mut entries);
        Ok(entries)
    }

    fn rebuild_index(&self) -> Result<()> {
        let entries = self.collect_sorted()?;
        let mut s =
            String::from("# Memory Index\n\n<!-- auto-generated; do not edit by hand -->\n\n");
        if entries.is_empty() {
            s.push_str("(empty)\n");
        } else {
            for layer in MemoryLayer::all() {
                let in_layer: Vec<_> = entries.iter().filter(|e| e.layer == layer).collect();
                if in_layer.is_empty() {
                    continue;
                }
                s.push_str(&format!("## {}\n\n", layer.pretty()));
                for e in in_layer {
                    let t = if e.tags.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", e.tags.join(", "))
                    };
                    s.push_str(&format!(
                        "- `{}` — {} (s{}){}\n",
                        e.file, e.summary, e.strength, t
                    ));
                }
                s.push('\n');
            }
        }
        atomic_write(&self.root.join("memory_index.md"), &s)?;
        Ok(())
    }

    fn resolve_id(&self, input: &str) -> Result<Option<PathBuf>> {
        let stripped = input.trim().strip_suffix(".md").unwrap_or(input.trim());
        if stripped.is_empty() {
            return Ok(None);
        }

        if let Some((l, fname)) = stripped.split_once('/') {
            let p = self.root.join(l).join(format!("{}.md", fname));
            return Ok(p.is_file().then_some(p));
        }

        let mut matches: Vec<PathBuf> = Vec::new();
        for layer in MemoryLayer::all() {
            let dir = self.root.join(layer.dir());
            let Ok(read) = fs::read_dir(&dir) else {
                continue;
            };
            for ent in read.flatten() {
                let fname = ent.file_name().to_string_lossy().to_string();
                if !fname.ends_with(".md") {
                    continue;
                }
                let stem = &fname[..fname.len() - 3];
                if stem == stripped {
                    return Ok(Some(ent.path()));
                }
                if stripped.len() >= 4 {
                    if let Some(hex) = stem.rsplit('_').next() {
                        if hex.starts_with(stripped) {
                            matches.push(ent.path());
                        }
                    }
                }
            }
        }

        if matches.len() > 1 {
            let names: Vec<_> = matches
                .iter()
                .filter_map(|p| p.file_name().and_then(|f| f.to_str()).map(String::from))
                .collect();
            bail!("ambiguous prefix, matches: {}", names.join(", "));
        }
        Ok(matches.into_iter().next())
    }

    fn find_existing_summary_match(
        &self,
        layer: MemoryLayer,
        summary: &str,
    ) -> Result<Option<(PathBuf, Frontmatter)>> {
        let wanted = normalize_summary(summary);
        if wanted.is_empty() {
            return Ok(None);
        }
        let dir = self.root.join(layer.dir());
        let Ok(read) = fs::read_dir(&dir) else {
            return Ok(None);
        };
        for ent in read.flatten() {
            let path = ent.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let (fm, _) = parse_frontmatter(&raw);
            if normalize_summary(&fm.summary) == wanted {
                return Ok(Some((path, fm)));
            }
        }
        Ok(None)
    }
}

fn to_index_entry(path: &Path, layer: MemoryLayer, fm: &Frontmatter) -> IndexEntry {
    let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    IndexEntry {
        file: format!("{}/{}", layer.dir(), fname),
        layer,
        summary: fm.summary.clone(),
        tags: fm.tags.clone(),
        strength: fm.strength.min(MAX_STRENGTH),
    }
}

fn sort_entries(v: &mut [IndexEntry]) {
    v.sort_by(|a, b| {
        b.layer
            .prio()
            .cmp(&a.layer.prio())
            .then_with(|| b.strength.cmp(&a.strength))
            .then_with(|| b.file.cmp(&a.file))
    });
}

fn score_memory_match(keywords: &[String], fm: &Frontmatter, body: &str) -> i32 {
    let summary = fm.summary.to_lowercase();
    let tags = fm
        .tags
        .iter()
        .map(|tag| tag.to_lowercase())
        .collect::<Vec<_>>();
    let body = body.to_lowercase();
    let mut score = 0i32;

    for keyword in keywords {
        if summary.contains(keyword) {
            score += 6;
        }
        if tags.iter().any(|tag| tag.contains(keyword)) {
            score += 4;
        }
        if body.contains(keyword) {
            score += 2;
        }
    }

    if !keywords.is_empty() && summary.contains(&keywords.join(" ")) {
        score += 4;
    }

    score + i32::from(fm.strength.min(MAX_STRENGTH))
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn write_entry(path: &Path, fm: &Frontmatter, body: &str) -> Result<()> {
    let header = render_frontmatter(fm);
    let full = format!("{}\n{}\n", header, body.trim_end());
    atomic_write(path, &full)
}

fn render_frontmatter(fm: &Frontmatter) -> String {
    let layer = fm.layer.unwrap_or(MemoryLayer::Working);
    let summary = fm.summary.lines().next().unwrap_or("").trim();
    let tags = format!("[{}]", fm.tags.join(", "));
    let last = fm.effective_last_reviewed();
    format!(
        "---\nlayer: {}\nsummary: {}\ntags: {}\ncreated: {}\nstrength: {}\nreviews: {}\nlast_reviewed: {}\n---\n",
        layer.dir(), summary, tags, fm.created,
        fm.strength.min(MAX_STRENGTH), fm.reviews, last,
    )
}

fn parse_frontmatter(content: &str) -> (Frontmatter, String) {
    let mut fm = Frontmatter::default();
    let Some(rest) = content.strip_prefix("---\n") else {
        return (fm, content.to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (fm, content.to_string());
    };
    let header = &rest[..end];
    let body = rest[end..]
        .trim_start_matches("\n---")
        .trim_start_matches('\n')
        .to_string();

    for line in header.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_lowercase();
        let val = v.trim();
        match key.as_str() {
            "layer" => fm.layer = MemoryLayer::parse(val),
            "summary" => fm.summary = val.to_string(),
            "created" => fm.created = val.to_string(),
            "strength" => fm.strength = val.parse().unwrap_or(0),
            "reviews" => fm.reviews = val.parse().unwrap_or(0),
            "last_reviewed" => fm.last_reviewed = val.to_string(),
            "tags" => {
                let inner = val.trim_start_matches('[').trim_end_matches(']');
                fm.tags = inner
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    (fm, body)
}

fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn normalize_summary(summary: &str) -> String {
    summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn infer_layer_from_path(p: &Path) -> Option<MemoryLayer> {
    let parent = p.parent()?.file_name()?.to_str()?;
    MemoryLayer::parse(parent)
}

fn new_filename() -> String {
    let ts = Utc::now().format("%Y%m%d_%H%M%S");
    format!("{}_{}.md", ts, random_hex8())
}

fn random_hex8() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:08x}", nanos)
}

#[cfg(test)]
mod tests {
    use super::{normalize_summary, MemoryLayer, MemorySearch};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        dir.push(format!(
            "rubot-memory-test-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(dir.join("memory/working")).unwrap();
        std::fs::create_dir_all(dir.join("memory/episodic")).unwrap();
        std::fs::create_dir_all(dir.join("memory/semantic")).unwrap();
        dir
    }

    #[tokio::test]
    async fn add_memory_deduplicates_same_summary_in_layer() {
        let workspace = temp_workspace();
        let memory = MemorySearch::new(&workspace);
        let first = memory
            .add_memory(MemoryLayer::Working, "Same Task", "first body", &[])
            .await
            .unwrap();
        let second = memory
            .add_memory(MemoryLayer::Working, "  same   task  ", "second body", &[])
            .await
            .unwrap();

        assert_eq!(first, second);
        let files = std::fs::read_dir(workspace.join("memory/working"))
            .unwrap()
            .flatten()
            .count();
        assert_eq!(files, 1);
        let body = memory.get_entry(&first).await.unwrap().unwrap();
        assert!(body.contains("second body"));
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn quick_search_matches_body_content() {
        let workspace = temp_workspace();
        let memory = MemorySearch::new(&workspace);
        memory
            .add_memory(
                MemoryLayer::Semantic,
                "Crawler notes",
                "The SSRN crawler is blocked by robots and 403 responses.",
                &["crawler"],
            )
            .await
            .unwrap();

        let hits = memory.quick_search("robots 403").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].summary.contains("Crawler notes"));
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn normalize_summary_squashes_whitespace() {
        assert_eq!(normalize_summary("  Hello   World "), "hello world");
    }
}
