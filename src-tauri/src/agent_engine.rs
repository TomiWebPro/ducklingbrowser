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
      | "list_proxy_pools"
      | "get_dns_blocklist_status"
      | "get_profile_fingerprint"
      | "screenshot"
      | "get_page_content"
      | "get_page_info"
      | "get_interactive_elements"
      | "get_downloads"
      | "llm_completion"
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
    "update_proxy" | "create_proxy" | "delete_proxy" | "import_proxies" | "import_vpn"
    | "create_proxy_pool" | "update_proxy_pool" | "delete_proxy_pool" => "proxy",
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
    "drag" => "Drag and drop".to_string(),
    "scroll" => "Scroll the page".to_string(),
    "press_key" => "Press a key".to_string(),
    "hover" => "Hover over an element".to_string(),
    "set_download_dir" | "wait_for_download" => format!("Download step ({})", tool_name),
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
      "drag",
      "Drag from a source element to a target element or viewport point (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "from_selector": { "type": "string", "description": "CSS selector for the drag source" },
        "from_index": { "type": "integer", "description": "Index of the drag source from get_interactive_elements" },
        "to_selector": { "type": "string", "description": "CSS selector for the drop target" },
        "to_index": { "type": "integer", "description": "Index of the drop target from get_interactive_elements" },
        "to_x": { "type": "number", "description": "Viewport x (use with to_y instead of a target)" },
        "to_y": { "type": "number", "description": "Viewport y (use with to_x instead of a target)" }
      }),
    ),
    tool_schema(
      "scroll",
      "Scroll the page or an element (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "selector": { "type": "string", "description": "CSS selector of the scrollable element (omit for the page)" },
        "index": { "type": "integer", "description": "Index from get_interactive_elements" },
        "direction": { "type": "string", "description": "up, down, left, or right (default down)" },
        "pixels": { "type": "integer", "description": "Pixels to scroll (default 500)" }
      }),
    ),
    tool_schema(
      "press_key",
      "Press a non-text key such as Enter, Tab, or Escape (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "key": { "type": "string", "description": "Key to press" }
      }),
    ),
    tool_schema(
      "hover",
      "Hover over an element to reveal menus or tooltips (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "selector": { "type": "string", "description": "CSS selector for the element" },
        "index": { "type": "integer", "description": "Index from get_interactive_elements" }
      }),
    ),
    tool_schema(
      "set_download_dir",
      "Route subsequent downloads into a sandboxed folder (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "path": { "type": "string", "description": "Download folder (omit for the profile default)" }
      }),
    ),
    tool_schema(
      "wait_for_download",
      "Wait for new files to finish downloading and return them (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the running profile" },
        "timeout_ms": { "type": "integer", "description": "Wait budget in milliseconds (default 30000)" }
      }),
    ),
    tool_schema(
      "get_downloads",
      "List finished files in the profile's download folder",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the profile" }
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
    client: None,
    endpoint_override: record.endpoint.clone(),
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
      .map_err(|e| agent_error(&e.message))?;
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

pub(crate) async fn delegate_to_agent(
  agent_id: &str,
  prompt: &str,
) -> Result<AgentChatResult, String> {
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

/// Parameters for an unattended `agent_browser` cron run: the agent drives a
/// real profile with direct tool execution (no ChangeCards) limited to an
/// explicit allowlist. Every step is recorded into the returned extraction
/// map for run history.
pub struct AgentBrowserParams {
  pub profile_id: String,
  pub key_id: Option<String>,
  pub model: Option<String>,
  pub prompt: String,
  pub allowed_tools: Vec<String>,
  pub download_dir_override: Option<String>,
  pub max_steps: u32,
}

fn agent_browser_system_prompt(allowed: &[String], profile_id: &str) -> String {
  let tools: Vec<serde_json::Value> = agent_tools()
    .into_iter()
    .filter(|t| allowed.iter().any(|a| a == &t.name))
    .map(|t| {
      serde_json::json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.input_schema
      })
    })
    .collect();
  format!(
    "You are Duckling Agent running UNATTENDED inside a scheduled task for browser profile {profile_id}.\n\n\
    You can use ONLY these tools (any other tool name is denied and fails the run):\n\n\
    {}\n\n\
    PROTOCOL\n\
    - Always include \"profile_id\": \"{profile_id}\" in every tool's args.\n\
    - Act step by step: emit a single JSON object on its own line per step: {{\"tool\": \"<name>\", \"args\": {{...}}}}. Each result is fed back to you.\n\
    - Prefer get_interactive_elements + click_by_index/type_by_index over guessing CSS selectors.\n\
    - Downloads go to the profile's sandboxed folder; use set_download_dir first when the task needs a subfolder, then wait_for_download.\n\
    - When finished, emit: {{\"reply\": \"<concise summary of what was done>\"}}.\n\
    - Never invent tool results. Only report what you observe.",
    serde_json::to_string_pretty(&tools).unwrap_or_default()
  )
}

