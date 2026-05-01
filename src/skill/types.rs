use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillType {
    Prompt,
    Workflow,
}

#[derive(Debug, Clone)]
pub struct SkillStep {
    pub tool: String,
    pub params: serde_json::Value,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub skill_type: SkillType,
    pub triggers: Vec<String>,
    pub body: String,
    pub source_file: PathBuf,
}
