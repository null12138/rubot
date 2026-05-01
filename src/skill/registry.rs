use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::sync::RwLock;

use super::parser;
use super::types::Skill;

pub struct SkillRegistry {
    skills: RwLock<HashMap<String, Skill>>,
    skills_dir: Option<PathBuf>,
    last_mtime: RwLock<Option<SystemTime>>,
}

impl SkillRegistry {
    pub fn new(skills_dir: Option<PathBuf>) -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
            skills_dir,
            last_mtime: RwLock::new(None),
        }
    }

    pub async fn load(&self) -> Result<usize> {
        let n = self.reload().await?;
        if let Some(dir) = self.skills_dir.as_ref() {
            *self.last_mtime.write().await = latest_mtime(dir);
        }
        Ok(n)
    }

    pub async fn rescan_if_changed(&self) -> Result<usize> {
        let Some(dir) = self.skills_dir.as_ref() else {
            return Ok(0);
        };
        let current = latest_mtime(dir);
        {
            let last = self.last_mtime.read().await;
            if *last == current {
                return Ok(0);
            }
        }
        *self.last_mtime.write().await = current;
        self.reload().await
    }

    pub async fn reload(&self) -> Result<usize> {
        let Some(dir) = self.skills_dir.as_ref() else {
            return Ok(0);
        };
        if !dir.is_dir() {
            return Ok(0);
        }
        let mut skills = self.skills.write().await;
        skills.clear();
        let mut n = 0usize;
        for ent in std::fs::read_dir(dir)?.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            match parser::parse(&raw, &p) {
                Ok(skill) => {
                    let name = skill.name.clone();
                    skills.insert(name, skill);
                    n += 1;
                }
                Err(e) => {
                    tracing::debug!("skill {}: {:#}", p.display(), e);
                }
            }
        }
        Ok(n)
    }

    pub async fn get_by_name(&self, name: &str) -> Option<Skill> {
        self.rescan_if_changed().await.ok();
        self.skills.read().await.get(name).cloned()
    }

    pub async fn get_by_trigger(&self, trigger: &str) -> Option<Skill> {
        self.rescan_if_changed().await.ok();
        let skills = self.skills.read().await;
        skills
            .values()
            .find(|s| s.triggers.iter().any(|t| t == trigger))
            .cloned()
    }

    pub async fn list(&self) -> Vec<Skill> {
        self.rescan_if_changed().await.ok();
        let skills = self.skills.read().await;
        let mut list: Vec<_> = skills.values().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub async fn delete(&self, name: &str) -> Result<bool> {
        let mut skills = self.skills.write().await;
        if let Some(skill) = skills.remove(name) {
            let _ = std::fs::remove_file(&skill.source_file);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn definitions_text(&self) -> String {
        self.rescan_if_changed().await.ok();
        let skills = self.skills.read().await;
        if skills.is_empty() {
            return String::new();
        }
        let mut lines: Vec<String> = skills
            .values()
            .map(|s| {
                let triggers = if s.triggers.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", s.triggers.join(", "))
                };
                let typ = match s.skill_type {
                    super::types::SkillType::Prompt => "prompt",
                    super::types::SkillType::Workflow => "workflow",
                };
                format!("- {} [{}]{}: {}", s.name, typ, triggers, s.description)
            })
            .collect();
        lines.sort();
        format!("## Available Skills\n{}\nUse `skill_run` to invoke a skill.", lines.join("\n"))
    }
}

fn latest_mtime(dir: &Path) -> Option<SystemTime> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut max: Option<SystemTime> = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    for ent in entries.flatten() {
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Ok(m) = ent.metadata() {
            if let Ok(mt) = m.modified() {
                max = Some(max.map_or(mt, |x| x.max(mt)));
            }
        }
    }
    max
}
