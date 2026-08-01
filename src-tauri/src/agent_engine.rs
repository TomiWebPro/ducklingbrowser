use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ai_keys::{all_records, get_key, AiProvider};
use crate::llm::{ChatMessage, LlmClient, ToolSpec};

const MAX_TOOL_ITERATIONS: usize = 20;
const DELEGATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChangeCard {
  pub id: String,
  pub kind: String,
  pub title: String,
  pub description: String,
  pub diff: serde_json::Value,
  pub reversible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentChatResult {
  pub reply: String,
  pub cards: Vec<ChangeCard>,
}

/// Pending mutations recorded as cards, awaiting confirmation. Keyed by card
/// id so `agent_chat_confirm` can re-dispatch the original tool call.
static PENDING_ACTIONS: LazyLock<Mutex<HashMap<String, (String, serde_json::Value)>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

fn code_error(code: &str, params: serde_json::Value) -> String {
  serde_json::json!({ "code": code, "params": params }).to_string()
}

fn agent_error(detail: &str) -> String {
  code_error("AGENT_LLM_ERROR", serde_json::json!({ "detail": detail }))
}

fn no_key() -> String {
  code_error("AGENT_NO_KEY", serde_json::json!({}))
}

fn delegate_not_found(id: &str) -> String {
  code_error("AGENT_DELEGATE_NOT_FOUND", serde_json::json!({ "id": id }))
}

fn card_not_found(id: &str) -> String {
  code_error("AGENT_CARD_NOT_FOUND", serde_json::json!({ "id": id }))
}

/// Tools whose execution is read-only and runs immediately in the agent loop.
/// Everything else is recorded as a ChangeCard awaiting confirmation.
fn is_read_only_tool(name: &str) -> bool {
  matches!(
    name,
    "list_profiles"
      | "get_profile"
      | "get_profile_status"
      | "list_proxies"
      | "list_groups"
      | "get_group"
      | "get_proxy"
      | "list_tags"
      | "get_vpn_status"
      | "list_extensions"
      | "list_extension_groups"
      | "get_dns_blocklist_status"
      | "get_profile_fingerprint"
      | "screenshot"
      | "get_page_content"
      | "get_page_info"
      | "get_interactive_elements"
  )
}

fn card_kind_for(name: &str) -> &'static str {
  match name {
    "navigate" => "navigate",
    "run_profile" => "run_browser",
    "update_profile"
    | "create_profile"
    | "delete_profile"
    | "import_browser_profiles"
    | "update_profile_fingerprint"
    | "update_profile_proxy_bypass_rules"
    | "update_profile_dns_blocklist"
    | "assign_extension_group_to_profile" => "profile_update",
    "update_proxy" | "create_proxy" | "delete_proxy" | "import_proxies" | "import_vpn" => "proxy",
    _ => "custom",
  }
}