/// Execute an unattended profile+AI run. Returns extraction entries:
/// `reply`, `steps` (human-readable log), `downloads` (final folder listing).
pub async fn agent_browser_run(
  params: AgentBrowserParams,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>, String> {
  if params.profile_id.trim().is_empty() {
    return Err("Task has no target profile".to_string());
  }
  if params.prompt.trim().is_empty() {
    return Err("Task has no prompt".to_string());
  }
  let record = match &params.key_id {
    Some(id) => {
      get_key(id)?.ok_or_else(|| code_error("AI_KEY_NOT_FOUND", serde_json::json!({ "id": id })))?
    }
    None => all_records().into_iter().next().ok_or_else(no_key)?,
  };
  let model = params.model.clone().unwrap_or_else(|| record.model.clone());
  let provider: AiProvider = record
    .provider
    .parse()
    .map_err(|_| agent_error(&format!("Unknown provider '{}'", record.provider)))?;
  let client = LlmClient {
    provider,
    api_key: record.key.clone(),
    model: model.clone(),
    client: None,
    endpoint_override: record.endpoint.clone(),
  };

  // Pre-apply the task download folder when the profile is already running.
  // (Tool-level set_download_dir calls apply on demand during the run.)
  if let Some(override_dir) = params.download_dir_override.as_deref() {
    if let Ok(profile) = running_profile(&params.profile_id) {
      if let Ok(dir) = crate::browser_downloads::resolve_download_dir(&profile, Some(override_dir))
      {
        if let Ok(session) = cdp_ws_for_profile(&profile).await {
          let _ = crate::cdp_session::CdpSession::new()
            .set_download_behavior(&session, &dir)
            .await;
        }
      }
    }
  }

  let mut messages = vec![ChatMessage {
    role: "system".to_string(),
    content: agent_browser_system_prompt(&params.allowed_tools, &params.profile_id),
  }];
  messages.push(ChatMessage {
    role: "user".to_string(),
    content: params.prompt.clone(),
  });

  let mut step_log: Vec<String> = Vec::new();
  let max_steps = params.max_steps.clamp(1, 100);
  for step in 0..max_steps {
    let raw = client
      .chat(&messages, None)
      .await
      .map_err(|e| agent_error(&e.message))?;
    messages.push(ChatMessage {
      role: "assistant".to_string(),
      content: raw.clone(),
    });
    let Some(json) = parse_model_json(&raw) else {
      step_log.push(format!("step {}: plain-text reply", step + 1));
      return finish_browser_run(&params, &raw, step_log);
    };
    if let Some(reply) = json.get("reply").and_then(|v| v.as_str()) {
      return finish_browser_run(&params, reply, step_log);
    }
    let (Some(tool), Some(mut args)) = (
      json
        .get("tool")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()),
      json.get("args").cloned(),
    ) else {
      messages.push(ChatMessage {
        role: "user".to_string(),
        content: format!(
          "Your previous response was not a valid tool call or final reply: {raw}\nEmit {{\"tool\": ...}} or {{\"reply\": ...}}."
        ),
      });
      continue;
    };
    if !params.allowed_tools.iter().any(|a| a == &tool) {
      return Err(format!(
        "Denied: tool '{tool}' is not in this task's allowlist. Failing the run instead of acting."
      ));
    }
    // Force the task profile on every call so the model cannot wander to a
    // different profile's browser.
    if let Some(obj) = args.as_object_mut() {
      obj.insert(
        "profile_id".to_string(),
        serde_json::Value::String(params.profile_id.clone()),
      );
    }
    // Inject the task download folder into bare set_download_dir calls.
    if tool == "set_download_dir"
      && params.download_dir_override.is_some()
      && args
        .get("path")
        .and_then(|v| v.as_str())
        .is_none_or(|s| s.trim().is_empty())
    {
      if let Some(obj) = args.as_object_mut() {
        obj.insert(
          "path".to_string(),
          serde_json::Value::String(params.download_dir_override.clone().unwrap_or_default()),
        );
      }
    }
    match crate::mcp_server::McpServer::instance()
      .dispatch_tool_call(&tool, &args)
      .await
    {
      Ok(result) => {
        let text = tool_result_text(&result);
        step_log.push(format!("step {}: {tool} ok", step + 1));
        messages.push(ChatMessage {
          role: "user".to_string(),
          content: format!("Tool {tool} result: {text}"),
        });
      }
      Err(e) => {
        step_log.push(format!("step {}: {tool} failed: {e}", step + 1));
        messages.push(ChatMessage {
          role: "user".to_string(),
          content: format!("Tool {tool} failed: {e}. Adjust and retry a different approach or finish with a reply."),
        });
      }
    }
  }

  finish_browser_run(
    &params,
    "Stopped after the maximum number of steps.",
    step_log,
  )
}

