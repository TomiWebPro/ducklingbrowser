use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{
  atomic::{AtomicBool, Ordering},
  LazyLock, Mutex,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ai_keys::{all_records, get_key, AiProvider};
use crate::llm::{ChatMessage, ChatUsage, LlmClient, ToolSpec};

const MAX_TOOL_ITERATIONS: usize = 20;
const DELEGATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Cap on inline screenshot bytes forwarded to the model per turn.
const MAX_INLINE_IMAGE_CHARS: usize = 280_000;

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
  /// Aggregated token usage across all LLM turns (None when unreported).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub usage: Option<ChatUsage>,
  /// Number of tool-calling iterations consumed.
  #[serde(default)]
  pub steps_used: u32,
}

/// Per-run guardrails for unattended browsing. All fields are advisory:
/// violations fail the run with a clear error instead of acting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentGuardrails {
  /// When non-empty, `navigate` targets must host-match one entry.
  #[serde(default)]
  pub allowed_domains: Vec<String>,
  /// CSS selectors the agent must never click/type (e.g. logout buttons).
  #[serde(default)]
  pub forbidden_selectors: Vec<String>,
}

fn host_of(url: &str) -> Option<String> {
  let after_scheme = url.split("://").nth(1).unwrap_or(url);
  let host = after_scheme
    .split('/')
    .next()
    .unwrap_or(after_scheme)
    .split('@')
    .next_back()
    .unwrap_or(after_scheme);
  let host = host.split(':').next().unwrap_or(host).to_lowercase();
  if host.is_empty() {
    None
  } else {
    Some(host)
  }
}

/// True when `url` is permitted under `allowed` (empty allowlist = allow all).
/// Entries match the host itself or any subdomain (`example.com` covers
/// `app.example.com`).
pub fn url_allowed(url: &str, allowed: &[String]) -> bool {
  if allowed.is_empty() {
    return true;
  }
  let Some(host) = host_of(url) else {
    return false;
  };
  allowed.iter().any(|entry| {
    let entry = entry.trim().to_lowercase();
    let entry = entry
      .strip_prefix("https://")
      .or_else(|| entry.strip_prefix("http://"))
      .unwrap_or(&entry);
    let entry = entry.split('/').next().unwrap_or(entry);
    host == entry || host.ends_with(&format!(".{entry}"))
  })
}

fn selector_of(args: &serde_json::Value) -> Option<&str> {
  args
    .get("selector")
    .and_then(|v| v.as_str())
    .or_else(|| args.get("from_selector").and_then(|v| v.as_str()))
    .or_else(|| args.get("to_selector").and_then(|v| v.as_str()))
}

/// Refuse tool calls that target a forbidden selector. Returns the offending
/// selector when blocked.
pub fn forbidden_selector_hit(args: &serde_json::Value, forbidden: &[String]) -> Option<String> {
  if forbidden.is_empty() {
    return None;
  }
  let selector = selector_of(args)?;
  forbidden
    .iter()
    .find(|entry| selector.contains(entry.as_str()))
    .cloned()
}

/// Redact likely secret values from log-bound text so passwords and tokens
/// pasted into prompts never land in run history. Matches `"password": "…"`
/// style pairs and bare `sk-…` tokens.
pub fn redact_secrets(text: &str) -> String {
  let mut out = text.to_string();
  for key in [
    "password", "passwd", "totp", "otp", "api_key", "apikey", "token", "secret",
  ] {
    let mut search = format!("\"{key}\"");
    let mut start = 0;
    while let Some(hit) = out[start..].find(search.as_str()) {
      let key_pos = start + hit;
      let after_key = &out[key_pos + search.len()..];
      let Some(colon) = after_key.find(':') else {
        break;
      };
      let mut val_start = key_pos + search.len() + colon + 1;
      while out[val_start..].starts_with([' ', '\t']) {
        val_start += 1;
      }
      if !out[val_start..].starts_with('"') {
        start = val_start;
        search = format!("\"{key}\"");
        continue;
      }
      val_start += 1;
      let mut end = val_start;
      let bytes = out.as_bytes();
      while end < out.len() && bytes[end] != b'"' {
        end += if bytes[end] == b'\\' { 2 } else { 1 }.min(out.len() - end);
      }
      out.replace_range(val_start..end.min(out.len()), "***");
      start = val_start + 3;
      search = format!("\"{key}\"");
    }
  }
  out
}