fn card_title_for(tool_name: &str, args: &serde_json::Value) -> String {
  match tool_name {
    "navigate" => format!(
      "Navigate to {}",
      args.get("url").and_then(|v| v.as_str()).unwrap_or("?")
    ),
    "run_profile" => format!(
      "Launch browser profile {}",
      args
        .get("profile_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
    ),
    "kill_profile" => format!(
      "Stop browser profile {}",
      args
        .get("profile_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
    ),
    "update_profile"
    | "create_profile"
    | "delete_profile"
    | "import_browser_profiles"
    | "update_profile_fingerprint"
    | "update_profile_proxy_bypass_rules"
    | "update_profile_dns_blocklist"
    | "assign_extension_group_to_profile" => format!(
      "{} (profile {})",
      tool_name.replace('_', " "),
      args
        .get("profile_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
    ),
    "evaluate_javascript" => "Execute JavaScript in the page".to_string(),
    "click_element" | "click_by_index" => format!("Click {}", tool_name),
    "type_text" | "type_by_index" => format!("Type into {}", tool_name),
    _ => tool_name.replace('_', " "),
  }
}

fn tool_schema(name: &str, description: &str, properties: serde_json::Value) -> ToolSpec {
  ToolSpec {
    name: name.to_string(),
    description: description.to_string(),
    input_schema: serde_json::json!({
      "type": "object",
      "properties": properties,
      "required": []
    }),
  }
}

/// The tool registry the agent can call. Schemas mirror the MCP server's
/// browser-interaction and profile tool set (mcp_server.rs `get_tools`).
fn agent_tools() -> Vec<ToolSpec> {
  vec![
    tool_schema(
      "list_profiles",
      "List all browser profiles",
      serde_json::json!({}),
    ),
    tool_schema(
      "get_profile",
      "Get details of a specific browser profile",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the profile" }
      }),
    ),
    tool_schema(
      "get_profile_status",
      "Get the running status of a profile",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the profile" }
      }),
    ),
    tool_schema(
      "list_proxies",
      "List all stored proxies",
      serde_json::json!({}),
    ),
    tool_schema(
      "list_groups",
      "List all profile groups",
      serde_json::json!({}),
    ),
    tool_schema(
      "run_profile",
      "Launch a browser profile with an optional URL (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the profile to launch" },
        "url": { "type": "string", "description": "Optional URL to open" },
        "headless": { "type": "boolean", "description": "Run headless" }
      }),
    ),
    tool_schema(
      "kill_profile",
      "Stop a running browser profile (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the profile to stop" }
      }),
    ),
    tool_schema(
      "navigate",
      "Navigate a running browser profile to a URL (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "url": { "type": "string", "description": "The URL to navigate to" }
      }),
    ),
    tool_schema(
      "screenshot",
      "Take a screenshot of the current page in a running browser profile. Returns base64-encoded image.",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" }
      }),
    ),
    tool_schema(
      "evaluate_javascript",
      "Execute JavaScript in the context of the current page (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "expression": { "type": "string", "description": "JavaScript expression to evaluate" }
      }),
    ),
    tool_schema(
      "click_element",
      "Click on an element identified by a CSS selector (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "selector": { "type": "string", "description": "CSS selector for the element" }
      }),
    ),
    tool_schema(
      "type_text",
      "Focus an element by CSS selector and type text into it (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "selector": { "type": "string", "description": "CSS selector for the input" },
        "text": { "type": "string", "description": "Text to type" }
      }),
    ),
    tool_schema(
      "get_page_content",
      "Get the content of the current page (html or visible text)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "format": { "type": "string", "enum": ["html", "text"], "description": "html or text" },
        "selector": { "type": "string", "description": "Optional CSS selector" }
      }),
    ),
    tool_schema(
      "get_page_info",
      "Get metadata about the current page including URL, title, and readiness state",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" }
      }),
    ),
    tool_schema(
      "get_interactive_elements",
      "Enumerate visible interactive elements on the page as a compact indexed list",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" }
      }),
    ),
    tool_schema(
      "click_by_index",
      "Click the element at the given index from get_interactive_elements (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "index": { "type": "integer", "description": "Zero-based element index" }
      }),
    ),
    tool_schema(
      "type_by_index",
      "Type text into the element at the given index from get_interactive_elements (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "index": { "type": "integer", "description": "Zero-based element index" },
        "text": { "type": "string", "description": "Text to type" }
      }),
    ),
    tool_schema(
      "update_profile",
      "Update profile settings such as name or fingerprint properties (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the profile" }
      }),
    ),
  ]
}

