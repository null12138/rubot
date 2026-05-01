use anyhow::{anyhow, bail, Result};
use std::path::Path;

use super::types::{Skill, SkillType};

/// Parse a skill `.md` file into a `Skill`.
///
/// Expected format:
/// ```markdown
/// ---
/// name: skill_name
/// description: What this skill does
/// type: prompt|workflow
/// triggers: ["/trigger1", "/trigger2"]
/// ---
/// Body content (prompt text or YAML steps)
/// ```
pub fn parse(content: &str, source_file: &Path) -> Result<Skill> {
    let Some(rest) = content.strip_prefix("---\n") else {
        bail!("missing frontmatter (expected --- at start)");
    };
    let Some(end) = rest.find("\n---") else {
        bail!("unterminated frontmatter (expected closing ---)");
    };
    let header = &rest[..end];
    let body = rest[end..]
        .trim_start_matches("\n---")
        .trim_start_matches('\n')
        .trim_end()
        .to_string();

    let mut name = String::new();
    let mut description = String::new();
    let mut skill_type: Option<SkillType> = None;
    let mut triggers: Option<Vec<String>> = None;

    for line in header.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_lowercase();
        let val = v.trim();
        match key.as_str() {
            "name" => name = val.to_string(),
            "description" => description = val.to_string(),
            "type" => {
                skill_type = match val.to_lowercase().as_str() {
                    "prompt" => Some(SkillType::Prompt),
                    "workflow" => Some(SkillType::Workflow),
                    _ => None,
                };
            }
            "triggers" => triggers = Some(parse_trigger_list(val)),
            _ => {}
        }
    }

    if name.is_empty() {
        bail!("missing name");
    }
    if !name
        .chars()
        .next()
        .map(|c| c.is_ascii_lowercase() || c == '_')
        .unwrap_or(false)
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        bail!("name must match [a-z_][a-z0-9_]*");
    }
    let skill_type = skill_type.ok_or_else(|| anyhow!("type must be prompt or workflow"))?;
    if body.is_empty() {
        bail!("body is empty");
    }

    Ok(Skill {
        name,
        description,
        skill_type,
        triggers: triggers.unwrap_or_default(),
        body,
        source_file: source_file.to_path_buf(),
    })
}

/// Parse a bracketed, comma-separated trigger list like `["/review", "/cr"]`.
fn parse_trigger_list(raw: &str) -> Vec<String> {
    let inner = raw.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_prompt_skill() {
        let md = r#"---
name: code_review
description: Review code for bugs
type: prompt
triggers: ["/review", "/cr"]
---
You are a code reviewer. Analyze for:
1. Bug risks
2. Style issues
"#;
        let skill = parse(md, Path::new("skills/code_review.md")).unwrap();
        assert_eq!(skill.name, "code_review");
        assert_eq!(skill.description, "Review code for bugs");
        assert_eq!(skill.skill_type, SkillType::Prompt);
        assert_eq!(skill.triggers, vec!["/review", "/cr"]);
        assert!(skill.body.contains("code reviewer"));
    }

    #[test]
    fn parse_workflow_skill() {
        let md = r#"---
name: fetch_and_summarize
description: Fetch and summarize a URL
type: workflow
triggers: ["/summarize"]
---
steps:
  - tool: web_fetch
    params: {"url": "{{input}}"}
    description: Fetch URL
"#;
        let skill = parse(md, Path::new("skills/fetch_and_summarize.md")).unwrap();
        assert_eq!(skill.name, "fetch_and_summarize");
        assert_eq!(skill.skill_type, SkillType::Workflow);
        assert_eq!(skill.triggers, vec!["/summarize"]);
    }

    #[test]
    fn parse_skill_without_triggers() {
        let md = r#"---
name: hello
description: Says hello
type: prompt
---
Hello world
"#;
        let skill = parse(md, Path::new("skills/hello.md")).unwrap();
        assert!(skill.triggers.is_empty());
    }

    #[test]
    fn reject_invalid_name() {
        let md = r#"---
name: My-Skill
description: Bad name
type: prompt
---
body
"#;
        assert!(parse(md, Path::new("skills/bad.md")).is_err());
    }

    #[test]
    fn reject_missing_type() {
        let md = r#"---
name: test
description: No type
---
body
"#;
        assert!(parse(md, Path::new("skills/bad.md")).is_err());
    }

    #[test]
    fn reject_empty_body() {
        let md = r#"---
name: test
description: Empty body
type: prompt
---
"#;
        assert!(parse(md, Path::new("skills/bad.md")).is_err());
    }

    #[test]
    fn parse_triggers_variants() {
        assert_eq!(
            parse_trigger_list(r#"["/a", "/b"]"#),
            vec!["/a", "/b"]
        );
        assert_eq!(
            parse_trigger_list(r#"['/a', '/b']"#),
            vec!["/a", "/b"]
        );
        assert_eq!(parse_trigger_list(r#""/single""#), vec!["/single"]);
        assert_eq!(parse_trigger_list(""), Vec::<String>::new());
    }
}
