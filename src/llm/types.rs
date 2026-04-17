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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub struct Choice { pub message: Message, pub finish_reason: Option<String> }

#[derive(Debug, Deserialize)]
pub struct Usage { pub prompt_tokens: u32, pub completion_tokens: u32, pub total_tokens: u32 }

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
    pub fn tokens(&self) -> usize {
        let c = self.content.as_ref().map_or(0, |s| s.len());
        let t = self.tool_calls.as_ref().map_or(0, |v| v.iter().map(|x| x.function.arguments.len() + x.function.name.len()).sum());
        (c + t) / 4
    }
}

impl ToolDefinition {
    pub fn new(name: &str, desc: &str, params: serde_json::Value) -> Self {
        Self { tool_type: "function".into(), function: FunctionDef { name: name.into(), description: desc.into(), parameters: params } }
    }
}

// Streaming types

#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    pub choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    pub delta: StreamDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StreamDelta {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<serde_json::Value>>,
}
