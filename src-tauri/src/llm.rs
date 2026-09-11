use serde::{Deserialize, Serialize};

use crate::ai_keys::AiProvider;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Default per-provider concurrency cap when no setting is configured.
pub const LLM_MAX_CONCURRENCY: usize = 8;
/// Default hourly request budget when no setting is configured (0 = unlimited).
pub const LLM_DEFAULT_REQUESTS_PER_HOUR: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
  pub role: String,
  pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
  pub name: String,
  pub description: String,
  pub input_schema: serde_json::Value,
}

/// Token usage reported by the provider, when the response carries it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChatUsage {
  pub prompt_tokens: u32,
  pub completion_tokens: u32,
  pub total_tokens: u32,
}

/// A completed non-streaming completion: the assistant text plus usage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LlmResponse {
  pub text: String,
  pub usage: Option<ChatUsage>,
}

#[derive(Debug, Clone)]
pub struct LlmError {
  pub message: String,
  /// True when the failure is transient (HTTP 429/5xx or a transport error)
  /// and a retry may succeed.
  pub retryable: bool,
}

impl std::fmt::Display for LlmError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.message)
  }
}

pub struct LlmClient {
  pub provider: AiProvider,
  pub api_key: String,
  pub model: String,
  /// Injectable HTTP client for tests; defaults to a timeout-bounded client.
  pub client: Option<reqwest::Client>,
  /// Injectable endpoint override for tests; when set, it replaces the
  /// provider's default URL entirely (including path).
  pub endpoint_override: Option<String>,
}

/// Wire protocol spoken for one request. OpenAI-compatible gateways expose
/// up to three flavors; the client follows the endpoint URL and, on the
/// OpenCode Go default endpoint, the per-model mapping below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiFlavor {
  ChatCompletions,
  Responses,
  AnthropicMessages,
}

/// OpenCode Go models served on the Responses API
/// (`.../zen/go/v1/responses`; see the Go docs endpoint table).
const OPENCODE_GO_RESPONSES_MODELS: &[&str] = &[
  "gpt-5.6-luna",
  "grok-4.6",
  "muse-spark-1.3-contributor",
  "muse-spark-1.2-contributor",
];

/// OpenCode Go models served on the Anthropic Messages API
/// (`.../zen/go/v1/messages`; see the Go docs endpoint table).
const OPENCODE_GO_MESSAGES_MODELS: &[&str] = &[
  "minimax-m3",
  "minimax-m2.7",
  "minimax-m2.5",
  "qwen3.8-max",
  "qwen3.8-flash",
  "qwen3.7-max",
  "qwen3.7-plus",
  "qwen3.6-plus",
  "qwen3.5-plus",
];

/// Pick the protocol for a request. An explicit endpoint path always wins —
/// speak the language of the endpoint — otherwise fall back to the provider
/// default (plus the OpenCode Go per-model map on its default endpoint).
fn resolve_flavor(provider: AiProvider, endpoint_override: Option<&str>, model: &str) -> ApiFlavor {
  if let Some(o) = endpoint_override {
    let trimmed = o.trim_end_matches('/');
    if trimmed.ends_with("/responses") {
      return ApiFlavor::Responses;
    }
    if trimmed.ends_with("/messages") {
      return ApiFlavor::AnthropicMessages;
    }
    if trimmed.ends_with("/chat/completions") {
      return ApiFlavor::ChatCompletions;
    }
  }
  match provider {
    AiProvider::Anthropic => ApiFlavor::AnthropicMessages,
    AiProvider::Opencode => {
      if OPENCODE_GO_RESPONSES_MODELS.contains(&model) {
        ApiFlavor::Responses
      } else if OPENCODE_GO_MESSAGES_MODELS.contains(&model) {
        ApiFlavor::AnthropicMessages
      } else {
        ApiFlavor::ChatCompletions
      }
    }
    _ => ApiFlavor::ChatCompletions,
  }
}

