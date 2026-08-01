use serde::{Deserialize, Serialize};

use crate::ai_keys::AiProvider;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
  pub role: String,
  pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
  pub name: String,
  pub description: String,
  pub input_schema: serde_json::Value,
}

pub struct LlmError(pub String);

pub struct LlmClient {
  pub provider: AiProvider,
  pub api_key: String,
  pub model: String,
}

/// OpenAI-compatible request body (openai / groq / openrouter).
fn openai_compat_body(
  model: &str,
  messages: &[ChatMessage],
  tools: Option<&[ToolSpec]>,
) -> serde_json::Value {
  let mut body = serde_json::json!({
    "model": model,
    "messages": messages
      .iter()
      .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
      .collect::<Vec<_>>(),
  });
  if let Some(tools) = tools {
    if !tools.is_empty() {
      body["tools"] = serde_json::json!(tools
        .iter()
        .map(|t| {
          serde_json::json!({
            "type": "function",
            "function": {
              "name": t.name,
              "description": t.description,
              "parameters": t.input_schema,
            }
          })
        })
        .collect::<Vec<_>>());
    }
  }
  body
}

/// Anthropic request body: system messages move to the top-level `system` field.
fn anthropic_body(
  model: &str,
  messages: &[ChatMessage],
  tools: Option<&[ToolSpec]>,
) -> serde_json::Value {
  let system: String = messages
    .iter()
    .filter(|m| m.role == "system")
    .map(|m| m.content.as_str())
    .collect::<Vec<_>>()
    .join("\n");
  let mut body = serde_json::json!({
    "model": model,
    "max_tokens": 4096,
    "messages": messages
      .iter()
      .filter(|m| m.role != "system")
      .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
      .collect::<Vec<_>>(),
  });
  if !system.is_empty() {
    body["system"] = serde_json::Value::String(system);
  }
  if let Some(tools) = tools {
    if !tools.is_empty() {
      body["tools"] = serde_json::json!(tools
        .iter()
        .map(|t| {
          serde_json::json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.input_schema,
          })
        })
        .collect::<Vec<_>>());
    }
  }
  body
}

fn google_body(messages: &[ChatMessage]) -> serde_json::Value {
  let system: String = messages
    .iter()
    .filter(|m| m.role == "system")
    .map(|m| m.content.as_str())
    .collect::<Vec<_>>()
    .join("\n");
  let mut body = serde_json::json!({
    "contents": messages
      .iter()
      .filter(|m| m.role != "system")
      .map(|m| {
        serde_json::json!({
          "role": if m.role == "assistant" { "model" } else { "user" },
          "parts": [{ "text": m.content }]
        })
      })
      .collect::<Vec<_>>(),
  });
  if !system.is_empty() {
    body["systemInstruction"] = serde_json::json!({ "parts": [{ "text": system }] });
  }
  body
}