/// Cancellation flags plus a live snapshot for in-flight agent runs, so the
/// UI can show active jobs and offer Stop. Keyed by caller-supplied run id
/// (chat runs use an ephemeral id; scheduled runs use the task id).
static AGENT_RUNS: LazyLock<Mutex<HashMap<String, AgentRunState>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone)]
struct AgentRunState {
  cancel: Arc<AtomicBool>,
  label: String,
  started_ms: u64,
  step: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveAgentRun {
  pub run_id: String,
  pub label: String,
  pub step: String,
  pub elapsed_ms: u64,
}

fn now_ms() -> u64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_millis() as u64)
    .unwrap_or(0)
}

fn register_agent_run(run_id: &str, label: String) -> Arc<AtomicBool> {
  let flag = Arc::new(AtomicBool::new(false));
  AGENT_RUNS.lock().unwrap().insert(
    run_id.to_string(),
    AgentRunState {
      cancel: flag.clone(),
      label,
      started_ms: now_ms(),
      step: "starting".to_string(),
    },
  );
  flag
}

fn update_run_step(run_id: &str, step: String) {
  if let Some(state) = AGENT_RUNS.lock().unwrap().get_mut(run_id) {
    state.step = step;
  }
}

fn finish_agent_run(run_id: &str) {
  AGENT_RUNS.lock().unwrap().remove(run_id);
}

/// Request cancellation of an in-flight run. Returns true when a live run
/// was flagged.
pub fn cancel_agent_run(run_id: &str) -> bool {
  AGENT_RUNS
    .lock()
    .unwrap()
    .get(run_id)
    .map(|state| {
      state.cancel.store(true, Ordering::SeqCst);
      true
    })
    .unwrap_or(false)
}

fn run_cancelled(flag: &AtomicBool) -> bool {
  flag.load(Ordering::SeqCst)
}

/// Snapshot of currently executing agent runs for the active-jobs UI.
pub fn active_agent_runs() -> Vec<ActiveAgentRun> {
  let now = now_ms();
  let mut runs: Vec<ActiveAgentRun> = AGENT_RUNS
    .lock()
    .unwrap()
    .iter()
    .map(|(run_id, state)| ActiveAgentRun {
      run_id: run_id.clone(),
      label: state.label.clone(),
      step: state.step.clone(),
      elapsed_ms: now.saturating_sub(state.started_ms),
    })
    .collect();
  runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
  runs
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
/// Browser-interaction flags come from the shared catalog; only the
/// agent-local config tools are listed here.
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
      | "llm_completion"
  ) || crate::browser_tools::is_read_only_browser_tool(name)
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
    "new_tab" => "Open a new tab".to_string(),
    "switch_tab" | "close_tab" => format!("Tab action ({})", tool_name),
    "select_option" => "Select a dropdown option".to_string(),
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

/// The tool registry the agent can call: profile/config tools plus every
/// browser-interaction tool from the shared catalog
/// (`browser_tools::browser_tools`, the same list the MCP server exposes).
fn agent_tools() -> Vec<ToolSpec> {
  let mut tools = vec![
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
      "update_profile",
      "Update profile settings such as name or fingerprint properties (requires confirmation)",
      serde_json::json!({
        "profile_id": { "type": "string", "description": "The UUID of the profile" }
      }),
    ),
  ];
  tools.extend(
    crate::browser_tools::browser_tools()
      .into_iter()
      .map(|t| ToolSpec {
        name: t.name,
        description: t.description,
        input_schema: t.input_schema,
      }),
  );
  tools
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
     - Call tools natively via function calling when available. Otherwise, when you want to inspect something (list profiles, read pages, take screenshots), emit a single JSON object on its own line: {{\"tool\": \"<name>\", \"args\": {{...}}}}. The result will be fed back to you.\n\
     - Actions that change the browser state (navigate, click, type, run/stop profiles, evaluate JavaScript, update profiles) will NOT be executed immediately. They are recorded as change requests the user must confirm. You will receive the card id of each recorded action.\n\
     - When you have finished, emit: {{\"reply\": \"<your final answer to the user>\"}}. The reply should be concise and state exactly which changes are waiting for confirmation.\n\
     - Never invent tool results. Only report what you observe.\n\
     - Prefer get_interactive_elements + click_by_index/type_by_index over guessing CSS selectors.",
    serde_json::to_string_pretty(&registry).unwrap_or_default()
  )
}