/// Rewrite a `/v1` base or `/chat/completions` URL onto the target flavor
/// path. Anything else (e.g. a vendor-specific full path) passes through.
fn flavor_url(base: &str, flavor: ApiFlavor) -> String {
  let target = match flavor {
    ApiFlavor::ChatCompletions => "/chat/completions",
    ApiFlavor::Responses => "/responses",
    ApiFlavor::AnthropicMessages => "/messages",
  };
  let trimmed = base.trim_end_matches('/');
  if let Some(root) = trimmed
    .strip_suffix("/chat/completions")
    .or_else(|| trimmed.strip_suffix("/responses"))
    .or_else(|| trimmed.strip_suffix("/messages"))
  {
    if flavor == ApiFlavor::ChatCompletions && trimmed.ends_with("/chat/completions") {
      return trimmed.to_string();
    }
    return format!("{root}{target}");
  }
  if trimmed.ends_with("/v1") {
    return format!("{trimmed}{target}");
  }
  trimmed.to_string()
}

impl LlmClient {
  fn endpoint_url(&self, provider: AiProvider, flavor: ApiFlavor) -> String {
    // Native providers keep their URL (an override points at a compatible
    // proxy and passes through verbatim, as before).
    if let Some(override_url) = &self.endpoint_override {
      match provider {
        AiProvider::Anthropic | AiProvider::Google => return override_url.clone(),
        _ => {}
      }
    }
    match provider {
      AiProvider::Anthropic => "https://api.anthropic.com/v1/messages".to_string(),
      AiProvider::Google => format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        self.model, self.api_key
      ),
      AiProvider::Openai => flavor_url(
        self
          .endpoint_override
          .as_deref()
          .unwrap_or("https://api.openai.com/v1/chat/completions"),
        flavor,
      ),
      AiProvider::Groq => flavor_url(
        self
          .endpoint_override
          .as_deref()
          .unwrap_or("https://api.groq.com/openai/v1/chat/completions"),
        flavor,
      ),
      AiProvider::Openrouter => flavor_url(
        self
          .endpoint_override
          .as_deref()
          .unwrap_or("https://openrouter.ai/api/v1/chat/completions"),
        flavor,
      ),
      AiProvider::Opencode | AiProvider::Custom => {
        let base = self.endpoint_override.clone().unwrap_or_else(|| {
          provider
            .default_endpoint()
            .unwrap_or("https://api.openai.com/v1/chat/completions")
            .to_string()
        });
        flavor_url(&base, flavor)
      }
    }
  }
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

/// Responses API request body: the transcript goes into `input` as message
/// items (system/user/assistant roles carry over); function tools use the
/// flat Responses shape instead of the chat-completions `function` wrapper.
fn responses_body(
  model: &str,
  messages: &[ChatMessage],
  tools: Option<&[ToolSpec]>,
) -> serde_json::Value {
  let mut body = serde_json::json!({
    "model": model,
    "input": messages
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
            "name": t.name,
            "description": t.description,
            "parameters": t.input_schema,
          })
        })
        .collect::<Vec<_>>());
    }
  }
  body
}

/// Extract the assistant text from a Responses API body by aggregating every
/// `output_text` block across all message items (never just `output[0]`,
/// which may hold reasoning or tool-call items).
fn extract_responses_text(body: &serde_json::Value) -> Result<String, String> {
  let items = body
    .get("output")
    .and_then(|v| v.as_array())
    .ok_or_else(|| "Response missing output".to_string())?;
  let text = items
    .iter()
    .filter(|item| item.get("type").and_then(|v| v.as_str()) == Some("message"))
    .filter_map(|item| item.get("content")?.as_array())
    .flatten()
    .filter(|block| block.get("type").and_then(|v| v.as_str()) == Some("output_text"))
    .filter_map(|block| block.get("text").and_then(|v| v.as_str()))
    .collect::<Vec<_>>()
    .join("");
  if text.trim().is_empty() {
    Err("Empty assistant response".to_string())
  } else {
    Ok(text)
  }
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

/// Extract token usage from a provider response body.
fn extract_usage(provider: AiProvider, body: &serde_json::Value) -> Option<ChatUsage> {
  match provider {
    AiProvider::Anthropic => {
      let usage = body.get("usage")?;
      let input = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
      let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
      Some(ChatUsage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input.saturating_add(output),
      })
    }
    AiProvider::Google => {
      let usage = body.get("usageMetadata")?;
      let prompt = usage
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
      let completion = usage
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
      Some(ChatUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt.saturating_add(completion),
      })
    }
    _ => {
      let usage = body.get("usage")?;
      let prompt = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
      let completion = usage
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
      Some(ChatUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: usage
          .get("total_tokens")
          .and_then(|v| v.as_u64())
          .map(|t| t as u32)
          .unwrap_or_else(|| prompt.saturating_add(completion)),
      })
    }
  }
}

