use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role { System, User, Assistant, Tool }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall { pub name: String, pub arguments: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef { pub name: String, pub description: String, pub parameters: serde_json::Value }

#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    pub tool_choice: Option<serde_json::Value>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse { pub choices: Vec<Choice> }

#[derive(Debug, Deserialize)]
pub struct Choice { pub message: Message }

#[derive(Debug, Deserialize)]
pub struct ApiError { pub error: ApiErrorDetail }

#[derive(Debug, Deserialize)]
pub struct ApiErrorDetail { pub message: String }

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self { role, content: Some(content.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn system(c: &str) -> Self { Self::new(Role::System, c) }
    pub fn user(c: &str) -> Self { Self::new(Role::User, c) }
    pub fn assistant(c: &str) -> Self { Self::new(Role::Assistant, c) }
    pub fn tool(id: &str, c: &str) -> Self {
        Self { role: Role::Tool, content: Some(c.into()), tool_calls: None, tool_call_id: Some(id.into()) }
    }
    pub fn tool_result(id: &str, c: &str) -> Self { Self::tool(id, c) }
}

impl ToolDefinition {
    pub fn new(name: &str, desc: &str, params: serde_json::Value) -> Self {
        Self { tool_type: "function".into(), function: FunctionDef { name: name.into(), description: desc.into(), parameters: params } }
    }
}