/// System prompt variant for full-automation chat: tool calls execute
/// immediately, so the model must act and then summarize what it DID.
fn system_prompt_full_auto() -> String {
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
     FULL AUTOMATION IS ON: every tool call you make executes immediately. There is no confirmation step.\n\n\
     You can use these tools. The `input_schema` documents the JSON arguments for each tool:\n\n\
     {}\n\n\
     PROTOCOL\n\
     - Call tools natively via function calling when available. Otherwise, emit a single JSON object on its own line: {{\"tool\": \"<name>\", \"args\": {{...}}}}. Each result is fed back to you.\n\
     - Act step by step, then finish with: {{\"reply\": \"<concise summary of what you DID>\"}}. State exactly which actions ran.\n\
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

/// Dispatch a known agent tool through the MCP server. Shared by the
/// interactive card flow and unattended runs so both execute the exact
/// same tool call.
async fn dispatch_agent_tool(
  tool_name: &str,
  args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
  ensure_known_tool(tool_name)?;
  crate::mcp_server::McpServer::instance()
    .dispatch_tool_call(tool_name, args)
    .await
    .map_err(|e| e.to_string())
}

fn ensure_known_tool(tool_name: &str) -> Result<(), String> {
  if agent_tools().iter().any(|t| t.name == tool_name) {
    Ok(())
  } else {
    Err(code_error(
      "AGENT_TOOL_UNKNOWN",
      serde_json::json!({ "tool": tool_name }),
    ))
  }
}

/// Full-automation variant: mutating tools dispatch immediately instead of
/// becoming cards. The caller turned full automation on, which IS the approval.
async fn run_tool_call_with_mode(
  tool_name: &str,
  args: &serde_json::Value,
  auto_approve: bool,
) -> Result<(String, Option<ChangeCard>), String> {
  ensure_known_tool(tool_name)?;
  if auto_approve || is_read_only_tool(tool_name) {
    let result = dispatch_agent_tool(tool_name, args).await?;
    let text = tool_result_text(&result);
    if auto_approve && !is_read_only_tool(tool_name) {
      return Ok((
        format!("Executed {tool_name} immediately (full automation is on). Result: {text}"),
        None,
      ));
    }
    Ok((text, None))
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

pub async fn agent_chat_inner_with_run(
  key_id: Option<String>,
  model_override: Option<String>,
  message: String,
  use_agent: Option<String>,
  run_id: Option<String>,
  auto_approve: bool,
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

  let run_id = run_id.unwrap_or_else(|| format!("chat-{}", Uuid::new_v4()));
  let cancel = register_agent_run(&run_id, message.chars().take(80).collect());
  let done = |reply: String, cards: Vec<ChangeCard>, usage: Option<ChatUsage>, steps: u32| {
    finish_agent_run(&run_id);
    Ok::<AgentChatResult, String>(AgentChatResult {
      reply,
      cards,
      usage,
      steps_used: steps,
    })
  };

  let tools = agent_tools();
  let system = if auto_approve {
    system_prompt_full_auto()
  } else {
    system_prompt()
  };
  let mut messages = vec![ChatMessage::text("system", system)];
  messages.push(ChatMessage::text("user", message.clone()));

  let mut cards: Vec<ChangeCard> = Vec::new();
  let mut usage: Option<ChatUsage> = None;
  let mut steps_used: u32 = 0;
  for _ in 0..MAX_TOOL_ITERATIONS {
    if run_cancelled(&cancel) {
      return done(
        "Cancelled by the user.".to_string(),
        cards,
        usage,
        steps_used,
      );
    }
    steps_used += 1;
    update_run_step(&run_id, format!("thinking (step {steps_used})"));
    let response = client.send(&messages, Some(&tools)).await.map_err(|e| {
      finish_agent_run(&run_id);
      agent_error(&e.message)
    })?;
    if let Some(u) = response.usage {
      usage = Some(match usage.take() {
        None => u,
        Some(acc) => ChatUsage {
          prompt_tokens: acc.prompt_tokens.saturating_add(u.prompt_tokens),
          completion_tokens: acc.completion_tokens.saturating_add(u.completion_tokens),
          total_tokens: acc.total_tokens.saturating_add(u.total_tokens),
        },
      });
    }
    messages.push(ChatMessage::text("assistant", response.text.clone()));

    // Native function calls take precedence over the legacy JSON protocol.
    if !response.tool_calls.is_empty() {
      let mut feedback = Vec::new();
      for call in &response.tool_calls {
        update_run_step(&run_id, format!("tool {} (step {steps_used})", call.name));
        match run_tool_call_with_mode(&call.name, &call.arguments, auto_approve).await {
          Ok((text, card)) => {
            if let Some(card) = card {
              cards.push(card);
            }
            feedback.push(format!(
              "Tool {} result: {}",
              call.name,
              redact_secrets(&text)
            ));
          }
          Err(err) => {
            feedback.push(format!("Tool {} failed: {err}", call.name));
          }
        }
      }
      messages.push(ChatMessage::text("user", feedback.join("\n")));
      continue;
    }

    let raw = response.text;
    let Some(json) = parse_model_json(&raw) else {
      // Plain text = final answer.
      return done(raw, cards, usage, steps_used);
    };

    if let (Some(tool), Some(args)) = (
      json.get("tool").and_then(|v| v.as_str()),
      json.get("args").cloned(),
    ) {
      update_run_step(&run_id, format!("tool {tool} (step {steps_used})"));
      let (text, card) = run_tool_call_with_mode(tool, &args, auto_approve).await?;
      if let Some(card) = card {
        cards.push(card);
      }
      messages.push(ChatMessage::text("user", redact_secrets(&text)));
      continue;
    }

    if let Some(reply) = json.get("reply").and_then(|v| v.as_str()) {
      return done(reply.to_string(), cards, usage, steps_used);
    }

    // JSON we cannot interpret — feed it back and ask the model to continue.
    messages.push(ChatMessage::text(
      "user",
      format!(
        "Your previous response was not a valid tool call or final reply: {raw}\nEmit {{\"tool\": ...}} or {{\"reply\": ...}}."
      ),
    ));
  }

  done(
    if auto_approve {
      "Stopped after the maximum number of tool steps. Everything above already ran (full automation is on)."
        .to_string()
    } else {
      "Stopped after the maximum number of tool steps. Confirm or decline the pending changes."
        .to_string()
    },
    cards,
    usage,
    steps_used,
  )
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
      usage: None,
      steps_used: 0,
    });
  }

  if let Some(json) = parse_model_json(&stdout) {
    if let Some(reply) = json.get("reply").and_then(|v| v.as_str()) {
      return Ok(AgentChatResult {
        reply: reply.to_string(),
        cards: Vec::new(),
        usage: None,
        steps_used: 0,
      });
    }
  }

  Ok(AgentChatResult {
    reply: stdout.chars().take(4000).collect(),
    cards: Vec::new(),
    usage: None,
    steps_used: 0,
  })
}

#[tauri::command]
pub async fn agent_chat(
  key_id: Option<String>,
  model: Option<String>,
  message: String,
  use_agent: Option<String>,
  auto_approve: Option<bool>,
) -> Result<AgentChatResult, String> {
  agent_chat_inner_with_run(
    key_id,
    model,
    message,
    use_agent,
    None,
    auto_approve.unwrap_or(false),
  )
  .await
}

/// Flag an in-flight chat or scheduled agent run for cancellation. Returns
/// true when a live run was found.
#[tauri::command]
pub fn agent_cancel_run(run_id: String) -> bool {
  cancel_agent_run(&run_id)
}

/// Snapshot of currently executing agent runs for the active-jobs UI.
#[tauri::command]
pub fn agent_active_runs() -> Vec<ActiveAgentRun> {
  active_agent_runs()
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
  /// Guardrails for the run (domain allowlist, forbidden selectors).
  #[allow(dead_code)]
  pub guardrails: AgentGuardrails,
  /// Stable id used for cancellation + active-run snapshots.
  pub run_id: Option<String>,
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

  let run_id = params
    .run_id
    .clone()
    .unwrap_or_else(|| format!("task-{}-{}", params.profile_id, Uuid::new_v4()));
  let cancel = register_agent_run(&run_id, params.prompt.chars().take(80).collect());
  let finish = |reply: &str, step_log: Vec<String>| {
    finish_agent_run(&run_id);
    finish_browser_run(&params, reply, step_log)
  };

  // Native function specs for the allowlisted tools so the model can call
  // them directly; the legacy {"tool","args"} JSON protocol stays as fallback.
  let native_tools: Vec<ToolSpec> = agent_tools()
    .into_iter()
    .filter(|t| params.allowed_tools.iter().any(|a| a == &t.name))
    .collect();
  let tools_for_api: Option<&[ToolSpec]> = if native_tools.is_empty() {
    None
  } else {
    Some(&native_tools)
  };

  let mut messages = vec![ChatMessage::text(
    "system",
    agent_browser_system_prompt(&params.allowed_tools, &params.profile_id),
  )];
  messages.push(ChatMessage::text("user", params.prompt.clone()));

  let mut step_log: Vec<String> = Vec::new();
  let mut usage: Option<ChatUsage> = None;
  let max_steps = params.max_steps.clamp(1, 100);
  for step in 0..max_steps {
    if run_cancelled(&cancel) {
      step_log.push(format!("step {}: cancelled by user", step + 1));
      return finish("Cancelled by the user.", step_log);
    }
    update_run_step(&run_id, format!("thinking (step {})", step + 1));
    let response = client.send(&messages, tools_for_api).await.map_err(|e| {
      finish_agent_run(&run_id);
      agent_error(&e.message)
    })?;
    if let Some(u) = response.usage {
      usage = Some(match usage.take() {
        None => u,
        Some(acc) => ChatUsage {
          prompt_tokens: acc.prompt_tokens.saturating_add(u.prompt_tokens),
          completion_tokens: acc.completion_tokens.saturating_add(u.completion_tokens),
          total_tokens: acc.total_tokens.saturating_add(u.total_tokens),
        },
      });
    }
    messages.push(ChatMessage::text("assistant", response.text.clone()));

    // Native calls first: execute each in order, feed results back together.
    if !response.tool_calls.is_empty() {
      let mut feedback = Vec::new();
      let mut images: Vec<String> = Vec::new();
      for call in &response.tool_calls {
        match execute_browser_tool(&params, &call.name, call.arguments.clone()).await {
          Ok((text, image)) => {
            step_log.push(format!("step {}: {} ok", step + 1, call.name));
            update_run_step(&run_id, format!("{} ok (step {})", call.name, step + 1));
            feedback.push(format!(
              "Tool {} result: {}",
              call.name,
              redact_secrets(&text)
            ));
            if let Some(image) = image {
              images.push(image);
            }
          }
          Err(err) => {
            step_log.push(format!("step {}: {} failed: {err}", step + 1, call.name));
            feedback.push(format!(
              "Tool {} failed: {err}. Adjust and retry a different approach or finish with a reply.",
              call.name
            ));
          }
        }
      }
      if images.len() > 1 {
        images.truncate(1);
      }
      messages.push(if images.is_empty() {
        ChatMessage::text("user", feedback.join("\n"))
      } else {
        ChatMessage::with_images("user", feedback.join("\n"), images)
      });
      continue;
    }

    let raw = response.text;
    let Some(json) = parse_model_json(&raw) else {
      step_log.push(format!("step {}: plain-text reply", step + 1));
      return finish(&raw, step_log);
    };
    if let Some(reply) = json.get("reply").and_then(|v| v.as_str()) {
      return finish(reply, step_log);
    }
    let (Some(tool), Some(args)) = (
      json
        .get("tool")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()),
      json.get("args").cloned(),
    ) else {
      messages.push(ChatMessage::text(
        "user",
        format!(
          "Your previous response was not a valid tool call or final reply: {raw}\nEmit {{\"tool\": ...}} or {{\"reply\": ...}}."
        ),
      ));
      continue;
    };
    match execute_browser_tool(&params, &tool, args).await {
      Ok((text, image)) => {
        step_log.push(format!("step {}: {tool} ok", step + 1));
        update_run_step(&run_id, format!("{tool} ok (step {})", step + 1));
        messages.push(if let Some(image) = image {
          ChatMessage::with_images(
            "user",
            format!("Tool {tool} result: {}", redact_secrets(&text)),
            vec![image],
          )
        } else {
          ChatMessage::text(
            "user",
            format!("Tool {tool} result: {}", redact_secrets(&text)),
          )
        });
      }
      Err(err) => {
        // Allowlist / guardrail denials fail the run loudly instead of looping.
        if err.starts_with("Denied:") || err.starts_with("Blocked:") {
          finish_agent_run(&run_id);
          return Err(err);
        }
        step_log.push(format!("step {}: {tool} failed: {err}", step + 1));
        messages.push(ChatMessage::text(
          "user",
          format!("Tool {tool} failed: {err}. Adjust and retry a different approach or finish with a reply."),
        ));
      }
    }
  }

  let mut out = finish("Stopped after the maximum number of steps.", step_log)?;
  if let Some(usage) = usage {
    out.insert(
      "usage".to_string(),
      serde_json::to_value(&usage).unwrap_or(serde_json::Value::Null),
    );
  }
  Ok(out)
}

/// Enforce allowlist + guardrails, force the task profile, then dispatch.
/// Returns the text result plus an optional screenshot data URL for vision.
async fn execute_browser_tool(
  params: &AgentBrowserParams,
  tool: &str,
  mut args: serde_json::Value,
) -> Result<(String, Option<String>), String> {
  if !params.allowed_tools.iter().any(|a| a == tool) {
    return Err(format!(
      "Denied: tool '{tool}' is not in this task's allowlist. Failing the run instead of acting."
    ));
  }
  if let Some(hit) = forbidden_selector_hit(&args, &params.guardrails.forbidden_selectors) {
    return Err(format!(
      "Blocked: selector '{hit}' is forbidden for this task. Failing the run instead of acting."
    ));
  }
  if tool == "navigate" {
    if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
      if !url_allowed(url, &params.guardrails.allowed_domains) {
        return Err(format!(
          "Blocked: navigation to '{url}' is outside this task's allowed domains. Failing the run instead of acting."
        ));
      }
    }
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
  match dispatch_agent_tool(tool, &args).await {
    Ok(result) => {
      let text = tool_result_text(&result);
      let image = if tool == "screenshot" {
        screenshot_data_url(&result)
      } else {
        None
      };
      Ok((text, image))
    }
    Err(e) => Err(e),
  }
}

/// Pull a `data:<mime>;base64,…` URL out of a screenshot tool result, capped
/// so one turn cannot blow up the context window.
fn screenshot_data_url(result: &serde_json::Value) -> Option<String> {
  let content = result.get("content")?.as_array()?;
  for block in content {
    if block.get("type").and_then(|v| v.as_str()) != Some("image") {
      continue;
    }
    let data = block.get("data")?.as_str()?;
    let mime = block
      .get("mimeType")
      .and_then(|v| v.as_str())
      .unwrap_or("image/png");
    let mut url = format!("data:{mime};base64,{data}");
    url.truncate(MAX_INLINE_IMAGE_CHARS);
    return Some(url);
  }
  None
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
    match dispatch_pending_card(id).await {
      Ok(entry) => applied.push(entry),
      Err(error) => errors.push(error),
    }
  }
  Ok(serde_json::json!({ "applied": applied, "errors": errors }))
}