/// HTTP statuses that warrant a retry: rate limit + all 5xx.
pub fn status_is_retryable(status: u16) -> bool {
  status == 429 || (500..=599).contains(&status)
}

/// Deterministic exponential backoff with jitter, capped at 60s.
/// `attempt` is 0-based (the first retry after the initial failure).
pub fn backoff_delay_ms(attempt: u32, base_ms: u64) -> u64 {
  let exponent = (1u64 << attempt.min(10)).saturating_sub(1);
  let base = base_ms
    .saturating_add(base_ms.saturating_mul(exponent))
    .min(MAX_BACKOFF_MS);
  let jitter_percent = (u64::from(attempt).wrapping_mul(37)) % 21;
  let jitter = base.saturating_mul(jitter_percent) / 100;
  (base + jitter).min(MAX_BACKOFF_MS)
}

/// Per-provider concurrency limit shared across every caller.
static LLM_SEMAPHORES: std::sync::LazyLock<
  std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Semaphore>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// The configured concurrency cap (min 1).
pub fn max_llm_concurrency() -> usize {
  crate::settings_manager::SettingsManager::instance()
    .load_settings()
    .ok()
    .map(|settings| settings.llm_max_concurrency.max(1))
    .unwrap_or(LLM_MAX_CONCURRENCY)
}

/// Semaphore guarding concurrent in-flight requests for one provider.
pub fn semaphore_for(provider: &str, cap: usize) -> std::sync::Arc<tokio::sync::Semaphore> {
  let mut semaphores = LLM_SEMAPHORES.lock().unwrap();
  semaphores
    .entry(provider.to_string())
    .or_insert_with(|| std::sync::Arc::new(tokio::sync::Semaphore::new(cap.max(1))))
    .clone()
}

impl LlmClient {
  /// Single attempt. Returns the assistant text plus usage when reported.
  pub async fn send(
    &self,
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
  ) -> Result<LlmResponse, LlmError> {
    let client = match &self.client {
      Some(client) => client.clone(),
      None => reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| LlmError {
          message: format!("Failed to build HTTP client: {e}"),
          retryable: false,
        })?,
    };

    let flavor = resolve_flavor(
      self.provider,
      self.endpoint_override.as_deref(),
      &self.model,
    );
    let url = self.endpoint_url(self.provider, flavor);
    // The Responses usage shape ({input_tokens, output_tokens}) matches the
    // Anthropic one, so usage parsing follows the response shape, not the
    // provider. Anthropic-messages via a compatible gateway authenticates
    // with a bearer token instead of `x-api-key`.
    let via_gateway_messages =
      flavor == ApiFlavor::AnthropicMessages && self.provider != AiProvider::Anthropic;
    let is_responses = flavor == ApiFlavor::Responses
      && self.provider != AiProvider::Anthropic
      && self.provider != AiProvider::Google;
    let body = match self.provider {
      AiProvider::Anthropic => anthropic_body(&self.model, messages, tools),
      AiProvider::Google => google_body(messages),
      _ => match flavor {
        ApiFlavor::ChatCompletions => openai_compat_body(&self.model, messages, tools),
        ApiFlavor::Responses => responses_body(&self.model, messages, tools),
        ApiFlavor::AnthropicMessages => anthropic_body(&self.model, messages, tools),
      },
    };

    let mut request = match self.provider {
      AiProvider::Anthropic => client
        .post(url)
        .header("x-api-key", &self.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body),
      AiProvider::Google => client.post(url).json(&body),
      _ if via_gateway_messages => client
        .post(url)
        .bearer_auth(&self.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body),
      _ => client.post(url).bearer_auth(&self.api_key).json(&body),
    };
    if self.provider == AiProvider::Openrouter {
      request = request.header("HTTP-Referer", "https://ducklingbrowser.com");
    }

    let response = request.send().await.map_err(|e| LlmError {
      message: format!("Request failed: {e}"),
      retryable: true,
    })?;
    let status = response.status();
    let response_body: serde_json::Value = response.json().await.map_err(|e| LlmError {
      message: format!("Invalid response body: {e}"),
      retryable: false,
    })?;