fn system_prompt() -> String {
  let tools = agent_tools();
  let registry: Vec<serde_json::Value> = tools
    .iter()
    .map(|t| {
      serde_json::json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.input_schema
      })
    })
    .collect();
  format!(
    "You are Duckling Agent, an assistant that controls the Duckling Browser desktop application on behalf of the user.\n\n\
     You can use these tools. The `input_schema` documents the JSON arguments for each tool:\n\n\
     {}\n\n\
     PROTOCOL\n\
     - When you want to inspect something (list profiles, read pages, take screenshots), emit a single JSON object on its own line: {{\"tool\": \"<name>\", \"args\": {{...}}}}. The result will be fed back to you.\n\
     - Actions that change the browser state (navigate, click, type, run/stop profiles, evaluate JavaScript, update profiles) will NOT be executed immediately. They are recorded as change requests the user must confirm. You will receive the card id of each recorded action.\n\
     - When you have finished, emit: {{\"reply\": \"<your final answer to the user>\"}}. The reply should be concise and state exactly which changes are waiting for confirmation.\n\
     - Never invent tool results. Only report what you observe.\n\
     - Prefer get_interactive_elements + click_by_index/type_by_index over guessing CSS selectors.",
    serde_json::to_string_pretty(&registry).unwrap_or_default()
  )
}

/// Try to parse a JSON object from model output: an isolated JSON block first,
/// then the whole trimmed text, then the first JSON-encoded substring.
fn parse_model_json(content: &str) -> Option<serde_json::Value> {
  let trimmed = content.trim();
  if let Some(start) = trimmed.find("```json") {
    let rest = &trimmed[start + 7..];
    if let Some(end) = rest.find("```") {
      if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest[..end].trim()) {
        return Some(v);
      }
    }
  }
  if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
    return Some(v);
  }
  let start = trimmed.find('{')?;
  let end = trimmed.rfind('}')?;
  if start < end {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end]) {
      return Some(v);
    }
  }
  None
}

/// Extract the `content` array text from an MCP-style tool result.
fn tool_result_text(result: &serde_json::Value) -> String {
  if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
    let text = content
      .iter()
      .filter_map(|c| c.get("text").and_then(|v| v.as_str()))
      .collect::<Vec<_>>()
      .join("\n");
    if !text.is_empty() {
      return text;
    }
  }
  result.to_string()
}

fn record_card(tool_name: &str, args: &serde_json::Value) -> ChangeCard {
  let id = Uuid::new_v4().to_string();
  let kind = card_kind_for(tool_name);
  let reversible = matches!(tool_name, "navigate" | "run_profile" | "kill_profile");
  let card = ChangeCard {
    id: id.clone(),
    kind: kind.to_string(),
    title: card_title_for(tool_name, args),
    description: format!(
      "{} {}",
      tool_name,
      args
        .get("profile_id")
        .and_then(|v| v.as_str())
        .map(|p| format!("on profile {p}"))
        .unwrap_or_default()
    )
    .trim_end()
    .to_string(),
    diff: serde_json::json!({ "tool": tool_name, "args": args }),
    reversible,
  };
  PENDING_ACTIONS
    .lock()
    .unwrap()
    .insert(id, (tool_name.to_string(), args.clone()));
  card
}

/// Execute a tool call in the loop. Read-only tools run now; mutating tools
/// become pending ChangeCards. Returns (text_for_model, card_option).
async fn run_tool_call(
  tool_name: &str,
  args: &serde_json::Value,
) -> Result<(String, Option<ChangeCard>), String> {
  let known = agent_tools().iter().any(|t| t.name == tool_name);
  if !known {
    return Err(code_error(
      "AGENT_TOOL_UNKNOWN",
      serde_json::json!({ "tool": tool_name }),
    ));
  }
  if is_read_only_tool(tool_name) {
    let result = crate::mcp_server::McpServer::instance()
      .dispatch_tool_call(tool_name, args)
      .await
      .map_err(|e| e.to_string())?;
    Ok((tool_result_text(&result), None))
  } else {
    let card = record_card(tool_name, args);
    Ok((
      format!(
        "The action {} was recorded as change request {} and is waiting for the user's confirmation. It was NOT executed. Continue with your next step or produce your final reply.",
        card.title, card.id
      ),
      Some(card),
    ))
  }
}