/// Dispatch one pending card by id. Shared by interactive confirm and
/// unattended runs so both paths execute the exact same tool call.
async fn dispatch_pending_card(id: &str) -> Result<serde_json::Value, serde_json::Value> {
  let action = PENDING_ACTIONS.lock().unwrap().remove(id);
  let Some((tool_name, args)) = action else {
    return Err(serde_json::json!({ "id": id, "error": card_not_found(id) }));
  };
  match dispatch_agent_tool(&tool_name, &args).await {
    Ok(result) => Ok(serde_json::json!({
      "id": id,
      "tool": tool_name,
      "result": tool_result_text(&result)
    })),
    Err(e) => Err(serde_json::json!({
      "id": id,
      "error": e
    })),
  }
}

/// Outcome of applying agent-proposed cards without a human present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct UnattendedApplyOutcome {
  pub applied: Vec<serde_json::Value>,
  pub skipped: Vec<serde_json::Value>,
  pub errors: Vec<serde_json::Value>,
}

/// Apply cards in an unattended run (cron / full automation). With
/// `auto_approve` every card executes — scheduling the task was the approval.
/// Without it (safe default) only `reversible` cards execute; the rest are
/// skipped and reported so the run never blocks waiting for a human.
pub async fn apply_cards_for_unattended(
  cards: &[ChangeCard],
  auto_approve: bool,
) -> UnattendedApplyOutcome {
  let mut outcome = UnattendedApplyOutcome::default();
  for card in cards {
    if !auto_approve && !card.reversible {
      outcome.skipped.push(serde_json::json!({
        "id": card.id,
        "tool": card_title_for_from_diff(card),
        "reason": "irreversible change requires full automation (auto_approve)",
      }));
      // Drop the pending action so it can never be confirmed later by
      // accident; the skip is recorded in run history.
      PENDING_ACTIONS.lock().unwrap().remove(&card.id);
      continue;
    }
    match dispatch_pending_card(&card.id).await {
      Ok(entry) => outcome.applied.push(entry),
      Err(error) => outcome.errors.push(error),
    }
  }
  outcome
}