/// Extract the assistant text from a provider response body.
fn extract_text(provider: AiProvider, body: &serde_json::Value) -> Result<String, String> {
  match provider {
    AiProvider::Anthropic => {
      let blocks = body
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Response missing content".to_string())?;
      let text = blocks
        .iter()
        .filter_map(|b| {
          if b.get("type").and_then(|v| v.as_str()) == Some("text") {
            b.get("text").and_then(|v| v.as_str())
          } else {
            None
          }
        })
        .collect::<Vec<_>>()
        .join("");
      if text.is_empty() {
        Err("Empty assistant response".to_string())
      } else {
        Ok(text)
      }
    }
    AiProvider::Google => {
      let parts = body
        .get("candidates")
        .and_then(|v| v.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Response missing candidates".to_string())?;
      let text = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("");
      if text.is_empty() {
        Err("Empty assistant response".to_string())
      } else {
        Ok(text)
      }
    }
    _ => {
      let content = body
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .ok_or_else(|| "Response missing choices".to_string())?;
      match content {
        serde_json::Value::String(s) => {
          if s.trim().is_empty() {
            Err("Empty assistant response".to_string())
          } else {
            Ok(s.clone())
          }
        }
        serde_json::Value::Array(parts) => {
          let text = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("");
          if text.is_empty() {
            Err("Empty assistant response".to_string())
          } else {
            Ok(text)
          }
        }
        serde_json::Value::Null => Err("Empty assistant response".to_string()),
        _ => Err("Unexpected message content shape".to_string()),
      }
    }
  }
}

impl LlmClient {
  pub async fn chat(
    &self,
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
  ) -> Result<String, LlmError> {
    let client = reqwest::Client::builder()
      .timeout(REQUEST_TIMEOUT)
      .build()
      .map_err(|e| LlmError(format!("Failed to build HTTP client: {e}")))?;

    let (url, body) = match self.provider {
      AiProvider::Anthropic => (
        "https://api.anthropic.com/v1/messages".to_string(),
        anthropic_body(&self.model, messages, tools),
      ),
      AiProvider::Google => (
        format!(
          "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
          self.model, self.api_key
        ),
        google_body(messages),
      ),
      AiProvider::Openai => (
        "https://api.openai.com/v1/chat/completions".to_string(),
        openai_compat_body(&self.model, messages, tools),
      ),
      AiProvider::Groq => (
        "https://api.groq.com/openai/v1/chat/completions".to_string(),
        openai_compat_body(&self.model, messages, tools),
      ),
      AiProvider::Openrouter => (
        "https://openrouter.ai/api/v1/chat/completions".to_string(),
        openai_compat_body(&self.model, messages, tools),
      ),
    };

    let mut request = match self.provider {
      AiProvider::Anthropic => client
        .post(url)
        .header("x-api-key", &self.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body),
      AiProvider::Google => client.post(url).json(&body),
      _ => client.post(url).bearer_auth(&self.api_key).json(&body),
    };
    if self.provider == AiProvider::Openrouter {
      request = request.header("HTTP-Referer", "https://ducklingbrowser.com");
    }

    let response = request
      .send()
      .await
      .map_err(|e| LlmError(format!("Request failed: {e}")))?;
    let status = response.status();
    let response_body: serde_json::Value = response
      .json()
      .await
      .map_err(|e| LlmError(format!("Invalid response body: {e}")))?;

    if !status.is_success() {
      let detail = response_body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown provider error")
        .chars()
        .take(300)
        .collect::<String>();
      return Err(LlmError(format!("HTTP {status}: {detail}")));
    }

    extract_text(self.provider, &response_body).map_err(LlmError)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn msg(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
      role: role.to_string(),
      content: content.to_string(),
    }
  }

  fn tool() -> ToolSpec {
    ToolSpec {
      name: "navigate".to_string(),
      description: "Navigate a profile".to_string(),
      input_schema: serde_json::json!({ "type": "object", "properties": {} }),
    }
  }

  #[test]
  fn openai_compat_body_shape() {
    let messages = vec![
      msg("system", "You are a browser agent."),
      msg("user", "Go to example.com"),
    ];
    let body = openai_compat_body("gpt-4o-mini", &messages, Some(&[tool()]));
    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["content"], "Go to example.com");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "navigate");
    let body = openai_compat_body("gpt-4o-mini", &messages, None);
    assert!(body.get("tools").is_none());
  }

  #[test]
  fn anthropic_body_moves_system_out() {
    let messages = vec![
      msg("system", "Rules here."),
      msg("user", "Hi"),
      msg("assistant", "Hello"),
    ];
    let body = anthropic_body("claude-sonnet-4-5", &messages, Some(&[tool()]));
    assert_eq!(body["system"], "Rules here.");
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["tools"][0]["name"], "navigate");
    assert!(body["tools"][0]["input_schema"].is_object());
  }

  #[test]
  fn google_body_uses_model_role_and_system_instruction() {
    let messages = vec![
      msg("system", "Be brief."),
      msg("user", "Hi"),
      msg("assistant", "Hello there"),
    ];
    let body = google_body(&messages);
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be brief.");
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(body["contents"][1]["role"], "model");
    assert_eq!(body["contents"][1]["parts"][0]["text"], "Hello there");
  }

  #[test]
  fn extract_text_openai_string_and_array() {
    let body = serde_json::json!({
      "choices": [{ "message": { "content": "plain text" } }]
    });
    assert_eq!(
      extract_text(AiProvider::Openai, &body).unwrap(),
      "plain text"
    );
    let body = serde_json::json!({
      "choices": [{ "message": { "content": [{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }] } }]
    });
    assert_eq!(extract_text(AiProvider::Openai, &body).unwrap(), "ab");
    let body = serde_json::json!({ "choices": [] });
    assert!(extract_text(AiProvider::Openai, &body).is_err());
  }

  #[test]
  fn extract_text_anthropic_and_google() {
    let body = serde_json::json!({
      "content": [
        { "type": "tool_use", "name": "x" },
        { "type": "text", "text": "hi " },
        { "type": "text", "text": "there" }
      ]
    });
    assert_eq!(
      extract_text(AiProvider::Anthropic, &body).unwrap(),
      "hi there"
    );
    let body = serde_json::json!({
      "candidates": [{ "content": { "parts": [{ "text": "gm" }, { "text": "!" }] } }]
    });
    assert_eq!(extract_text(AiProvider::Google, &body).unwrap(), "gm!");
  }
}