pub async fn agent_chat_inner(
  key_id: Option<String>,
  model_override: Option<String>,
  message: String,
  use_agent: Option<String>,
) -> Result<AgentChatResult, String> {
  if let Some(agent_id) = use_agent {
    return delegate_to_agent(&agent_id, &message).await;
  }

  let record = match key_id {
    Some(id) => get_key(&id)?
      .ok_or_else(|| code_error("AI_KEY_NOT_FOUND", serde_json::json!({ "id": id })))?,
    None => all_records().into_iter().next().ok_or_else(no_key)?,
  };
  let model = model_override.unwrap_or_else(|| record.model.clone());
  let provider: AiProvider = record
    .provider
    .parse()
    .map_err(|_| agent_error(&format!("Unknown provider '{}'", record.provider)))?;
  let client = LlmClient {
    provider,
    api_key: record.key.clone(),
    model: model.clone(),
  };

  let mut messages = vec![ChatMessage {
    role: "system".to_string(),
    content: system_prompt(),
  }];
  messages.push(ChatMessage {
    role: "user".to_string(),
    content: message,
  });

  let mut cards: Vec<ChangeCard> = Vec::new();
  for _ in 0..MAX_TOOL_ITERATIONS {
    let raw = client
      .chat(&messages, None)
      .await
      .map_err(|e| agent_error(&e.0))?;
    messages.push(ChatMessage {
      role: "assistant".to_string(),
      content: raw.clone(),
    });

    let Some(json) = parse_model_json(&raw) else {
      // Plain text = final answer.
      return Ok(AgentChatResult { reply: raw, cards });
    };

    if let (Some(tool), Some(args)) = (
      json.get("tool").and_then(|v| v.as_str()),
      json.get("args").cloned(),
    ) {
      let (text, card) = run_tool_call(tool, &args).await?;
      if let Some(card) = card {
        cards.push(card);
      }
      messages.push(ChatMessage {
        role: "user".to_string(),
        content: text,
      });
      continue;
    }

    if let Some(reply) = json.get("reply").and_then(|v| v.as_str()) {
      return Ok(AgentChatResult {
        reply: reply.to_string(),
        cards,
      });
    }

    // JSON we cannot interpret — feed it back and ask the model to continue.
    messages.push(ChatMessage {
      role: "user".to_string(),
      content: format!(
        "Your previous response was not a valid tool call or final reply: {raw}\nEmit {{\"tool\": ...}} or {{\"reply\": ...}}."
      ),
    });
  }

  Ok(AgentChatResult {
    reply:
      "Stopped after the maximum number of tool steps. Confirm or decline the pending changes."
        .to_string(),
    cards,
  })
}

/// CLI flags used to run each delegated agent non-interactively.
fn delegate_flags(agent_id: &str) -> Option<Vec<String>> {
  match agent_id {
    "claude-code" | "gemini-cli" | "cline-cli" | "github-copilot-cli" => {
      Some(vec!["-p".to_string()])
    }
    "goose" | "codex" | "opencode" => Some(vec!["run".to_string()]),
    _ => None,
  }
}

async fn delegate_to_agent(agent_id: &str, prompt: &str) -> Result<AgentChatResult, String> {
  let flags = delegate_flags(agent_id).ok_or_else(|| delegate_not_found(agent_id))?;
  let full_prompt = format!(
    "{prompt}\n\nRespond in JSON with the shape {{\"reply\": \"<your answer>\", \"cards\": []}}.",
  );
  let output = tokio::time::timeout(
    DELEGATE_TIMEOUT,
    tokio::process::Command::new(agent_id)
      .args(&flags)
      .arg(&full_prompt)
      .output(),
  )
  .await
  .map_err(|_| agent_error(&format!("{agent_id} timed out")))?
  .map_err(|e| agent_error(&format!("Failed to launch {agent_id}: {e}")))?;

  let stdout = String::from_utf8_lossy(&output.stdout).to_string();
  let _ = output.status;
  if stdout.trim().is_empty() {
    return Ok(AgentChatResult {
      reply: format!("{agent_id} produced no output."),
      cards: Vec::new(),
    });
  }

  if let Some(json) = parse_model_json(&stdout) {
    if let Some(reply) = json.get("reply").and_then(|v| v.as_str()) {
      return Ok(AgentChatResult {
        reply: reply.to_string(),
        cards: Vec::new(),
      });
    }
  }

  Ok(AgentChatResult {
    reply: stdout.chars().take(4000).collect(),
    cards: Vec::new(),
  })
}