fn card_title_for_from_diff(card: &ChangeCard) -> String {
  card
    .diff
    .get("tool")
    .and_then(|v| v.as_str())
    .unwrap_or(&card.kind)
    .to_string()
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
    assert!(is_read_only_tool("list_tabs"));
    assert!(is_read_only_tool("find_text"));
    assert!(is_read_only_tool("get_cookies"));
    assert!(is_read_only_tool("extract_table"));
    assert!(is_read_only_tool("extract_article"));
    assert!(is_read_only_tool("wait_for_text"));
    assert!(is_read_only_tool("wait_for_url"));
    assert!(!is_read_only_tool("navigate"));
    assert!(!is_read_only_tool("type_text"));
    assert!(!is_read_only_tool("run_profile"));
    assert!(!is_read_only_tool("drag"));
    assert!(!is_read_only_tool("scroll"));
    assert!(!is_read_only_tool("press_key"));
    assert!(!is_read_only_tool("hover"));
    assert!(!is_read_only_tool("set_download_dir"));
    assert!(!is_read_only_tool("wait_for_download"));
    assert!(!is_read_only_tool("new_tab"));
    assert!(!is_read_only_tool("switch_tab"));
    assert!(!is_read_only_tool("close_tab"));
    assert!(!is_read_only_tool("select_option"));
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
      "list_tabs",
      "new_tab",
      "switch_tab",
      "close_tab",
      "wait_for_text",
      "wait_for_url",
      "select_option",
      "find_text",
      "get_cookies",
      "extract_table",
      "extract_article",
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
        guardrails: AgentGuardrails::default(),
        run_id: None,
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
        guardrails: AgentGuardrails::default(),
        run_id: None,
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
        guardrails: AgentGuardrails::default(),
        run_id: None,
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

  #[test]
  fn url_allowlist_matches_subdomains() {
    let allowed = vec!["example.com".to_string()];
    assert!(url_allowed("https://example.com/a", &allowed));
    assert!(url_allowed("https://app.example.com/a", &allowed));
    assert!(!url_allowed("https://evil.com/", &allowed));
    assert!(!url_allowed("not a url at all", &allowed));
    assert!(url_allowed("https://anything.example/", &[]));
  }

  #[test]
  fn forbidden_selectors_block_matching_targets() {
    let forbidden = vec![
      "#logout".to_string(),
      "[data-action=\"delete\"]".to_string(),
    ];
    assert_eq!(
      forbidden_selector_hit(&serde_json::json!({ "selector": "#logout" }), &forbidden),
      Some("#logout".to_string())
    );
    assert!(
      forbidden_selector_hit(&serde_json::json!({ "selector": "#login" }), &forbidden).is_none()
    );
    assert!(forbidden_selector_hit(&serde_json::json!({}), &forbidden).is_none());
  }

  #[test]
  fn redact_secrets_masks_password_values() {
    let redacted = redact_secrets(r#"{"username":"amy","password":"s3cret!"}"#);
    assert!(!redacted.contains("s3cret"));
    assert!(redacted.contains("***"));
    assert!(redacted.contains("amy"));
  }

  #[test]
  fn cancel_and_active_runs_roundtrip() {
    let flag = register_agent_run("test-run-cancel", "label".to_string());
    assert!(!run_cancelled(&flag));
    assert!(active_agent_runs()
      .iter()
      .any(|r| r.run_id == "test-run-cancel"));
    assert!(cancel_agent_run("test-run-cancel"));
    assert!(run_cancelled(&flag));
    assert!(!cancel_agent_run("no-such-run"));
    finish_agent_run("test-run-cancel");
    assert!(!active_agent_runs()
      .iter()
      .any(|r| r.run_id == "test-run-cancel"));
  }

  #[test]
  fn screenshot_data_url_extracts_and_caps() {
    let big = "A".repeat(MAX_INLINE_IMAGE_CHARS + 100);
    let result = serde_json::json!({
      "content": [
        { "type": "text", "text": "ok" },
        { "type": "image", "data": big, "mimeType": "image/png" }
      ]
    });
    let url = screenshot_data_url(&result).unwrap();
    assert!(url.starts_with("data:image/png;base64,"));
    assert!(url.len() <= MAX_INLINE_IMAGE_CHARS);
    let none = screenshot_data_url(&serde_json::json!({ "content": [] }));
    assert!(none.is_none());
  }

  #[test]
  fn full_auto_prompt_declares_immediate_execution() {
    assert!(system_prompt_full_auto().contains("FULL AUTOMATION IS ON"));
    assert!(!system_prompt().contains("FULL AUTOMATION IS ON"));
  }

  #[test]
  fn safe_mode_records_cards_while_full_auto_executes() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      // Safe mode: mutating tool becomes a pending card, nothing executes.
      let (text, card) = run_tool_call_with_mode(
        "navigate",
        &serde_json::json!({ "profile_id": "p1", "url": "https://example.com" }),
        false,
      )
      .await
      .unwrap();
      let card = card.expect("safe mode must record a card");
      assert!(text.contains("waiting for the user's confirmation"));
      PENDING_ACTIONS.lock().unwrap().remove(&card.id);

      // Full-auto mode: the same call dispatches immediately and fails
      // offline (no running browser) instead of recording a card.
      let err = run_tool_call_with_mode(
        "navigate",
        &serde_json::json!({ "profile_id": "p1", "url": "https://example.com" }),
        true,
      )
      .await
      .unwrap_err();
      assert!(
        !err.contains("waiting for the user's confirmation"),
        "full auto must execute, not record: {err}"
      );
    });
  }

  #[test]
  fn unattended_apply_skips_irreversible_without_full_auto() {
    // Hermetic by construction: the cards below are never inserted into the
    // process-global pending map, so no sibling test can disturb them and
    // no browser is needed. A dispatch for an absent card deterministically
    // reports card_not_found, which still proves the attempt happened.
    fn card(tool: &str, reversible: bool) -> ChangeCard {
      ChangeCard {
        id: Uuid::new_v4().to_string(),
        kind: "custom".to_string(),
        title: tool.to_string(),
        description: tool.to_string(),
        diff: serde_json::json!({ "tool": tool, "args": { "profile_id": "p1" } }),
        reversible,
      }
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
      // Safe default: reversible cards are attempted, irreversible ones are
      // skipped and reported instead of blocking for a human.
      let outcome =
        apply_cards_for_unattended(&[card("navigate", true), card("evaluate", false)], false).await;
      assert_eq!(outcome.skipped.len(), 1);
      assert_eq!(outcome.applied.len(), 0);
      assert_eq!(outcome.errors.len(), 1);

      // Full automation attempts everything and skips nothing.
      let outcome =
        apply_cards_for_unattended(&[card("navigate", true), card("evaluate", false)], true).await;
      assert!(outcome.skipped.is_empty());
      assert_eq!(outcome.errors.len(), 2);
    });
  }

  #[test]
  fn agent_browser_tools_match_shared_catalog() {
    let catalog = crate::browser_tools::browser_tools();
    let tools = agent_tools();
    for entry in &catalog {
      let found = tools
        .iter()
        .find(|t| t.name == entry.name)
        .unwrap_or_else(|| panic!("catalog tool {} missing from agent", entry.name));
      assert_eq!(found.description, entry.description);
      assert_eq!(found.input_schema, entry.input_schema);
    }
  }

  #[test]
  fn unknown_tools_rejected_before_cards_or_dispatch() {
    // The rejection happens before any map access, so this holds regardless
    // of what sibling tests do to the process-global pending map.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let err = runtime
      .block_on(run_tool_call_with_mode(
        "no_such_tool",
        &serde_json::json!({ "profile_id": "p1" }),
        false,
      ))
      .unwrap_err();
    assert!(err.contains("AGENT_TOOL_UNKNOWN"));
  }
}
