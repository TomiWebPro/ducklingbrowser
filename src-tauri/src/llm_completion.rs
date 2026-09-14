use crate::ai_keys::{all_records, get_key, AiProvider};
use crate::llm::{ChatMessage, ChatUsage, LlmClient, ToolSpec};
use serde::{Deserialize, Serialize};

const DEFAULT_MAX_RETRIES: u32 = 3;

fn code_error(code: &str, params: serde_json::Value) -> String {
  serde_json::json!({ "code": code, "params": params }).to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LlmCompletionRequest {
  /// Saved key id from the key vault. When omitted, the first saved key is used.
  pub key_id: Option<String>,
  /// Provider override. When omitted, the key's own provider is used.
  pub provider: Option<String>,
  /// Model override. When omitted, the key's configured model is used.
  pub model: Option<String>,
  /// Chat history (roles: system/user/assistant).
  pub messages: Vec<ChatMessage>,
  /// Optional function-calling tools.
  #[serde(default)]
  pub tools: Option<Vec<ToolSpec>>,
  /// Retries on transient failures (429/5xx/transport). Defaults to 3.
  #[serde(default)]
  pub max_retries: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LlmCompletionResult {
  pub reply: String,
  pub usage: Option<ChatUsage>,
  /// Provider actually used (after overrides).
  pub provider: String,
  /// Model actually used (after overrides).
  pub model: String,
}

/// Resolve key + provider + model, then run a completion with retry.
pub async fn run_llm_completion(
  request: LlmCompletionRequest,
) -> Result<LlmCompletionResult, String> {
  let record = match &request.key_id {
    Some(id) => {
      get_key(id)?.ok_or_else(|| code_error("AI_KEY_NOT_FOUND", serde_json::json!({ "id": id })))?
    }
    None => all_records()
      .into_iter()
      .next()
      .ok_or_else(|| code_error("LLM_NO_KEY", serde_json::json!({})))?,
  };

  let provider: AiProvider = match &request.provider {
    Some(provider) => provider.parse().map_err(|_| {
      code_error(
        "LLM_UNKNOWN_PROVIDER",
        serde_json::json!({ "provider": provider }),
      )
    })?,
    None => record.provider.parse().map_err(|_| {
      code_error(
        "LLM_UNKNOWN_PROVIDER",
        serde_json::json!({ "provider": record.provider }),
      )
    })?,
  };
  let model = request
    .model
    .clone()
    .unwrap_or_else(|| record.model.clone());

  if request.messages.is_empty() {
    return Err(code_error("LLM_EMPTY_MESSAGES", serde_json::json!({})));
  }

  let client = LlmClient {
    provider,
    api_key: record.key.clone(),
    model: model.clone(),
    client: None,
    endpoint_override: record.endpoint.clone(),
  };
  let max_retries = request.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);

  let response = client
    .chat_with_retry(&request.messages, request.tools.as_deref(), max_retries)
    .await
    .map_err(|e| {
      if e.retryable {
        code_error(
          "LLM_RETRY_EXHAUSTED",
          serde_json::json!({ "detail": e.message }),
        )
      } else {
        code_error(
          "LLM_REQUEST_FAILED",
          serde_json::json!({ "detail": e.message }),
        )
      }
    })?;

  if let Some(u) = &response.usage {
    crate::ai_usage::record_usage(Some(record.id.as_str()), Some(provider.as_str()), None, u);
  }

  Ok(LlmCompletionResult {
    reply: response.text,
    usage: response.usage,
    provider: provider.as_str().to_string(),
    model,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn no_keys_yields_structured_no_key_error() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
      .block_on(run_llm_completion(LlmCompletionRequest {
        key_id: None,
        provider: None,
        model: None,
        messages: vec![ChatMessage::text("user", "hi")],
        tools: None,
        max_retries: None,
      }))
      .unwrap_err();
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(&err).unwrap()["code"],
      "LLM_NO_KEY"
    );
  }

  #[test]
  fn missing_key_id_yields_structured_not_found_error() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
      .block_on(run_llm_completion(LlmCompletionRequest {
        key_id: Some("missing-key".to_string()),
        provider: None,
        model: None,
        messages: vec![ChatMessage::text("user", "hi")],
        tools: None,
        max_retries: None,
      }))
      .unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["code"], "AI_KEY_NOT_FOUND");
    assert_eq!(parsed["params"]["id"], "missing-key");
  }

  #[test]
  fn empty_messages_are_rejected_before_any_request() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    crate::ai_keys::save_key("openai", "Main", "gpt-4o-mini", "sk-test", None).unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
      .block_on(run_llm_completion(LlmCompletionRequest {
        key_id: None,
        provider: None,
        model: None,
        messages: vec![],
        tools: None,
        max_retries: None,
      }))
      .unwrap_err();
    assert_eq!(
      serde_json::from_str::<serde_json::Value>(&err).unwrap()["code"],
      "LLM_EMPTY_MESSAGES"
    );
  }

  #[test]
  fn unknown_provider_override_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    crate::ai_keys::save_key("openai", "Main", "gpt-4o-mini", "sk-test", None).unwrap();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
      .block_on(run_llm_completion(LlmCompletionRequest {
        key_id: None,
        provider: Some("watson".to_string()),
        model: None,
        messages: vec![ChatMessage::text("user", "hi")],
        tools: None,
        max_retries: None,
      }))
      .unwrap_err();
    let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
    assert_eq!(parsed["code"], "LLM_UNKNOWN_PROVIDER");
    assert_eq!(parsed["params"]["provider"], "watson");
  }
}