#[tauri::command]
pub async fn agent_chat(
  key_id: Option<String>,
  model: Option<String>,
  message: String,
  use_agent: Option<String>,
) -> Result<AgentChatResult, String> {
  agent_chat_inner(key_id, model, message, use_agent).await
}

#[tauri::command]
pub async fn agent_chat_confirm(card_ids: Vec<String>) -> Result<serde_json::Value, String> {
  let mut applied = Vec::new();
  let mut errors = Vec::new();
  for id in &card_ids {
    let action = PENDING_ACTIONS.lock().unwrap().remove(id);
    let Some((tool_name, args)) = action else {
      errors.push(serde_json::json!({ "id": id, "error": card_not_found(id) }));
      continue;
    };
    match crate::mcp_server::McpServer::instance()
      .dispatch_tool_call(&tool_name, &args)
      .await
    {
      Ok(result) => applied.push(serde_json::json!({
        "id": id,
        "tool": tool_name,
        "result": tool_result_text(&result)
      })),
      Err(e) => errors.push(serde_json::json!({
        "id": id,
        "error": e.to_string()
      })),
    }
  }
  Ok(serde_json::json!({ "applied": applied, "errors": errors }))
}

#[tauri::command]
pub fn agent_chat_decline(card_ids: Vec<String>) -> Result<serde_json::Value, String> {
  let mut pending = PENDING_ACTIONS.lock().unwrap();
  for id in &card_ids {
    pending.remove(id);
  }
  Ok(serde_json::json!({ "declined": card_ids }))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_model_json_handles_plain_json_and_fenced_blocks() {
    let v = parse_model_json(r#"{"tool":"navigate","args":{"url":"https://x"}}"#).unwrap();
    assert_eq!(v["tool"], "navigate");
    let v = parse_model_json("Here is my call:\n```json\n{\"reply\":\"done\"}\n```\n").unwrap();
    assert_eq!(v["reply"], "done");
    let v = parse_model_json("text before { \"reply\": \"ok\" } text after").unwrap();
    assert_eq!(v["reply"], "ok");
    assert!(parse_model_json("no json here").is_none());
  }

  #[test]
  fn read_only_classification() {
    assert!(is_read_only_tool("list_profiles"));
    assert!(is_read_only_tool("get_page_content"));
    assert!(is_read_only_tool("screenshot"));
    assert!(!is_read_only_tool("navigate"));
    assert!(!is_read_only_tool("type_text"));
    assert!(!is_read_only_tool("run_profile"));
  }

  #[test]
  fn card_records_pending_action_and_kinds() {
    PENDING_ACTIONS.lock().unwrap().clear();
    let card = record_card(
      "navigate",
      &serde_json::json!({ "profile_id": "p1", "url": "https://example.com" }),
    );
    assert_eq!(card.kind, "navigate");
    assert!(card.reversible);
    assert!(card.title.contains("https://example.com"));
    assert!(PENDING_ACTIONS.lock().unwrap().contains_key(&card.id));

    let card = record_card(
      "evaluate_javascript",
      &serde_json::json!({ "profile_id": "p1" }),
    );
    assert_eq!(card.kind, "custom");
    assert!(!card.reversible);
  }

  #[test]
  fn confirm_unknown_card_returns_error_shape() {
    PENDING_ACTIONS.lock().unwrap().clear();
    let result = agent_chat_decline(vec!["missing".to_string()]).unwrap();
    assert_eq!(result["declined"][0], "missing");
  }

  #[test]
  fn tool_result_text_extracts_content_array() {
    let result = serde_json::json!({
      "content": [{ "type": "text", "text": "hello" }, { "type": "text", "text": " world" }]
    });
    assert_eq!(tool_result_text(&result), "hello\n world");
    let result = serde_json::json!({ "plain": true });
    assert_eq!(tool_result_text(&result), "{\"plain\":true}");
  }
}