fn running_profile(profile_id: &str) -> Result<crate::profile::BrowserProfile, String> {
  let profiles = crate::profile::ProfileManager::instance()
    .list_profiles()
    .map_err(|e| format!("Failed to list profiles: {e}"))?;
  let profile = profiles
    .into_iter()
    .find(|p| p.id.to_string() == profile_id)
    .ok_or_else(|| format!("Profile '{profile_id}' not found"))?;
  if profile.process_id.is_none() {
    return Err(format!("Profile '{}' is not running", profile.name));
  }
  Ok(profile)
}

async fn cdp_ws_for_profile(profile: &crate::profile::BrowserProfile) -> Result<String, String> {
  let session = crate::cdp_session::CdpSession::new();
  let port = session
    .get_cdp_port_for_profile(profile)
    .await
    .map_err(|e| e.message)?;
  session.get_cdp_ws_url(port).await.map_err(|e| e.message)
}

fn finish_browser_run(
  params: &AgentBrowserParams,
  reply: &str,
  step_log: Vec<String>,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>, String> {
  let mut extracted = std::collections::BTreeMap::new();
  extracted.insert(
    "reply".to_string(),
    serde_json::Value::String(reply.to_string()),
  );
  extracted.insert(
    "steps".to_string(),
    serde_json::Value::Array(
      step_log
        .into_iter()
        .map(serde_json::Value::String)
        .collect(),
    ),
  );
  // Final folder listing so run history shows what was downloaded.
  if let Ok(profiles) = crate::profile::ProfileManager::instance().list_profiles() {
    if let Some(profile) = profiles
      .into_iter()
      .find(|p| p.id.to_string() == params.profile_id)
    {
      if let Ok(dir) = crate::browser_downloads::resolve_download_dir(
        &profile,
        params.download_dir_override.as_deref(),
      ) {
        let files = crate::browser_downloads::list_downloads(&dir);
        extracted.insert(
          "downloads".to_string(),
          serde_json::to_value(&files).unwrap_or(serde_json::Value::Null),
        );
      }
    }
  }
  Ok(extracted)
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
    assert!(is_read_only_tool("get_downloads"));
    assert!(!is_read_only_tool("navigate"));
    assert!(!is_read_only_tool("type_text"));
    assert!(!is_read_only_tool("run_profile"));
    assert!(!is_read_only_tool("drag"));
    assert!(!is_read_only_tool("scroll"));
    assert!(!is_read_only_tool("press_key"));
    assert!(!is_read_only_tool("hover"));
    assert!(!is_read_only_tool("set_download_dir"));
    assert!(!is_read_only_tool("wait_for_download"));
  }

  #[test]
  fn every_agent_tool_is_registered_and_unique() {
    let tools = agent_tools();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), tools.len());
    for required in [
      "drag",
      "scroll",
      "press_key",
      "hover",
      "set_download_dir",
      "wait_for_download",
      "get_downloads",
      "click_by_index",
      "type_by_index",
      "get_interactive_elements",
    ] {
      assert!(
        tools.iter().any(|t| t.name == required),
        "missing agent tool {required}"
      );
    }
  }

  #[test]
  fn interaction_cards_carry_titles() {
    // Remove exactly the ids this test records: the pending-action map is
    // process-global and other tests run on sibling threads, so a blanket
    // clear() here would flake them.
    let mut ids = Vec::new();
    for tool in ["drag", "scroll", "press_key", "hover"] {
      let card = record_card(tool, &serde_json::json!({ "profile_id": "p1" }));
      assert_eq!(card.kind, "custom");
      assert!(!card.title.is_empty());
      ids.push(card.id);
    }
    let mut pending = PENDING_ACTIONS.lock().unwrap();
    for id in ids {
      pending.remove(&id);
    }
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
  fn agent_browser_run_rejects_empty_params_without_network() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
      .block_on(agent_browser_run(AgentBrowserParams {
        profile_id: String::new(),
        key_id: None,
        model: None,
        prompt: "hi".to_string(),
        allowed_tools: Vec::new(),
        download_dir_override: None,
        max_steps: 5,
      }))
      .unwrap_err();
    assert!(err.contains("target profile"));

    let err = runtime
      .block_on(agent_browser_run(AgentBrowserParams {
        profile_id: "p1".to_string(),
        key_id: None,
        model: None,
        prompt: "   ".to_string(),
        allowed_tools: Vec::new(),
        download_dir_override: None,
        max_steps: 5,
      }))
      .unwrap_err();
    assert!(err.contains("prompt"));
  }

  #[test]
  fn agent_browser_run_needs_a_key() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::app_dirs::set_test_data_dir(tmp.path().to_path_buf());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
      .block_on(agent_browser_run(AgentBrowserParams {
        profile_id: "p1".to_string(),
        key_id: None,
        model: None,
        prompt: "Do it".to_string(),
        allowed_tools: vec!["navigate".to_string()],
        download_dir_override: None,
        max_steps: 5,
      }))
      .unwrap_err();
    assert!(err.contains("AGENT_NO_KEY"));
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
