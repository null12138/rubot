use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

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
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ChatResponse {
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub message: Message,
}

#[derive(Debug, Deserialize)]
pub struct ApiError {
    pub error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
pub struct ApiErrorDetail {
    pub message: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn system(c: &str) -> Self {
        Self::new(Role::System, c)
    }
    pub fn user(c: &str) -> Self {
        Self::new(Role::User, c)
    }
    pub fn tool(id: &str, c: &str) -> Self {
        Self {
            role: Role::Tool,
            content: Some(c.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
        }
    }
    pub fn tool_result(id: &str, c: &str) -> Self {
        Self::tool(id, c)
    }
}

impl ToolDefinition {
    pub fn new(name: &str, desc: &str, params: serde_json::Value) -> Self {
        Self {
            tool_type: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: desc.into(),
                parameters: params,
            },
        }
    }

    pub fn compact_for_llm(mut self) -> Self {
        self.function.description = compact_text(&self.function.description, 160);
        compact_schema_value(&mut self.function.parameters);
        self
    }
}

fn compact_schema_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(description) = map.get_mut("description") {
                if let Some(text) = description.as_str() {
                    *description = serde_json::Value::String(compact_text(text, 96));
                }
            }
            map.remove("title");
            for child in map.values_mut() {
                compact_schema_value(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                compact_schema_value(item);
            }
        }
        _ => {}
    }
}

fn compact_text(input: &str, max_chars: usize) -> String {
    let squashed = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = squashed.chars().count();
    if char_count <= max_chars {
        return squashed;
    }

    let truncated: String = squashed.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated)
}

#[cfg(test)]
mod tests {
    use super::ToolDefinition;

    #[test]
    fn compact_for_llm_keeps_schema_shape() {
        let def = ToolDefinition::new(
            "demo",
            "A very long description that should be squashed and potentially truncated for the language model while keeping the actual schema intact.",
            serde_json::json!({
                "type": "object",
                "title": "Demo Tool",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "A very long field description that explains the path parameter in more detail than the model really needs for successful tool calling."
                    }
                },
                "required": ["path"]
            }),
        )
        .compact_for_llm();

        assert_eq!(def.function.name, "demo");
        assert_eq!(def.function.parameters["type"], "object");
        assert_eq!(def.function.parameters["required"][0], "path");
        assert!(def.function.parameters.get("title").is_none());
        assert!(def.function.description.chars().count() <= 160);
        assert!(
            def.function.parameters["properties"]["path"]["description"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 96
        );
    }
}
