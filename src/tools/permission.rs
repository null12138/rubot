use crate::tools::registry::RiskLevel;
use serde::{Deserialize, Serialize};

/// Determines how aggressively the bot gates tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Auto-approve Low; run YOLO classifier for Medium+ (Claude Code default)
    Yolo,
    /// Auto-approve Low + Medium; run classifier for High+
    AcceptDefaults,
    /// Always run the YOLO classifier before executing
    AlwaysAsk,
    /// Auto-approve everything (scripting / CI)
    AcceptAll,
}

impl PermissionMode {
    pub fn parse(input: &str) -> Option<Self> {
        let normalized = input.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "yolo" | "auto" => Some(Self::Yolo),
            "accept_defaults" | "defaults" | "accept-defaults" => Some(Self::AcceptDefaults),
            "always_ask" | "ask" | "always-ask" => Some(Self::AlwaysAsk),
            "accept_all" | "all" | "accept-all" => Some(Self::AcceptAll),
            _ => None,
        }
    }

    /// Whether a tool at the given risk level can execute without a permission check.
    pub fn auto_approve(&self, risk: RiskLevel) -> bool {
        match self {
            Self::AcceptAll => true,
            Self::Yolo => risk == RiskLevel::Low,
            Self::AcceptDefaults => matches!(risk, RiskLevel::Low | RiskLevel::Medium),
            Self::AlwaysAsk => false,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Yolo => write!(f, "yolo"),
            Self::AcceptDefaults => write!(f, "accept_defaults"),
            Self::AlwaysAsk => write!(f, "always_ask"),
            Self::AcceptAll => write!(f, "accept_all"),
        }
    }
}

/// The YOLO classifier prompt sent to the fast model to evaluate a tool call.
fn yolo_classification_prompt(
    request: Option<&str>,
    tool_name: &str,
    params: &serde_json::Value,
) -> String {
    let request_line = match request {
        Some(r) if !r.trim().is_empty() => format!("User request: {}", r.trim()),
        _ => "User request: (not available)".into(),
    };
    format!(
        r#"You are a permission gate for an AI coding assistant.
Given the user's request and the tool the assistant wants to call,
determine if this tool call is appropriate and aligns with the user's intent.
Answer only YES or NO. Do not explain your reasoning.

{request_line}

Tool: {tool_name}
Arguments: {params}"#,
    )
}

/// Run the YOLO classifier via the fast LLM and return whether the call is approved.
pub(crate) async fn yolo_classify(
    llm: &crate::llm::client::LlmClient,
    request: Option<&str>,
    tool_name: &str,
    params: &serde_json::Value,
) -> bool {
    let prompt = yolo_classification_prompt(request, tool_name, params);
    let msg = crate::llm::types::Message::user(&prompt);
    match llm.chat_fast(&[msg], None, Some(0.1)).await {
        Ok(resp) => {
            let text = resp
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .unwrap_or_default();
            text.trim().to_ascii_uppercase().starts_with("YES")
        }
        Err(e) => {
            tracing::warn!("YOLO classifier failed, defaulting to granted: {:#}", e);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_auto_approves_low() {
        assert!(PermissionMode::Yolo.auto_approve(RiskLevel::Low));
        assert!(!PermissionMode::Yolo.auto_approve(RiskLevel::Medium));
        assert!(!PermissionMode::Yolo.auto_approve(RiskLevel::High));
        assert!(!PermissionMode::Yolo.auto_approve(RiskLevel::Critical));
    }

    #[test]
    fn accept_defaults_auto_approves_low_and_medium() {
        assert!(PermissionMode::AcceptDefaults.auto_approve(RiskLevel::Low));
        assert!(PermissionMode::AcceptDefaults.auto_approve(RiskLevel::Medium));
        assert!(!PermissionMode::AcceptDefaults.auto_approve(RiskLevel::High));
        assert!(!PermissionMode::AcceptDefaults.auto_approve(RiskLevel::Critical));
    }

    #[test]
    fn accept_all_approves_everything() {
        assert!(PermissionMode::AcceptAll.auto_approve(RiskLevel::Critical));
    }

    #[test]
    fn always_ask_approves_nothing() {
        assert!(!PermissionMode::AlwaysAsk.auto_approve(RiskLevel::Low));
    }

    #[test]
    fn parse_accepts_variants() {
        assert_eq!(PermissionMode::parse("yolo"), Some(PermissionMode::Yolo));
        assert_eq!(PermissionMode::parse("auto"), Some(PermissionMode::Yolo));
        assert_eq!(
            PermissionMode::parse("accept-defaults"),
            Some(PermissionMode::AcceptDefaults)
        );
        assert_eq!(
            PermissionMode::parse("always_ask"),
            Some(PermissionMode::AlwaysAsk)
        );
        assert_eq!(
            PermissionMode::parse("all"),
            Some(PermissionMode::AcceptAll)
        );
    }

    #[test]
    fn yolo_template_mentions_tool_and_params() {
        let params = serde_json::json!({"cmd": "rm -rf /"});
        let prompt = yolo_classification_prompt(Some("clean up temp files"), "code_exec", &params);
        assert!(prompt.contains("code_exec"));
        assert!(prompt.contains("rm -rf"));
        assert!(prompt.contains("clean up temp files"));
    }
}
