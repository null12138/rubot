use crate::llm::types::{Message, Role};

pub struct ContextCleaner { pub max: usize }
impl ContextCleaner {
    pub fn new(max: usize) -> Self { Self { max } }
    pub fn tokens(ms: &[Message]) -> usize { ms.iter().map(|m| m.tokens()).sum() }
    pub fn needs(&self, ms: &[Message]) -> bool { Self::tokens(ms) > (self.max * 4 / 5) }

    pub fn prune(&self, ms: &[Message], sum: &str) -> Vec<Message> {
        if ms.len() <= 4 { return ms.to_vec(); }
        let mut p = vec![];
        if let Some(m) = ms.first() { if m.role == Role::System { p.push(m.clone()); } }
        if !sum.is_empty() { p.push(Message::system(&format!("[Summary]\n{}", sum))); }
        let start = ms.len().saturating_sub(12);
        for m in &ms[start..] { if m.role != Role::System { p.push(m.clone()); } }
        p
    }

    pub fn prompt(ms: &[Message]) -> String {
        let mut t = "Summarize this context (max 300 words):\n\n".to_string();
        for m in ms {
            if m.role == Role::System { continue; }
            if let Some(c) = &m.content {
                let p: String = c.chars().take(200).collect();
                t.push_str(&format!("{:?}: {}{}\n", m.role, p, if c.len() > 200 { "..." } else { "" }));
            }
        }
        t
    }
}