    if !status.is_success() {
      let detail = response_body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown provider error")
        .chars()
        .take(300)
        .collect::<String>();
      return Err(LlmError {
        message: format!("HTTP {status}: {detail}"),
        retryable: status_is_retryable(status.as_u16()),
      });
    }

    // Gateway Anthropic-messages and Responses bodies parse like native
    // Anthropic ones (content blocks / input_tokens), not like choices.
    let shaped_as_anthropic =
      self.provider == AiProvider::Anthropic || is_responses || via_gateway_messages;
    let text = if is_responses {
      extract_responses_text(&response_body)
    } else if shaped_as_anthropic {
      extract_text(AiProvider::Anthropic, &response_body)
    } else {
      extract_text(self.provider, &response_body)
    }
    .map_err(|message| LlmError {
      message,
      retryable: false,
    })?;
    let shape_provider = if shaped_as_anthropic {
      AiProvider::Anthropic
    } else {
      self.provider
    };
    Ok(LlmResponse {
      text,
      usage: extract_usage(shape_provider, &response_body),
    })
  }

  /// Single-shot completion; returns only the assistant text.
  pub async fn chat(
    &self,
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
  ) -> Result<String, LlmError> {
    self
      .send(messages, tools)
      .await
      .map(|response| response.text)
  }

  /// Completion with exponential-backoff retries on transient failures
  /// (429/5xx/transport), bounded by `max_retries` retries, and a
  /// per-provider concurrency permit shared across all callers.
  pub async fn chat_with_retry(
    &self,
    messages: &[ChatMessage],
    tools: Option<&[ToolSpec]>,
    max_retries: u32,
  ) -> Result<LlmResponse, LlmError> {
    // Held for the whole call: bounds per-provider concurrency across callers.
    let _permit = semaphore_for(self.provider.as_str(), max_llm_concurrency())
      .acquire_owned()
      .await
      .map_err(|_| LlmError {
        message: "LLM concurrency semaphore closed".to_string(),
        retryable: false,
      })?;

    let mut attempts: u32 = 0;
    loop {
      match self.send(messages, tools).await {
        Ok(response) => return Ok(response),
        Err(e) if e.retryable && attempts < max_retries => {
          let delay_ms = backoff_delay_ms(attempts, 1000);
          log::info!(
            "LLM request failed transiently (attempt {}): {}; retrying in {}ms",
            attempts + 1,
            e.message,
            delay_ms
          );
          tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
          attempts += 1;
        }
        Err(e) => return Err(e),
      }
    }
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
  fn resolve_flavor_follows_the_endpoint_url() {
    // An explicit path always wins, regardless of provider.
    assert_eq!(
      resolve_flavor(
        AiProvider::Openai,
        Some("https://gw.example.com/v1/responses"),
        "gpt-4o"
      ),
      ApiFlavor::Responses
    );
    assert_eq!(
      resolve_flavor(
        AiProvider::Opencode,
        Some("https://opencode.ai/zen/go/v1/messages"),
        "kimi-k3"
      ),
      ApiFlavor::AnthropicMessages
    );
    assert_eq!(
      resolve_flavor(
        AiProvider::Custom,
        Some("http://localhost:11434/v1/chat/completions"),
        "qwen3"
      ),
      ApiFlavor::ChatCompletions
    );
    // Trailing slashes don't confuse the match.
    assert_eq!(
      resolve_flavor(AiProvider::Custom, Some("https://gw.example.com/v1/"), "m"),
      ApiFlavor::ChatCompletions
    );
  }

  #[test]
  fn resolve_flavor_opencode_model_map() {
    assert_eq!(
      resolve_flavor(AiProvider::Opencode, None, "grok-4.6"),
      ApiFlavor::Responses
    );
    assert_eq!(
      resolve_flavor(AiProvider::Opencode, None, "muse-spark-1.3-contributor"),
      ApiFlavor::Responses
    );
    assert_eq!(
      resolve_flavor(AiProvider::Opencode, None, "minimax-m3"),
      ApiFlavor::AnthropicMessages
    );
    assert_eq!(
      resolve_flavor(AiProvider::Opencode, None, "qwen3.7-max"),
      ApiFlavor::AnthropicMessages
    );
    assert_eq!(
      resolve_flavor(AiProvider::Opencode, None, "kimi-k3"),
      ApiFlavor::ChatCompletions
    );
    // Unknown models default to chat completions, the most common flavor.
    assert_eq!(
      resolve_flavor(AiProvider::Opencode, None, "something-new"),
      ApiFlavor::ChatCompletions
    );
    assert_eq!(
      resolve_flavor(AiProvider::Anthropic, None, "claude-sonnet-5"),
      ApiFlavor::AnthropicMessages
    );
    assert_eq!(
      resolve_flavor(AiProvider::Openai, None, "gpt-4o-mini"),
      ApiFlavor::ChatCompletions
    );
  }

  #[test]
  fn flavor_url_rewrites_bases_and_passes_custom_paths_through() {
    assert_eq!(
      flavor_url(
        "https://opencode.ai/zen/go/v1/chat/completions",
        ApiFlavor::Responses
      ),
      "https://opencode.ai/zen/go/v1/responses"
    );
    assert_eq!(
      flavor_url(
        "https://opencode.ai/zen/go/v1/chat/completions",
        ApiFlavor::AnthropicMessages
      ),
      "https://opencode.ai/zen/go/v1/messages"
    );
    assert_eq!(
      flavor_url("https://opencode.ai/zen/go/v1", ApiFlavor::Responses),
      "https://opencode.ai/zen/go/v1/responses"
    );
    assert_eq!(
      flavor_url(
        "https://opencode.ai/zen/go/v1/responses",
        ApiFlavor::ChatCompletions
      ),
      "https://opencode.ai/zen/go/v1/chat/completions"
    );
    // Idempotent when already on the target flavor.
    assert_eq!(
      flavor_url(
        "https://opencode.ai/zen/go/v1/messages",
        ApiFlavor::AnthropicMessages
      ),
      "https://opencode.ai/zen/go/v1/messages"
    );
    // Vendor-specific full paths pass through untouched.
    assert_eq!(
      flavor_url(
        "https://gw.example.com/openai/deployments/x/chat",
        ApiFlavor::ChatCompletions
      ),
      "https://gw.example.com/openai/deployments/x/chat"
    );
  }

  #[test]
  fn endpoint_url_uses_flavor_for_compat_providers() {
    let client = LlmClient {
      provider: AiProvider::Opencode,
      api_key: "k".to_string(),
      model: "grok-4.6".to_string(),
      client: None,
      endpoint_override: None,
    };
    assert_eq!(
      client.endpoint_url(AiProvider::Opencode, ApiFlavor::Responses),
      "https://opencode.ai/zen/go/v1/responses"
    );
    assert_eq!(
      client.endpoint_url(AiProvider::Opencode, ApiFlavor::ChatCompletions),
      "https://opencode.ai/zen/go/v1/chat/completions"
    );
  }

  #[test]
  fn responses_body_shape() {
    let messages = vec![msg("system", "Be brief."), msg("user", "Hi")];
    let body = responses_body("grok-4.6", &messages, Some(&[tool()]));
    assert_eq!(body["model"], "grok-4.6");
    assert_eq!(body["input"][0]["role"], "system");
    assert_eq!(body["input"][1]["content"], "Hi");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "navigate");
    assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    assert!(body["tools"][0].get("function").is_none());
    let body = responses_body("grok-4.6", &messages, None);
    assert!(body.get("tools").is_none());
  }

  #[test]
  fn extract_responses_text_aggregates_message_items() {
    let body = serde_json::json!({
      "output": [
        { "type": "reasoning", "summary": [] },
        { "type": "message", "content": [
          { "type": "output_text", "text": "hi " },
          { "type": "output_text", "text": "there" },
        ] },
        { "type": "function_call", "name": "x" },
      ],
      "usage": { "input_tokens": 7, "output_tokens": 3, "total_tokens": 10 },
    });
    assert_eq!(extract_responses_text(&body).unwrap(), "hi there");
    // Usage shares the Anthropic key shape.
    assert_eq!(
      extract_usage(AiProvider::Anthropic, &body),
      Some(ChatUsage {
        prompt_tokens: 7,
        completion_tokens: 3,
        total_tokens: 10
      })
    );
    assert!(extract_responses_text(&serde_json::json!({})).is_err());
    assert!(extract_responses_text(&serde_json::json!({ "output": [
        { "type": "message", "content": [] }
      ] }))
    .is_err());
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

  #[test]
  fn retryable_statuses_cover_rate_limit_and_five_xx() {
    assert!(status_is_retryable(429));
    assert!(status_is_retryable(500));
    assert!(status_is_retryable(502));
    assert!(status_is_retryable(503));
    assert!(!status_is_retryable(400));
    assert!(!status_is_retryable(401));
    assert!(!status_is_retryable(404));
    assert!(!status_is_retryable(200));
  }

  #[test]
  fn semaphore_for_caps_concurrent_permits_per_provider() {
    let cap_1 = semaphore_for("e2e-cap-provider", 1);
    let _held = cap_1.try_acquire().unwrap();
    assert!(cap_1.try_acquire().is_err());
    drop(_held);
    assert!(cap_1.try_acquire().is_ok());

    // A different provider gets its own semaphore, independent of the first.
    let other = semaphore_for("e2e-cap-other", 1);
    let _held_other = other.try_acquire().unwrap();
    assert!(other.try_acquire().is_err());
    drop(_held_other);

    // A raised cap applies to new providers (worst case 8).
    let capped = semaphore_for("e2e-cap-raised", 1000);
    let mut held = Vec::new();
    for _ in 0..1000 {
      held.push(capped.try_acquire().unwrap());
    }
    assert!(capped.try_acquire().is_err());
  }

  #[test]
  fn backoff_grows_with_attempt_and_stays_capped() {
    let first = backoff_delay_ms(0, 1000);
    let second = backoff_delay_ms(1, 1000);
    let third = backoff_delay_ms(2, 1000);
    assert!(first >= 1000);
    assert!(second > first);
    assert!(third > second);
    assert!(backoff_delay_ms(10, 1000) <= 60_000);
    assert!(backoff_delay_ms(40, 1000) <= 60_000);
  }

  #[test]
  fn extract_usage_across_providers() {
    let openai = serde_json::json!({
      "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
    });
    assert_eq!(
      extract_usage(AiProvider::Openai, &openai),
      Some(ChatUsage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15
      })
    );
    let anthropic = serde_json::json!({
      "usage": { "input_tokens": 12, "output_tokens": 4 }
    });
    assert_eq!(
      extract_usage(AiProvider::Anthropic, &anthropic),
      Some(ChatUsage {
        prompt_tokens: 12,
        completion_tokens: 4,
        total_tokens: 16
      })
    );
    let google = serde_json::json!({
      "usageMetadata": { "promptTokenCount": 8, "candidatesTokenCount": 2 }
    });
    assert_eq!(
      extract_usage(AiProvider::Google, &google),
      Some(ChatUsage {
        prompt_tokens: 8,
        completion_tokens: 2,
        total_tokens: 10
      })
    );
    assert_eq!(
      extract_usage(AiProvider::Openai, &serde_json::json!({})),
      None
    );
  }

  #[test]
  fn send_parses_success_and_returns_usage() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .and(wiremock::matchers::header(
          "authorization",
          "Bearer sk-test",
        ))
        .respond_with(
          wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{ "message": { "content": "hello world" } }],
            "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 }
          })),
        )
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Openai,
        api_key: "sk-test".to_string(),
        model: "gpt-4o-mini".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/chat/completions", mock_server.uri())),
      };
      let response = client.send(&[msg("user", "hi")], None).await.unwrap();
      assert_eq!(response.text, "hello world");
      assert_eq!(
        response.usage,
        Some(ChatUsage {
          prompt_tokens: 3,
          completion_tokens: 2,
          total_tokens: 5
        })
      );
    });
  }

  #[test]
  fn send_speaks_responses_flavor_end_to_end() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .and(wiremock::matchers::header(
          "authorization",
          "Bearer sk-test",
        ))
        .respond_with(
          wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [
              { "type": "reasoning", "summary": [] },
              { "type": "message", "content": [
                { "type": "output_text", "text": "resp ok" },
              ] },
            ],
            "usage": { "input_tokens": 4, "output_tokens": 2, "total_tokens": 6 }
          })),
        )
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Opencode,
        api_key: "sk-test".to_string(),
        model: "grok-4.6".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/responses", mock_server.uri())),
      };
      let response = client.send(&[msg("user", "hi")], None).await.unwrap();
      assert_eq!(response.text, "resp ok");
      assert_eq!(
        response.usage,
        Some(ChatUsage {
          prompt_tokens: 4,
          completion_tokens: 2,
          total_tokens: 6
        })
      );
      let received = mock_server.received_requests().await.unwrap();
      assert_eq!(received.len(), 1);
      let sent: serde_json::Value = received[0].body_json().unwrap();
      assert_eq!(sent["model"], "grok-4.6");
      assert_eq!(sent["input"][0]["role"], "user");
    });
  }

  #[test]
  fn send_speaks_gateway_messages_flavor_end_to_end() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .and(wiremock::matchers::header(
          "authorization",
          "Bearer sk-test",
        ))
        .respond_with(
          wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{ "type": "text", "text": "msg ok" }],
            "usage": { "input_tokens": 5, "output_tokens": 1 }
          })),
        )
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Opencode,
        api_key: "sk-test".to_string(),
        model: "minimax-m3".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/messages", mock_server.uri())),
      };
      let response = client.send(&[msg("user", "hi")], None).await.unwrap();
      assert_eq!(response.text, "msg ok");
      assert_eq!(
        response.usage,
        Some(ChatUsage {
          prompt_tokens: 5,
          completion_tokens: 1,
          total_tokens: 6
        })
      );
    });
  }

  #[test]
  fn resolve_flavor_explicit_chat_path_beats_model_map() {
    // A responses-model pointed at a chat endpoint speaks chat, and a
    // chat-model pointed at a messages endpoint speaks messages.
    assert_eq!(
      resolve_flavor(
        AiProvider::Opencode,
        Some("https://opencode.ai/zen/go/v1/chat/completions"),
        "grok-4.6"
      ),
      ApiFlavor::ChatCompletions
    );
    assert_eq!(
      resolve_flavor(
        AiProvider::Opencode,
        Some("https://opencode.ai/zen/go/v1/responses"),
        "kimi-k3"
      ),
      ApiFlavor::Responses
    );
    // A bare /v1 base carries no flavor signal: provider default applies.
    assert_eq!(
      resolve_flavor(
        AiProvider::Custom,
        Some("http://localhost:11434/v1"),
        "qwen3"
      ),
      ApiFlavor::ChatCompletions
    );
  }

  #[test]
  fn endpoint_url_appends_chat_path_to_v1_bases() {
    let client = LlmClient {
      provider: AiProvider::Openai,
      api_key: "k".to_string(),
      model: "gpt-4o-mini".to_string(),
      client: None,
      endpoint_override: Some("http://localhost:11434/v1".to_string()),
    };
    assert_eq!(
      client.endpoint_url(AiProvider::Openai, ApiFlavor::ChatCompletions),
      "http://localhost:11434/v1/chat/completions"
    );
    assert_eq!(
      client.endpoint_url(AiProvider::Openai, ApiFlavor::Responses),
      "http://localhost:11434/v1/responses"
    );
  }

  #[test]
  fn send_native_anthropic_keeps_api_key_auth() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .and(wiremock::matchers::header("x-api-key", "sk-ant"))
        .respond_with(
          wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{ "type": "text", "text": "native ok" }],
            "usage": { "input_tokens": 6, "output_tokens": 2 }
          })),
        )
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Anthropic,
        api_key: "sk-ant".to_string(),
        model: "claude-sonnet-5".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/messages", mock_server.uri())),
      };
      let response = client
        .send(&[msg("system", "sys"), msg("user", "hi")], None)
        .await
        .unwrap();
      assert_eq!(response.text, "native ok");
      assert_eq!(
        response.usage,
        Some(ChatUsage {
          prompt_tokens: 6,
          completion_tokens: 2,
          total_tokens: 8
        })
      );
      let received = mock_server.received_requests().await.unwrap();
      let sent: serde_json::Value = received[0].body_json().unwrap();
      assert_eq!(sent["system"], "sys");
      assert_eq!(sent["model"], "claude-sonnet-5");
    });
  }

  #[test]
  fn send_native_google_uses_key_url() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(
          "/v1beta/models/gemini-2.5-flash:generateContent",
        ))
        .respond_with(
          wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [{ "content": { "parts": [{ "text": "g ok" }] } }],
            "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 1 }
          })),
        )
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Google,
        api_key: "g-key".to_string(),
        model: "gemini-2.5-flash".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!(
          "{}/v1beta/models/gemini-2.5-flash:generateContent?key=g-key",
          mock_server.uri()
        )),
      };
      let response = client.send(&[msg("user", "hi")], None).await.unwrap();
      assert_eq!(response.text, "g ok");
      assert_eq!(
        response.usage,
        Some(ChatUsage {
          prompt_tokens: 3,
          completion_tokens: 1,
          total_tokens: 4
        })
      );
    });
  }

  #[test]
  fn send_responses_with_tools_serializes_flat_function_tools() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(
          wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [
              { "type": "message", "content": [
                { "type": "output_text", "text": "tooled" },
              ] },
            ],
            "usage": { "input_tokens": 9, "output_tokens": 1, "total_tokens": 10 }
          })),
        )
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Custom,
        api_key: "k".to_string(),
        model: "m".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/responses", mock_server.uri())),
      };
      let response = client
        .send(&[msg("user", "hi")], Some(&[tool()]))
        .await
        .unwrap();
      assert_eq!(response.text, "tooled");
      let received = mock_server.received_requests().await.unwrap();
      let sent: serde_json::Value = received[0].body_json().unwrap();
      assert_eq!(sent["tools"][0]["type"], "function");
      assert_eq!(sent["tools"][0]["name"], "navigate");
      assert!(sent["tools"][0].get("function").is_none());
    });
  }

  #[test]
  fn send_marks_transient_failures_retryable_and_client_errors_not() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
          wiremock::ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": { "message": "overloaded" }
          })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Openai,
        api_key: "sk-test".to_string(),
        model: "gpt-4o-mini".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/chat/completions", mock_server.uri())),
      };
      let err = client.send(&[msg("user", "hi")], None).await.unwrap_err();
      assert!(err.retryable);
      assert!(err.message.contains("503"));

      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
          wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": { "message": "bad request" }
          })),
        )
        .mount(&mock_server)
        .await;
      let err = client.send(&[msg("user", "hi")], None).await.unwrap_err();
      assert!(!err.retryable);
    });
  }

  #[test]
  fn chat_with_retry_retries_then_succeeds() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      // The 503 mock only serves the first request, then falls through.
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
          wiremock::ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": { "message": "busy" }
          })),
        )
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
          wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{ "message": { "content": "recovered" } }]
          })),
        )
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Openai,
        api_key: "sk-test".to_string(),
        model: "gpt-4o-mini".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/chat/completions", mock_server.uri())),
      };
      // First attempt hits the 503 mock, the retry hits the 200 mock.
      let result = client
        .chat_with_retry(&[msg("user", "hi")], None, 3)
        .await
        .unwrap();
      assert_eq!(result.text, "recovered");
      let received = mock_server.received_requests().await.unwrap();
      assert_eq!(received.len(), 2);
    });
  }

  #[test]
  fn chat_with_retry_gives_up_after_max_retries() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
          wiremock::ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": { "message": "busy" }
          })),
        )
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Openai,
        api_key: "sk-test".to_string(),
        model: "gpt-4o-mini".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/chat/completions", mock_server.uri())),
      };
      let start = std::time::Instant::now();
      let err = client
        .chat_with_retry(&[msg("user", "hi")], None, 2)
        .await
        .unwrap_err();
      // 1 initial + 2 retries, backoffs ~1s + ~3s.
      assert!(err.retryable);
      assert!(start.elapsed().as_secs() >= 3);
    });
  }

  #[test]
  fn chat_with_retry_does_not_retry_client_errors() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      let mock_server = wiremock::MockServer::start().await;
      wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
          wiremock::ResponseTemplate::new(400)
            .set_body_json(serde_json::json!({ "error": { "message": "nope" } })),
        )
        .up_to_n_times(10)
        .mount(&mock_server)
        .await;

      let client = LlmClient {
        provider: AiProvider::Openai,
        api_key: "sk-test".to_string(),
        model: "gpt-4o-mini".to_string(),
        client: Some(
          reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
        ),
        endpoint_override: Some(format!("{}/v1/chat/completions", mock_server.uri())),
      };
      let err = client
        .chat_with_retry(&[msg("user", "hi")], None, 5)
        .await
        .unwrap_err();
      assert!(!err.retryable);
      // A non-retryable failure must not have been retried.
      let received = mock_server.received_requests().await.unwrap();
      assert_eq!(received.len(), 1);
    });
  }
}
