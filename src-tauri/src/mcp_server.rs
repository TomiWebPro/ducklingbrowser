use axum::{
  body::Body,
  extract::State,
  http::{header, Request, StatusCode},
  middleware::{self, Next},
  response::{IntoResponse, Response},
  routing::{get, post},
  Json, Router,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use tauri::AppHandle;
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::browser::ProxySettings;
use crate::cdp_session::CdpSession;
use crate::chromium_terms::ChromiumTermsManager;
use crate::group_manager::GROUP_MANAGER;
use crate::profile::{BrowserProfile, ProfileManager};
use crate::proxy_manager::PROXY_MANAGER;
use crate::settings_manager::SettingsManager;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
  pub name: String,
  pub description: String,
  pub input_schema: serde_json::Value,
}

/// JavaScript executed in the target page to enumerate visible interactive
/// elements. Returns a JSON string `{elements, count, truncated}` where
/// `elements` is the newline-joined labeled list. Live references are stashed
/// on `window.__duckling_interactive` so subsequent `click_by_index` /
/// `type_by_index` calls can resolve `index → Element` without round-tripping
/// a selector. `__MAX_CHARS__` is substituted at call time.
const INTERACTIVE_ELEMENTS_JS: &str = r#"(() => {
  const SELECTORS = 'a, button, input, select, textarea, [role="button"], [role="link"], [role="checkbox"], [role="radio"], [role="tab"], [role="menuitem"], [role="combobox"], [role="option"], [contenteditable=""], [contenteditable="true"], [tabindex]:not([tabindex="-1"])';
  const ATTRS = ['type','name','id','role','aria-label','aria-checked','aria-expanded','placeholder','title','value','href','alt'];
  const MAX_CHARS = __MAX_CHARS__;
  const interactive = [];
  const lines = [];
  let truncated = false;
  let total = 0;
  const nodes = document.querySelectorAll(SELECTORS);
  for (const el of nodes) {
    if (el.disabled) continue;
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) continue;
    const style = window.getComputedStyle(el);
    if (style.visibility === 'hidden' || style.display === 'none' || style.opacity === '0') continue;
    const tag = el.tagName.toLowerCase();
    const parts = [];
    for (const a of ATTRS) {
      const v = el.getAttribute(a);
      if (v) parts.push(a + '="' + String(v).slice(0,100).replace(/"/g,'\\"') + '"');
    }
    let text = '';
    if (!['INPUT','TEXTAREA','SELECT'].includes(el.tagName)) {
      text = (el.innerText || el.textContent || '').trim().replace(/\s+/g,' ').slice(0,100);
    }
    const idx = interactive.length;
    const line = '[' + idx + ']<' + tag + (parts.length ? ' ' + parts.join(' ') : '') + '>' + text + '</' + tag + '>';
    if (total + line.length + 1 > MAX_CHARS) { truncated = true; break; }
    total += line.length + 1;
    interactive.push(el);
    lines.push(line);
  }
  window.__duckling_interactive = interactive;
  return JSON.stringify({ elements: lines.join('\n'), count: interactive.length, truncated: truncated });
})()"#;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpRequest {
  jsonrpc: String,
  id: Option<serde_json::Value>,
  method: String,
  params: Option<serde_json::Value>,
}

const PROTOCOL_VERSION: &str = "2025-11-25";
const SERVER_NAME: &str = "duckling-browser";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize)]
pub struct McpResponse {
  jsonrpc: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  id: Option<serde_json::Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  result: Option<serde_json::Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<McpError>,
}

#[derive(Debug, Serialize)]
pub struct McpError {
  code: i32,
  message: String,
}

impl std::fmt::Display for McpError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}: {}", self.code, self.message)
  }
}

impl From<crate::cdp_session::CdpError> for McpError {
  fn from(e: crate::cdp_session::CdpError) -> Self {
    McpError {
      code: e.code,
      message: e.message,
    }
  }
}

const DEFAULT_MCP_PORT: u16 = 51080;

struct McpSession {
  initialized: bool,
}

struct McpServerInner {
  app_handle: Option<AppHandle>,
  token: Option<String>,
  shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
  sessions: HashMap<String, McpSession>,
}

#[derive(Clone)]
struct McpHttpState {
  server: &'static McpServer,
  token: String,
}

pub struct McpServer {
  inner: Arc<AsyncMutex<McpServerInner>>,
  is_running: AtomicBool,
  port: AtomicU16,
}

impl McpServer {
  fn new() -> Self {
    Self {
      inner: Arc::new(AsyncMutex::new(McpServerInner {
        app_handle: None,
        token: None,
        shutdown_tx: None,
        sessions: HashMap::new(),
      })),
      is_running: AtomicBool::new(false),
      port: AtomicU16::new(0),
    }
  }

  pub fn instance() -> &'static McpServer {
    &MCP_SERVER
  }

  pub fn is_running(&self) -> bool {
    self.is_running.load(Ordering::SeqCst)
  }

  pub fn get_port(&self) -> Option<u16> {
    let port = self.port.load(Ordering::SeqCst);
    if port > 0 {
      Some(port)
    } else {
      None
    }
  }

  pub async fn start(&self, app_handle: AppHandle) -> Result<u16, String> {
    if !ChromiumTermsManager::instance().is_terms_accepted() {
      return Err(crate::backend_error("CHROMIUM_TERMS_REQUIRED"));
    }

    if self.is_running() {
      return Err(crate::backend_error("MCP_SERVER_ALREADY_RUNNING"));
    }

    let settings_manager = SettingsManager::instance();
    let settings = settings_manager
      .load_settings()
      .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?;

    // Get or generate token
    let existing_token = settings_manager
      .get_mcp_token(&app_handle)
      .await
      .ok()
      .flatten();

    let token = if let Some(t) = existing_token {
      t
    } else {
      settings_manager
        .generate_mcp_token(&app_handle)
        .await
        .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?
    };

    // Determine port (use saved port, or try default, or random)
    let preferred_port = settings.mcp_port.unwrap_or(DEFAULT_MCP_PORT);
    let listener = self.bind_to_available_port(preferred_port).await?;
    let actual_port = listener
      .local_addr()
      .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?
      .port();

    // Save port if it changed
    if settings.mcp_port != Some(actual_port) {
      let mut new_settings = settings;
      new_settings.mcp_port = Some(actual_port);
      settings_manager
        .save_settings(&new_settings)
        .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?;
    }

    // Store state
    let mut inner = self.inner.lock().await;
    inner.app_handle = Some(app_handle);
    inner.token = Some(token.clone());

    // Create shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    inner.shutdown_tx = Some(shutdown_tx);

    self.port.store(actual_port, Ordering::SeqCst);
    self.is_running.store(true, Ordering::SeqCst);

    // Start HTTP server in background
    let http_state = McpHttpState {
      server: McpServer::instance(),
      token,
    };
    tokio::spawn(Self::run_http_server(listener, http_state, shutdown_rx));

    log::info!("[mcp] Server started on port {}", actual_port);
    Ok(actual_port)
  }

  async fn bind_to_available_port(&self, preferred: u16) -> Result<TcpListener, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], preferred));
    if let Ok(listener) = TcpListener::bind(addr).await {
      return Ok(listener);
    }

    for _ in 0..10 {
      let port = 51000 + (rand::random::<u16>() % 1000);
      let addr = SocketAddr::from(([127, 0, 0, 1], port));
      if let Ok(listener) = TcpListener::bind(addr).await {
        return Ok(listener);
      }
    }

    Err(crate::backend_error("MCP_PORT_UNAVAILABLE"))
  }

  async fn run_http_server(
    listener: TcpListener,
    state: McpHttpState,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
  ) {
    let app = Router::new()
      .route(
        "/mcp/{token}",
        post(Self::handle_mcp_post)
          .get(Self::handle_mcp_get)
          .delete(Self::handle_mcp_delete),
      )
      .route(
        "/mcp",
        post(Self::handle_mcp_post)
          .get(Self::handle_mcp_get)
          .delete(Self::handle_mcp_delete),
      )
      .route("/health", get(Self::handle_health))
      .layer(middleware::from_fn_with_state(
        state.clone(),
        Self::auth_middleware,
      ))
      .with_state(state);

    let port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
    let server = async move {
      log::info!("[mcp] Server listening on http://127.0.0.1:{}/mcp", port);
      if let Err(e) = axum::serve(listener, app).await {
        log::error!("[mcp] Server error: {}", e);
      }
    };

    tokio::select! {
      _ = server => {},
      _ = shutdown_rx => {
        log::info!("[mcp] Server shutting down");
      },
    }
  }

  async fn auth_middleware(
    State(state): State<McpHttpState>,
    req: Request<Body>,
    next: Next,
  ) -> Result<Response, StatusCode> {
    let path = req.uri().path();

    if path == "/health" {
      return Ok(next.run(req).await);
    }

    // Check token from URL path: /mcp/{token}
    let path_token = path
      .strip_prefix("/mcp/")
      .filter(|t| !t.is_empty() && !t.contains('/'));

    // Check token from Authorization header
    let header_token = req
      .headers()
      .get(header::AUTHORIZATION)
      .and_then(|h| h.to_str().ok())
      .and_then(|h| h.strip_prefix("Bearer "));

    // Constant-time comparison to avoid leaking the token prefix via timing.
    use subtle::ConstantTimeEq;
    let expected = state.token.as_bytes();
    let ct_eq = |t: Option<&str>| {
      t.is_some_and(|t| {
        let b = t.as_bytes();
        b.len() == expected.len() && b.ct_eq(expected).into()
      })
    };
    let valid = ct_eq(path_token) || ct_eq(header_token);

    if !valid {
      return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
  }

  async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({
      "status": "ok",
      "server": SERVER_NAME,
      "version": SERVER_VERSION,
      "protocolVersion": PROTOCOL_VERSION,
    }))
  }

  async fn handle_mcp_get() -> impl IntoResponse {
    // We don't support server-initiated SSE streams
    StatusCode::METHOD_NOT_ALLOWED
  }

  async fn handle_mcp_delete(
    State(state): State<McpHttpState>,
    req: Request<Body>,
  ) -> impl IntoResponse {
    let session_id = req
      .headers()
      .get("mcp-session-id")
      .and_then(|h| h.to_str().ok())
      .map(|s| s.to_string());

    if let Some(sid) = session_id {
      let mut inner = state.server.inner.lock().await;
      inner.sessions.remove(&sid);
      log::info!("[mcp] Session terminated: {}", sid);
    }

    StatusCode::OK
  }

  async fn handle_mcp_post(State(state): State<McpHttpState>, req: Request<Body>) -> Response {
    let session_id = req
      .headers()
      .get("mcp-session-id")
      .and_then(|h| h.to_str().ok())
      .map(|s| s.to_string());

    let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
      Ok(b) => b,
      Err(_) => {
        return (StatusCode::BAD_REQUEST, "Invalid request body").into_response();
      }
    };

    let request: McpRequest = match serde_json::from_slice(&body_bytes) {
      Ok(r) => r,
      Err(_) => {
        return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
      }
    };

    let is_notification = request.id.is_none();
    let method = request.method.clone();

    // Handle initialize (no session required)
    if method == "initialize" {
      let response = state.server.handle_initialize(request).await;
      match response {
        Ok((session_id, result)) => {
          let body = McpResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(result.0),
            result: Some(result.1),
            error: None,
          };
          Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header("mcp-session-id", &session_id)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
        }
        Err((id, error)) => {
          let body = McpResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            result: None,
            error: Some(error),
          };
          Json(body).into_response()
        }
      }
    } else if is_notification {
      // Notifications (like notifications/initialized) -> 202 Accepted
      if method == "notifications/initialized" {
        if let Some(sid) = &session_id {
          let mut inner = state.server.inner.lock().await;
          if let Some(session) = inner.sessions.get_mut(sid) {
            session.initialized = true;
          }
        }
      }
      StatusCode::ACCEPTED.into_response()
    } else {
      // Validate session exists
      if let Some(sid) = &session_id {
        let inner = state.server.inner.lock().await;
        if !inner.sessions.contains_key(sid) {
          return StatusCode::NOT_FOUND.into_response();
        }
      }

      if Self::is_automation_tool_call(&request) {
        if let crate::automation_rate_limiter::RateLimitOutcome::Limited { retry_after_secs } =
          crate::automation_rate_limiter::check_automation_rate_limit().await
        {
          log::warn!(
            "[mcp] Rejected tools/call: automation rate limit exceeded; retry in {}s",
            retry_after_secs
          );
          return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after_secs.to_string())],
            "automation request rate limit exceeded",
          )
            .into_response();
        }
      }

      let response = state.server.handle_request(request).await;
      Json(response).into_response()
    }
  }

  fn is_automation_tool_call(request: &McpRequest) -> bool {
    if request.method != "tools/call" {
      return false;
    }

    let Some(tool_name) = request
      .params
      .as_ref()
      .and_then(|params| params.get("name"))
      .and_then(|name| name.as_str())
    else {
      return false;
    };

    matches!(
      tool_name,
      "run_profile"
        | "kill_profile"
        | "batch_run_profiles"
        | "batch_stop_profiles"
        | "start_sync_session"
        | "scheduler_run_now"
    ) || crate::browser_tools::is_automation_browser_tool(tool_name)
  }

  pub async fn stop(&self) -> Result<(), String> {
    if !self.is_running() {
      return Err(crate::backend_error("MCP_SERVER_NOT_RUNNING"));
    }

    let mut inner = self.inner.lock().await;
    inner.app_handle = None;
    inner.token = None;
    inner.sessions.clear();

    // Send shutdown signal
    if let Some(tx) = inner.shutdown_tx.take() {
      let _ = tx.send(());
    }

    self.port.store(0, Ordering::SeqCst);
    self.is_running.store(false, Ordering::SeqCst);

    log::info!("[mcp] Server stopped");
    Ok(())
  }

  pub fn get_tools(&self) -> Vec<McpTool> {
    let mut tools = vec![
      McpTool {
        name: "list_profiles".to_string(),
        description: "List all browser profiles".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_profile".to_string(),
        description: "Get details of a specific browser profile".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to retrieve"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "run_profile".to_string(),
        description: "Launch a browser profile with an optional URL.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to launch"
            },
            "url": {
              "type": "string",
              "description": "Optional URL to open in the browser"
            },
            "headless": {
              "type": "boolean",
              "description": "Run the browser in headless mode"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "kill_profile".to_string(),
        description: "Stop a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to stop"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "batch_run_profiles".to_string(),
        description: "Launch multiple browser profiles at once with an optional URL.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "UUIDs of the profiles to launch"
            },
            "url": {
              "type": "string",
              "description": "Optional URL to open in every launched profile"
            },
            "headless": {
              "type": "boolean",
              "description": "Run the browsers in headless mode"
            }
          },
          "required": ["profile_ids"]
        }),
      },
      McpTool {
        name: "batch_stop_profiles".to_string(),
        description: "Stop multiple running browser profiles at once.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "UUIDs of the profiles to stop"
            }
          },
          "required": ["profile_ids"]
        }),
      },
      McpTool {
        name: "create_profile".to_string(),
        description: "Create a new browser profile. Schemas stay English; pass responseLanguage to localize the AI reply only.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": {
              "type": "string",
              "description": "Name for the new profile"
            },
            "browser": {
              "type": "string",
              "enum": ["chromium"],
              "description": "Browser engine to use"
            },
            "proxy_id": {
              "type": "string",
              "description": "Optional proxy UUID to assign (mutually exclusive with vpn_id)"
            },
            "vpn_id": {
              "type": "string",
              "description": "Optional VPN UUID to assign (mutually exclusive with proxy_id)"
            },
            "launch_hook": {
              "type": "string",
              "description": "Optional HTTP(S) URL to call before launch for transient proxy overrides"
            },
            "group_id": {
              "type": "string",
              "description": "Optional group UUID to assign"
            },
            "tags": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Optional tags for the profile"
            },
            "note": {
              "type": "string",
              "description": "Optional user note"
            },
            "window_color": {
              "type": "string",
              "description": "Optional window frame color #RRGGBB (invalid reverts to auto)"
            },
            "download_dir": {
              "type": "string",
              "description": "Optional absolute download folder (empty clears to default)"
            },
            "allow_agent_downloads": {
              "type": "boolean",
              "description": "Allow agents to trigger downloads in this profile"
            },
            "agent_auto_approve": {
              "type": "boolean",
              "description": "Apply all agent-proposed changes without asking"
            },
            "agent_key_id": {
              "type": "string",
              "description": "Preferred saved AI key id (empty clears)"
            },
            "agent_id": {
              "type": "string",
              "description": "Preferred CLI agent id, e.g. opencode (empty clears)"
            },
            "dns_blocklist": {
              "type": "string",
              "enum": ["light", "normal", "pro", "pro_plus", "ultimate"],
              "description": "Optional DNS blocklist level (omit for none)"
            },
            "ephemeral": {
              "type": "boolean",
              "description": "RAM-backed profile wiped on quit (cannot combine with password/sync)"
            },
            "clear_on_close": {
              "type": "boolean",
              "description": "Wipe browsing data on exit (rejected for ephemeral profiles)"
            },
            "responseLanguage": {
              "type": "string",
              "description": "BCP-47 tag for the AI reply language, e.g. es. Tool schema stays English."
            }
          },
          "required": ["name", "browser"]
        }),
      },
      McpTool {
        name: "detect_browser_profiles".to_string(),
        description: "Detect importable Chromium-family browser profiles (Chrome, Chromium, Brave) on this machine, or scan a custom folder for profile directories".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "folder": {
              "type": "string",
              "description": "Optional folder to scan instead of the default browser locations. Accepts a single profile dir, a Chromium user-data dir, or a folder holding one profile dir per child."
            }
          }
        }),
      },
      McpTool {
        name: "import_browser_profiles".to_string(),
        description: "Bulk-import browser profiles from on-disk profile folders (e.g. paths returned by detect_browser_profiles). Each imported profile becomes a Chromium profile; items are isolated so one failure doesn't stop the rest".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "items": {
              "type": "array",
              "items": {
                "type": "object",
                "properties": {
                  "source_path": {
                    "type": "string",
                    "description": "Path to the source profile directory"
                  },
                  "new_profile_name": {
                    "type": "string",
                    "description": "Name for the imported profile"
                  },
                  "proxy_id": {
                    "type": "string",
                    "description": "Optional proxy UUID to assign to this profile"
                  },
                  "vpn_id": {
                    "type": "string",
                    "description": "Optional VPN UUID to assign to this profile"
                  }
                },
                "required": ["source_path", "new_profile_name"]
              },
              "description": "Profiles to import"
            },
            "group_id": {
              "type": "string",
              "description": "Optional group UUID assigned to every imported profile"
            },
            "duplicate_strategy": {
              "type": "string",
              "enum": ["skip", "rename"],
              "description": "How to handle an already-taken profile name (default: rename with a numeric suffix)"
            }
          },
          "required": ["items"]
        }),
      },
      McpTool {
        name: "update_profile".to_string(),
        description: "Update an existing browser profile's settings. Schemas stay English; pass responseLanguage to localize the AI reply only.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to update"
            },
            "name": {
              "type": "string",
              "description": "New name for the profile"
            },
            "proxy_id": {
              "type": "string",
              "description": "Proxy UUID to assign (empty string to remove; clears vpn_id)"
            },
            "vpn_id": {
              "type": "string",
              "description": "VPN UUID to assign (empty string to remove; clears proxy_id)"
            },
            "launch_hook": {
              "type": "string",
              "description": "Launch hook URL to assign (empty string to remove)"
            },
            "group_id": {
              "type": "string",
              "description": "Group UUID to assign (empty string to remove)"
            },
            "tags": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Tags for the profile (replaces existing tags)"
            },
            "extension_group_id": {
              "type": "string",
              "description": "Extension group UUID to assign (empty string to remove)"
            },
            "proxy_bypass_rules": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Proxy bypass rules (replaces existing rules)"
            },
            "clear_on_close": {
              "type": "boolean",
              "description": "Wipe browsing data (keeping extensions and bookmarks) when the browser exits. Not available for ephemeral or password-protected profiles."
            },
            "version": {
              "type": "string",
              "description": "Downloaded browser version to pin (rejected while running)"
            },
            "note": {
              "type": "string",
              "description": "User note (empty string clears)"
            },
            "window_color": {
              "type": "string",
              "description": "Window frame color #RRGGBB (empty string clears to auto)"
            },
            "download_dir": {
              "type": "string",
              "description": "Absolute download folder (empty string clears to default)"
            },
            "allow_agent_downloads": {
              "type": "boolean",
              "description": "Allow agents to trigger downloads in this profile"
            },
            "agent_auto_approve": {
              "type": "boolean",
              "description": "Apply all agent-proposed changes without asking"
            },
            "agent_key_id": {
              "type": "string",
              "description": "Preferred saved AI key id (empty string clears)"
            },
            "agent_id": {
              "type": "string",
              "description": "Preferred CLI agent id (empty string clears)"
            },
            "sync_mode": {
              "type": "string",
              "enum": ["Disabled", "Regular", "Encrypted"],
              "description": "Cloud sync mode for this profile"
            },
            "dns_blocklist": {
              "type": "string",
              "enum": ["none", "light", "normal", "pro", "pro_plus", "ultimate"],
              "description": "'none'/empty clears DNS blocking"
            },
            "responseLanguage": {
              "type": "string",
              "description": "BCP-47 tag for the AI reply language, e.g. es. Tool schema stays English."
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "delete_profile".to_string(),
        description: "Delete a browser profile and all its data".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to delete"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "list_tags".to_string(),
        description: "List all tags used across profiles".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "list_proxies".to_string(),
        description: "List all configured proxies".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_profile_status".to_string(),
        description: "Check if a browser profile is currently running".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to check"
            }
          },
          "required": ["profile_id"]
        }),
      },
      // Group management tools
      McpTool {
        name: "list_groups".to_string(),
        description: "List all profile groups".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_group".to_string(),
        description: "Get details of a specific group".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": {
              "type": "string",
              "description": "The UUID of the group to retrieve"
            }
          },
          "required": ["group_id"]
        }),
      },
      McpTool {
        name: "create_group".to_string(),
        description: "Create a new profile group".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": {
              "type": "string",
              "description": "The name for the new group"
            }
          },
          "required": ["name"]
        }),
      },
      McpTool {
        name: "update_group".to_string(),
        description: "Update an existing group's name".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": {
              "type": "string",
              "description": "The UUID of the group to update"
            },
            "name": {
              "type": "string",
              "description": "The new name for the group"
            }
          },
          "required": ["group_id", "name"]
        }),
      },
      McpTool {
        name: "delete_group".to_string(),
        description: "Delete a profile group".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": {
              "type": "string",
              "description": "The UUID of the group to delete"
            }
          },
          "required": ["group_id"]
        }),
      },
      McpTool {
        name: "assign_profiles_to_group".to_string(),
        description: "Assign one or more profiles to a group".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Array of profile UUIDs to assign"
            },
            "group_id": {
              "type": "string",
              "description": "The UUID of the group to assign to (null to remove from group)"
            }
          },
          "required": ["profile_ids"]
        }),
      },
      // Full proxy management tools
      McpTool {
        name: "get_proxy".to_string(),
        description: "Get details of a specific proxy".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "proxy_id": {
              "type": "string",
              "description": "The UUID of the proxy to retrieve"
            }
          },
          "required": ["proxy_id"]
        }),
      },
      McpTool {
        name: "create_proxy".to_string(),
        description: "Create a new proxy configuration.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": {
              "type": "string",
              "description": "The name for the new proxy"
            },
            "proxy_type": {
              "type": "string",
              "enum": ["http", "https", "socks4", "socks5"],
              "description": "The type of proxy (for regular proxies)"
            },
            "host": {
              "type": "string",
              "description": "The proxy host address (for regular proxies)"
            },
            "port": {
              "type": "integer",
              "description": "The proxy port number (for regular proxies)"
            },
            "username": {
              "type": "string",
              "description": "Optional username for authentication (for regular proxies)"
            },
            "password": {
              "type": "string",
              "description": "Optional password for authentication (for regular proxies)"
            }
          },
          "required": ["name", "proxy_type", "host", "port"]
        }),
      },
      McpTool {
        name: "update_proxy".to_string(),
        description: "Update an existing proxy configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "proxy_id": {
              "type": "string",
              "description": "The UUID of the proxy to update"
            },
            "name": {
              "type": "string",
              "description": "New name for the proxy"
            },
            "proxy_type": {
              "type": "string",
              "enum": ["http", "https", "socks4", "socks5"],
              "description": "The type of proxy (for regular proxies)"
            },
            "host": {
              "type": "string",
              "description": "The proxy host address (for regular proxies)"
            },
            "port": {
              "type": "integer",
              "description": "The proxy port number (for regular proxies)"
            },
            "username": {
              "type": "string",
              "description": "Optional username for authentication (for regular proxies)"
            },
            "password": {
              "type": "string",
              "description": "Optional password for authentication (for regular proxies)"
            }
          },
          "required": ["proxy_id"]
        }),
      },
      McpTool {
        name: "delete_proxy".to_string(),
        description: "Delete a proxy configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "proxy_id": {
              "type": "string",
              "description": "The UUID of the proxy to delete"
            }
          },
          "required": ["proxy_id"]
        }),
      },
      McpTool {
        name: "export_proxies".to_string(),
        description: "Export all proxy configurations".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "format": {
              "type": "string",
              "enum": ["json", "txt"],
              "description": "Export format (json for structured data, txt for URL format)"
            }
          },
          "required": ["format"]
        }),
      },
      McpTool {
        name: "import_proxies".to_string(),
        description: "Import proxy configurations from JSON or TXT content".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "content": {
              "type": "string",
              "description": "The proxy configuration content to import"
            },
            "format": {
              "type": "string",
              "enum": ["json", "txt"],
              "description": "Import format (json or txt)"
            },
            "name_prefix": {
              "type": "string",
              "description": "Optional prefix for imported proxy names (default: 'Imported')"
            }
          },
          "required": ["content", "format"]
        }),
      },
      // Proxy pool tools
      McpTool {
        name: "create_proxy_pool".to_string(),
        description: "Create a proxy pool from existing stored proxies".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": {
              "type": "string",
              "description": "Pool name"
            },
            "proxy_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Stored proxy IDs that make up the pool"
            }
          },
          "required": ["name", "proxy_ids"]
        }),
      },
      McpTool {
        name: "list_proxy_pools".to_string(),
        description: "List all proxy pools".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "update_proxy_pool".to_string(),
        description: "Update a proxy pool's name and/or members".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "pool_id": {
              "type": "string",
              "description": "The UUID of the pool to update"
            },
            "name": {
              "type": "string",
              "description": "New pool name"
            },
            "proxy_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "New member proxy IDs"
            }
          },
          "required": ["pool_id", "name", "proxy_ids"]
        }),
      },
      McpTool {
        name: "delete_proxy_pool".to_string(),
        description: "Delete a proxy pool. Profiles keep their current proxy.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "pool_id": {
              "type": "string",
              "description": "The UUID of the pool to delete"
            }
          },
          "required": ["pool_id"]
        }),
      },
      McpTool {
        name: "assign_profiles_to_pool".to_string(),
        description: "Assign pool proxies to profiles using round-robin distribution".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "pool_id": {
              "type": "string",
              "description": "The UUID of the pool to distribute from"
            },
            "profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Profile UUIDs to assign a pool member to"
            }
          },
          "required": ["pool_id", "profile_ids"]
        }),
      },
      McpTool {
        name: "rotate_profile_proxy".to_string(),
        description: "Rotate a profile to the next member of its proxy pool".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to rotate"
            }
          },
          "required": ["profile_id"]
        }),
      },
      // LLM tools
      McpTool {
        name: "llm_completion".to_string(),
        description: "Fire a single LLM completion through the app's own key vault, with retry and per-provider concurrency. The key never leaves the machine.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "key_id": {
              "type": "string",
              "description": "Saved key id from the key vault. Omit to use the first saved key."
            },
            "provider": {
              "type": "string",
              "enum": ["anthropic", "openai", "groq", "xai", "google", "openrouter", "opencode", "custom"],
              "description": "Provider override. Omit to use the key's own provider."
            },
            "model": {
              "type": "string",
              "description": "Model override. Omit to use the key's configured model."
            },
            "messages": {
              "type": "array",
              "items": {
                "type": "object",
                "properties": {
                  "role": {
                    "type": "string",
                    "enum": ["system", "user", "assistant"]
                  },
                  "content": { "type": "string" }
                },
                "required": ["role", "content"]
              },
              "description": "Chat history (roles: system/user/assistant)"
            },
            "tools": {
              "type": "array",
              "items": {
                "type": "object",
                "properties": {
                  "name": { "type": "string" },
                  "description": { "type": "string" },
                  "input_schema": { "type": "object" }
                },
                "required": ["name", "description", "input_schema"]
              },
              "description": "Optional function-calling tools"
            },
            "max_retries": {
              "type": "integer",
              "minimum": 0,
              "maximum": 10,
              "description": "Retries on transient failures (429/5xx/transport). Defaults to 3."
            },
            "responseLanguage": {
              "type": "string",
              "description": "BCP-47 tag for the AI reply language, e.g. es. Tool schema stays English."
            }
          },
          "required": ["messages"]
        }),
      },
      // Agent tools
      McpTool {
        name: "agent_chat".to_string(),
        description: "Run the in-app AI agent against a saved key from the key vault. The agent can browse profiles, execute browser tools, and returns change cards for confirmation. Pass auto_approve=true for full automation (tools execute immediately, no cards).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "key_id": {
              "type": "string",
              "description": "Saved key id from the key vault. Omit to use the first saved key."
            },
            "model": {
              "type": "string",
              "description": "Model override. Omit to use the key's configured model."
            },
            "message": {
              "type": "string",
              "description": "The task or question for the agent"
            },
            "use_agent": {
              "type": "string",
              "enum": ["agent", "chat"],
              "description": "Run with the full agent (tools) or as a plain chat"
            },
            "auto_approve": {
              "type": "boolean",
              "description": "Full automation: execute tool actions immediately without confirmation cards (default: false)"
            },
            "responseLanguage": {
              "type": "string",
              "description": "BCP-47 tag for the AI reply language, e.g. es. Tool schema stays English."
            }
          },
          "required": ["message"]
        }),
      },
      McpTool {
        name: "agent_chat_confirm".to_string(),
        description: "Confirm and execute the pending change cards produced by agent_chat".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "card_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Change card ids to confirm"
            }
          },
          "required": ["card_ids"]
        }),
      },
      McpTool {
        name: "agent_chat_decline".to_string(),
        description: "Decline and discard pending change cards produced by agent_chat".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "card_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Change card ids to decline"
            }
          },
          "required": ["card_ids"]
        }),
      },
      // VPN management tools
      McpTool {
        name: "import_vpn".to_string(),
        description: "Import a WireGuard (.conf), VLESS (vless://), or Xray JSON VPN configuration"
          .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "content": {
              "type": "string",
              "description": "Raw VPN config: WireGuard .conf, vless:// share link, or Xray JSON"
            },
            "filename": {
              "type": "string",
              "description": "Original filename (e.g. server.conf, link.txt, config.json)"
            },
            "name": {
              "type": "string",
              "description": "Optional display name for the VPN config"
            }
          },
          "required": ["content", "filename"]
        }),
      },
      McpTool {
        name: "list_vpn_configs".to_string(),
        description: "List all stored VPN configurations".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "delete_vpn".to_string(),
        description: "Delete a VPN configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": {
              "type": "string",
              "description": "The UUID of the VPN config to delete"
            }
          },
          "required": ["vpn_id"]
        }),
      },
      McpTool {
        name: "connect_vpn".to_string(),
        description: "Connect to a VPN configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": {
              "type": "string",
              "description": "The UUID of the VPN config to connect"
            }
          },
          "required": ["vpn_id"]
        }),
      },
      McpTool {
        name: "disconnect_vpn".to_string(),
        description: "Disconnect from a VPN".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": {
              "type": "string",
              "description": "The UUID of the VPN to disconnect"
            }
          },
          "required": ["vpn_id"]
        }),
      },
      McpTool {
        name: "get_vpn_status".to_string(),
        description: "Get the connection status of a VPN".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": {
              "type": "string",
              "description": "The UUID of the VPN to check"
            }
          },
          "required": ["vpn_id"]
        }),
      },
      // Fingerprint management tools
      McpTool {
        name: "get_profile_fingerprint".to_string(),
        description: "Get the fingerprint configuration for a Chromium profile"
          .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "update_profile_fingerprint".to_string(),
        description:
          "Update the fingerprint configuration for a Chromium profile."
            .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to update"
            },
            "fingerprint": {
              "type": "string",
              "description": "JSON string of the fingerprint configuration, or null to clear"
            },
            "os": {
              "type": "string",
              "enum": ["windows", "macos", "linux"],
              "description": "Operating system for fingerprint generation"
            },
            "randomize_fingerprint_on_launch": {
              "type": "boolean",
              "description": "Whether to generate a new fingerprint on every launch"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "update_profile_proxy_bypass_rules".to_string(),
        description:
          "Update proxy bypass rules for a profile. Requests matching these rules will connect directly, bypassing the proxy."
            .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to update"
            },
            "rules": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Array of bypass rules. Supports hostnames (e.g. 'example.com'), IP addresses, and regex patterns."
            }
          },
          "required": ["profile_id", "rules"]
        }),
      },
      McpTool {
        name: "update_profile_dns_blocklist".to_string(),
        description:
          "Update the DNS blocklist level for a profile. Blocks ads, trackers, and malware domains at the proxy level."
            .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to update"
            },
            "level": {
              "type": "string",
              "enum": ["none", "light", "normal", "pro", "pro_plus", "ultimate"],
              "description": "DNS blocklist level. 'none' disables blocking."
            }
          },
          "required": ["profile_id", "level"]
        }),
      },
      McpTool {
        name: "get_dns_blocklist_status".to_string(),
        description: "Get the cache status of all DNS blocklist tiers including entry counts and freshness.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "list_extensions".to_string(),
        description: "List all managed browser extensions.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "list_extension_groups".to_string(),
        description: "List all extension groups.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "create_extension_group".to_string(),
        description: "Create a new extension group.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": { "type": "string", "description": "Name for the extension group" }
          },
          "required": ["name"]
        }),
      },
      McpTool {
        name: "delete_extension".to_string(),
        description: "Delete a managed extension.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "extension_id": { "type": "string", "description": "The extension ID to delete" }
          },
          "required": ["extension_id"]
        }),
      },
      McpTool {
        name: "delete_extension_group".to_string(),
        description: "Delete an extension group.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": { "type": "string", "description": "The extension group ID to delete" }
          },
          "required": ["group_id"]
        }),
      },
      McpTool {
        name: "assign_extension_group_to_profile".to_string(),
        description: "Assign an extension group to a profile, or remove the assignment.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The profile ID" },
            "extension_group_id": { "type": "string", "description": "The extension group ID, or empty string to remove" }
          },
          "required": ["profile_id"]
        }),
      },
      // Cookie management tools
      McpTool {
        name: "import_profile_cookies".to_string(),
        description: "Import cookies into a Chromium profile from a JSON array (Puppeteer / EditThisCookie format) or a Netscape cookies.txt. Format is auto-detected. The browser must not be running.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the target profile"
            },
            "content": {
              "type": "string",
              "description": "Raw cookie file content (JSON array or Netscape cookies.txt)"
            }
          },
          "required": ["profile_id", "content"]
        }),
      },
      // Team lock tools
      McpTool {
        name: "get_team_locks".to_string(),
        description: "List all active team profile locks.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_team_lock_status".to_string(),
        description: "Check if a profile is locked by a team member.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to check"
            }
          },
          "required": ["profile_id"]
        }),
      },
      // Synchronizer tools
      McpTool {
        name: "start_sync_session".to_string(),
        description: "Start a synchronizer session. Launches a leader profile and follower profiles, then mirrors all actions from the leader to the followers in real time. Only Chromium profiles are supported.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "leader_profile_id": {
              "type": "string",
              "description": "The UUID of the leader profile"
            },
            "follower_profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "UUIDs of follower profiles"
            }
          },
          "required": ["leader_profile_id", "follower_profile_ids"]
        }),
      },
      McpTool {
        name: "stop_sync_session".to_string(),
        description: "Stop an active synchronizer session. Kills all follower profiles and the leader.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "session_id": {
              "type": "string",
              "description": "The sync session ID"
            }
          },
          "required": ["session_id"]
        }),
      },
      McpTool {
        name: "get_sync_sessions".to_string(),
        description: "List all active synchronizer sessions.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {}
        }),
      },
      McpTool {
        name: "remove_sync_follower".to_string(),
        description: "Remove a follower from an active synchronizer session.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "session_id": {
              "type": "string",
              "description": "The sync session ID"
            },
            "follower_profile_id": {
              "type": "string",
              "description": "The UUID of the follower to remove"
            }
          },
          "required": ["session_id", "follower_profile_id"]
        }),
      },
      McpTool {
        name: "clone_profile".to_string(),
        description: "Clone a browser profile (storage copy with fresh fingerprint, sync disabled, no password). Schemas stay English; pass responseLanguage to localize the AI reply only.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "UUID of the profile to clone" },
            "name": { "type": "string", "description": "Optional name for the clone (omit for auto '<name> copy')" },
            "responseLanguage": { "type": "string", "description": "BCP-47 tag for the AI reply language, e.g. es. Tool schema stays English." }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "get_app_settings".to_string(),
        description: "Read redacted application settings (theme, language, background mode, LLM/launch/automation quotas). Secret tokens are never returned; token fields report presence only.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string", "description": "BCP-47 tag for the AI reply language, e.g. es. Tool schema stays English." }
          },
          "required": []
        }),
      },
      McpTool {
        name: "update_app_settings".to_string(),
        description: "Update safe application settings. Only listed fields are writable; API/MCP server wiring, tokens, sync URL, onboarding and OS-registration flags are rejected.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "theme": { "type": "string", "enum": ["light", "dark", "system", "custom"] },
            "custom_theme": { "type": "object", "additionalProperties": { "type": "string" }, "description": "CSS var map, e.g. {\"--background\": \"#1a1b26\"}. Only applied when theme is custom." },
            "language": { "type": "string", "enum": ["system", "en", "es", "pt", "fr", "zh", "ja", "ko", "ru", "tr", "vi"], "description": "'system' clears to None (system default)" },
            "disable_auto_updates": { "type": "boolean" },
            "keep_decrypted_profiles_in_ram": { "type": "boolean" },
            "keep_running_in_background": { "type": "boolean" },
            "llm_max_concurrency": { "type": "integer", "minimum": 1, "maximum": 64 },
            "llm_requests_per_hour": { "type": "integer", "minimum": 0 },
            "max_concurrent_launches": { "type": "integer", "minimum": 1, "maximum": 64 },
            "automation_requests_per_hour": { "type": "integer", "minimum": 0 },
            "responseLanguage": { "type": "string", "description": "BCP-47 tag for the AI reply language, e.g. es. Tool schema stays English." }
          },
          "required": [],
          "additionalProperties": false
        }),
      },
      McpTool {
        name: "get_table_sorting".to_string(),
        description: "Read the profile table sort state.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "update_table_sorting".to_string(),
        description: "Update the profile table sort state.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "column": { "type": "string", "enum": ["name", "browser", "status"] },
            "direction": { "type": "string", "enum": ["asc", "desc"] }
          },
          "required": ["column", "direction"],
          "additionalProperties": false
        }),
      },
      McpTool {
        name: "scheduler_list".to_string(),
        description: "List all scheduled AI tasks. Schemas stay English; pass responseLanguage to localize the AI reply only.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "scheduler_get".to_string(),
        description: "Get a scheduled task by id.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "task_id": { "type": "string", "description": "Task UUID" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["task_id"]
        }),
      },
      McpTool {
        name: "scheduler_save".to_string(),
        description: "Create or update a scheduled task (pass full TaskDefinition camelCase object; omit id on create). Validation stays server-side.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "task": { "type": "object", "description": "Full TaskDefinition (camelCase)" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["task"]
        }),
      },
      McpTool {
        name: "scheduler_delete".to_string(),
        description: "Delete a scheduled task.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "task_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["task_id"]
        }),
      },
      McpTool {
        name: "scheduler_set_enabled".to_string(),
        description: "Enable or disable a scheduled task.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "task_id": { "type": "string" },
            "enabled": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["task_id", "enabled"]
        }),
      },
      McpTool {
        name: "scheduler_run_now".to_string(),
        description: "Execute a scheduled task immediately (consumes automation quota).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "task_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["task_id"]
        }),
      },
      McpTool {
        name: "ai_keys_list".to_string(),
        description: "List saved AI keys (redacted; masked_key only, never plaintext). Configure the AI endpoint here, then ask the AI to tune the app.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "ai_keys_save".to_string(),
        description: "Create or update a saved AI key (creates or overwrites by name). Secret is accepted on input, never echoed back.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "provider": { "type": "string", "enum": ["anthropic", "openai", "groq", "xai", "google", "openrouter", "opencode", "custom"] },
            "name": { "type": "string" },
            "model": { "type": "string" },
            "key": { "type": "string", "description": "Plaintext secret" },
            "endpoint": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["provider", "model", "key"]
        }),
      },
      McpTool {
        name: "ai_keys_delete".to_string(),
        description: "Delete a saved AI key.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "key_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["key_id"]
        }),
      },
      McpTool {
        name: "ai_keys_test".to_string(),
        description: "Probe an AI key/endpoint without saving.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "provider": { "type": "string", "enum": ["anthropic", "openai", "groq", "xai", "google", "openrouter", "opencode", "custom"] },
            "model": { "type": "string" },
            "key": { "type": "string" },
            "key_id": { "type": "string", "description": "Saved key id; used when key is omitted" },
            "endpoint": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["provider", "model"]
        }),
      },
      McpTool {
        name: "ai_keys_models_get".to_string(),
        description: "List available models for a provider (public catalogs work keyless).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "provider": { "type": "string", "enum": ["anthropic", "openai", "groq", "xai", "google", "openrouter", "opencode", "custom"] },
            "key": { "type": "string" },
            "key_id": { "type": "string" },
            "endpoint": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["provider"]
        }),
      },
      McpTool {
        name: "subscription_list".to_string(),
        description: "List saved proxy subscriptions (import sources for proxy pools).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "subscription_entries".to_string(),
        description: "List parsed entries imported from a proxy subscription.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "subscription_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["subscription_id"]
        }),
      },
      McpTool {
        name: "subscription_save".to_string(),
        description: "Create a proxy subscription (omit id) or update one (pass id).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "id": { "type": "string", "description": "Omit to create" },
            "name": { "type": "string" },
            "url": { "type": "string" },
            "refresh_hours": { "type": "integer", "minimum": 1 },
            "use_proxy_id": { "type": "string" },
            "auto_check": { "type": "boolean" },
            "auto_prune": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["name", "url"]
        }),
      },
      McpTool {
        name: "subscription_delete".to_string(),
        description: "Delete a proxy subscription, optionally with its imported entries.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "id": { "type": "string" },
            "delete_entries": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["id"]
        }),
      },
      McpTool {
        name: "subscription_refresh".to_string(),
        description: "Fetch a proxy subscription now and reconcile its entries.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["id"]
        }),
      },
      McpTool {
        name: "subscription_preview".to_string(),
        description: "Parse a subscription URL without saving (shows link counts and suggested refresh interval).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "url": { "type": "string" },
            "use_proxy_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["url"]
        }),
      },
      McpTool {
        name: "dns_custom_get".to_string(),
        description: "Read the custom DNS rules (sources, block/allow lists, allowlist mode).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "dns_custom_set".to_string(),
        description: "Replace the custom DNS rules. All four fields are required and applied together.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "sources": { "type": "array", "items": { "type": "string" } },
            "block_domains": { "type": "array", "items": { "type": "string" } },
            "allow_domains": { "type": "array", "items": { "type": "string" } },
            "allowlist_mode": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["sources", "block_domains", "allow_domains", "allowlist_mode"]
        }),
      },
      McpTool {
        name: "dns_custom_import".to_string(),
        description: "Import custom DNS rules from text content (e.g. txt, hosts, adblock format).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "content": { "type": "string" },
            "format": { "type": "string", "description": "Rule list format, e.g. txt" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["content", "format"]
        }),
      },
      McpTool {
        name: "dns_custom_export".to_string(),
        description: "Export custom DNS rules as text in the requested format.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "format": { "type": "string", "description": "Rule list format, e.g. txt" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["format"]
        }),
      },
      McpTool {
        name: "dns_cache_status".to_string(),
        description: "Show DNS blocklist cache status per level.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "dns_refresh".to_string(),
        description: "Refresh stale DNS blocklists from their sources.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "extension_get".to_string(),
        description: "Get a browser extension by id.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "extension_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["extension_id"]
        }),
      },
      McpTool {
        name: "extension_add".to_string(),
        description: "Install a browser extension from base64 file content (.crx/.zip/.xpi).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": { "type": "string" },
            "file_name": { "type": "string" },
            "file_data_base64": { "type": "string", "description": "Base64-encoded extension file" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["name", "file_name", "file_data_base64"]
        }),
      },
      McpTool {
        name: "extension_update".to_string(),
        description: "Rename a browser extension.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "extension_id": { "type": "string" },
            "name": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["extension_id", "name"]
        }),
      },
      McpTool {
        name: "extension_update_group".to_string(),
        description: "Rename an extension group and/or replace its member list.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": { "type": "string" },
            "name": { "type": "string" },
            "extension_ids": { "type": "array", "items": { "type": "string" } },
            "responseLanguage": { "type": "string" }
          },
          "required": ["group_id"]
        }),
      },
      McpTool {
        name: "extension_add_to_group".to_string(),
        description: "Add an extension to an extension group.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": { "type": "string" },
            "extension_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["group_id", "extension_id"]
        }),
      },
      McpTool {
        name: "extension_remove_from_group".to_string(),
        description: "Remove an extension from an extension group.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": { "type": "string" },
            "extension_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["group_id", "extension_id"]
        }),
      },
      McpTool {
        name: "extension_group_for_profile".to_string(),
        description: "Get the extension group assigned to a profile, if any.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "vpn_get".to_string(),
        description: "Get a VPN config by id. Secret config_data is never returned; presence and length only.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["vpn_id"]
        }),
      },
      McpTool {
        name: "vpn_update".to_string(),
        description: "Rename a VPN config.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": { "type": "string" },
            "name": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["vpn_id", "name"]
        }),
      },
      McpTool {
        name: "vpn_validate".to_string(),
        description: "Check whether a VPN config is currently working.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["vpn_id"]
        }),
      },
      McpTool {
        name: "vpn_batch_import".to_string(),
        description: "Mass-import VPN configs, one per line (vless:// links or compact JSON; # comments skipped).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "content": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["content"]
        }),
      },
      McpTool {
        name: "vpn_list_active".to_string(),
        description: "List currently connected VPN tunnels.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "vpn_create_manual".to_string(),
        description: "Create a VPN config from pasted config text. The secret is accepted on input, never echoed back.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": { "type": "string" },
            "vpn_type": { "type": "string", "enum": ["wireguard", "vless"] },
            "config_data": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["name", "vpn_type", "config_data"]
        }),
      },
      McpTool {
        name: "sync_settings_get".to_string(),
        description: "Read cloud sync settings. The sync token is never returned; presence only.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "sync_status".to_string(),
        description: "Show whether sync is configured plus unsynced entity counts.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "sync_set_proxy_enabled".to_string(),
        description: "Toggle cloud sync for a stored proxy.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "proxy_id": { "type": "string" },
            "enabled": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["proxy_id", "enabled"]
        }),
      },
      McpTool {
        name: "sync_set_group_enabled".to_string(),
        description: "Toggle cloud sync for a profile group.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": { "type": "string" },
            "enabled": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["group_id", "enabled"]
        }),
      },
      McpTool {
        name: "sync_set_vpn_enabled".to_string(),
        description: "Toggle cloud sync for a VPN config.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": { "type": "string" },
            "enabled": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["vpn_id", "enabled"]
        }),
      },
      McpTool {
        name: "sync_set_extension_enabled".to_string(),
        description: "Toggle cloud sync for a browser extension.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "extension_id": { "type": "string" },
            "enabled": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["extension_id", "enabled"]
        }),
      },
      McpTool {
        name: "sync_set_extension_group_enabled".to_string(),
        description: "Toggle cloud sync for an extension group.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "extension_group_id": { "type": "string" },
            "enabled": { "type": "boolean" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["extension_group_id", "enabled"]
        }),
      },
      McpTool {
        name: "sync_request_profile".to_string(),
        description: "Queue a cloud sync for one profile now.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "sync_enable_all".to_string(),
        description: "Enable sync for all metadata entities (proxies, groups, VPNs, extensions).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "e2e_has_password".to_string(),
        description: "Check whether a sync encryption password is set.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "e2e_set_password".to_string(),
        description: "Set the sync encryption password. The secret is accepted on input, never echoed back.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "password": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["password"]
        }),
      },
      McpTool {
        name: "e2e_delete_password".to_string(),
        description: "Delete the sync encryption password.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "browser_versions".to_string(),
        description: "List downloaded browser versions for a browser (read-only).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "browser": { "type": "string", "description": "e.g. chromium" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["browser"]
        }),
      },
      McpTool {
        name: "browser_download_check".to_string(),
        description: "Check whether a browser version is downloaded (read-only).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "browser": { "type": "string" },
            "version": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["browser", "version"]
        }),
      },
      McpTool {
        name: "browser_missing_binaries".to_string(),
        description: "List profile browsers whose binaries are missing (read-only).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "traffic_get_all".to_string(),
        description: "Read live per-profile traffic snapshots (read-only).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "traffic_get_profile".to_string(),
        description: "Read the traffic snapshot for one profile (read-only).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "traffic_clear_profile".to_string(),
        description: "Securely erase traffic history for one profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string" },
            "responseLanguage": { "type": "string" }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "traffic_clear_all".to_string(),
        description: "Securely erase all traffic history.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "default_browser_check".to_string(),
        description: "Check whether Duckling is the OS default browser (read-only).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
      McpTool {
        name: "logs_read".to_string(),
        description: "Read the redacted application logs (read-only, size-capped).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "responseLanguage": { "type": "string" }
          },
          "required": []
        }),
      },
    ];
    // Browser interaction tools come from the shared catalog so the MCP
    // server, the in-app agent, and scheduled runs can never drift apart.
    tools.extend(
      crate::browser_tools::browser_tools()
        .into_iter()
        .map(|t| McpTool {
          name: t.name,
          description: t.description,
          input_schema: t.input_schema,
        }),
    );
    tools
  }

  async fn handle_initialize(
    &self,
    request: McpRequest,
  ) -> Result<(String, (serde_json::Value, serde_json::Value)), (serde_json::Value, McpError)> {
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);

    if !self.is_running() {
      return Err((
        id,
        McpError {
          code: -32001,
          message: "MCP server is not running".to_string(),
        },
      ));
    }

    // Create session
    let session_id = Uuid::new_v4().to_string();
    {
      let mut inner = self.inner.lock().await;
      inner
        .sessions
        .insert(session_id.clone(), McpSession { initialized: false });
    }

    let result = serde_json::json!({
      "protocolVersion": PROTOCOL_VERSION,
      "capabilities": {
        "tools": {
          "listChanged": false
        }
      },
      "serverInfo": {
        "name": SERVER_NAME,
        "version": SERVER_VERSION,
      },
      "instructions": "Duckling Browser MCP server. Use tools/list to discover available browser automation tools."
    });

    log::info!("[mcp] New session initialized: {}", session_id);
    Ok((session_id, (id, result)))
  }

  pub async fn handle_request(&self, request: McpRequest) -> McpResponse {
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);

    if !self.is_running() {
      return McpResponse {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        result: None,
        error: Some(McpError {
          code: -32001,
          message: "MCP server is not running".to_string(),
        }),
      };
    }

    let result = match request.method.as_str() {
      "ping" => Ok(serde_json::json!({})),
      "tools/list" => self.handle_tools_list().await,
      "tools/call" => self.handle_tool_call(request.params).await,
      _ => Err(McpError {
        code: -32601,
        message: format!("Method not found: {}", request.method),
      }),
    };

    match result {
      Ok(value) => McpResponse {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        result: Some(value),
        error: None,
      },
      Err(error) => McpResponse {
        jsonrpc: "2.0".to_string(),
        id: Some(id),
        result: None,
        error: Some(error),
      },
    }
  }

  async fn handle_tools_list(&self) -> Result<serde_json::Value, McpError> {
    Ok(serde_json::json!({
      "tools": self.get_tools()
    }))
  }

  async fn handle_tool_call(
    &self,
    params: Option<serde_json::Value>,
  ) -> Result<serde_json::Value, McpError> {
    let params = params.ok_or_else(|| McpError {
      code: -32602,
      message: "Missing parameters".to_string(),
    })?;

    let tool_name = params
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing tool name".to_string(),
      })?;

    let arguments = params
      .get("arguments")
      .cloned()
      .unwrap_or(serde_json::json!({}));

    // Surface the call in logs so customer reports show which tools the MCP
    // client is actually invoking (and therefore which gate any subsequent
    // error came from). Log only the tool name and the profile_id arg —
    // arbitrary URLs / JS / selectors can be sensitive.
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .unwrap_or("<none>");
    log::info!("[mcp] tools/call name={tool_name} profile_id={profile_id}");

    let started = std::time::Instant::now();
    let result = self.dispatch_tool_call(tool_name, &arguments).await;
    let elapsed_ms = started.elapsed().as_millis();
    match &result {
      Ok(_) => {
        log::info!(
          "[mcp] tools/call name={tool_name} profile_id={profile_id} -> ok ({elapsed_ms} ms)"
        );
      }
      Err(e) => {
        log::warn!(
          "[mcp] tools/call name={tool_name} profile_id={profile_id} -> error code={} msg={:?} ({elapsed_ms} ms)",
          e.code,
          e.message
        );
      }
    }
    result
  }

  pub(crate) async fn dispatch_tool_call(
    &self,
    tool_name: &str,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    match tool_name {
      "list_profiles" => self.handle_list_profiles().await,
      "get_profile" => self.handle_get_profile(arguments).await,
      "run_profile" => self.handle_run_profile(arguments).await,
      "kill_profile" => self.handle_kill_profile(arguments).await,
      "batch_run_profiles" => self.handle_batch_run_profiles(arguments).await,
      "batch_stop_profiles" => self.handle_batch_stop_profiles(arguments).await,
      "create_profile" => self.handle_create_profile(arguments).await,
      // Profile import
      "detect_browser_profiles" => self.handle_detect_browser_profiles(arguments).await,
      "import_browser_profiles" => self.handle_import_browser_profiles(arguments).await,
      "update_profile" => self.handle_update_profile(arguments).await,
      "delete_profile" => self.handle_delete_profile(arguments).await,
      "list_tags" => self.handle_list_tags().await,
      "list_proxies" => self.handle_list_proxies().await,
      "get_profile_status" => self.handle_get_profile_status(arguments).await,
      // Group management
      "list_groups" => self.handle_list_groups().await,
      "get_group" => self.handle_get_group(arguments).await,
      "create_group" => self.handle_create_group(arguments).await,
      "update_group" => self.handle_update_group(arguments).await,
      "delete_group" => self.handle_delete_group(arguments).await,
      "assign_profiles_to_group" => self.handle_assign_profiles_to_group(arguments).await,
      // Full proxy management
      "get_proxy" => self.handle_get_proxy(arguments).await,
      "create_proxy" => self.handle_create_proxy(arguments).await,
      "update_proxy" => self.handle_update_proxy(arguments).await,
      "delete_proxy" => self.handle_delete_proxy(arguments).await,
      // Proxy import/export
      "export_proxies" => self.handle_export_proxies(arguments).await,
      "import_proxies" => self.handle_import_proxies(arguments).await,
      // Proxy pool management
      "create_proxy_pool" => self.handle_create_proxy_pool(arguments).await,
      "list_proxy_pools" => self.handle_list_proxy_pools().await,
      "update_proxy_pool" => self.handle_update_proxy_pool(arguments).await,
      "delete_proxy_pool" => self.handle_delete_proxy_pool(arguments).await,
      "assign_profiles_to_pool" => self.handle_assign_profiles_to_pool(arguments).await,
      "rotate_profile_proxy" => self.handle_rotate_profile_proxy(arguments).await,
      // LLM completion
      "llm_completion" => self.handle_llm_completion(arguments).await,
      // Agent tools
      "agent_chat" => self.handle_agent_chat(arguments).await,
      "agent_chat_confirm" => self.handle_agent_chat_confirm(arguments).await,
      "agent_chat_decline" => self.handle_agent_chat_decline(arguments).await,
      // VPN management
      "import_vpn" => self.handle_import_vpn(arguments).await,
      "list_vpn_configs" => self.handle_list_vpn_configs().await,
      "delete_vpn" => self.handle_delete_vpn(arguments).await,
      "connect_vpn" => self.handle_connect_vpn(arguments).await,
      "disconnect_vpn" => self.handle_disconnect_vpn(arguments).await,
      "get_vpn_status" => self.handle_get_vpn_status(arguments).await,
      // Fingerprint management
      "get_profile_fingerprint" => self.handle_get_profile_fingerprint(arguments).await,
      "update_profile_fingerprint" => self.handle_update_profile_fingerprint(arguments).await,
      "update_profile_proxy_bypass_rules" => {
        self
          .handle_update_profile_proxy_bypass_rules(arguments)
          .await
      }
      // DNS blocklist management
      "update_profile_dns_blocklist" => self.handle_update_profile_dns_blocklist(arguments).await,
      "get_dns_blocklist_status" => self.handle_get_dns_blocklist_status().await,
      // Extension management
      "list_extensions" => self.handle_list_extensions().await,
      "list_extension_groups" => self.handle_list_extension_groups().await,
      "create_extension_group" => self.handle_create_extension_group(arguments).await,
      "delete_extension" => self.handle_delete_extension_mcp(arguments).await,
      "delete_extension_group" => self.handle_delete_extension_group_mcp(arguments).await,
      "assign_extension_group_to_profile" => {
        self
          .handle_assign_extension_group_to_profile(arguments)
          .await
      }
      // Cookie management
      "import_profile_cookies" => self.handle_import_profile_cookies(arguments).await,
      // Team lock tools
      "get_team_locks" => self.handle_get_team_locks().await,
      "get_team_lock_status" => self.handle_get_team_lock_status(arguments).await,
      // Synchronizer tools
      "start_sync_session" => self.handle_start_sync_session(arguments).await,
      "stop_sync_session" => self.handle_stop_sync_session(arguments).await,
      "get_sync_sessions" => self.handle_get_sync_sessions().await,
      "remove_sync_follower" => self.handle_remove_sync_follower(arguments).await,
      "clone_profile" => self.handle_clone_profile(arguments).await,
      "get_app_settings" => self.handle_get_app_settings().await,
      "update_app_settings" => self.handle_update_app_settings(arguments).await,
      "get_table_sorting" => self.handle_get_table_sorting().await,
      "update_table_sorting" => self.handle_update_table_sorting(arguments).await,
      "scheduler_list" => self.handle_scheduler_list().await,
      "scheduler_get" => self.handle_scheduler_get(arguments).await,
      "scheduler_save" => self.handle_scheduler_save(arguments).await,
      "scheduler_delete" => self.handle_scheduler_delete(arguments).await,
      "scheduler_set_enabled" => self.handle_scheduler_set_enabled(arguments).await,
      "scheduler_run_now" => self.handle_scheduler_run_now(arguments).await,
      "ai_keys_list" => self.handle_ai_keys_list().await,
      "ai_keys_save" => self.handle_ai_keys_save(arguments).await,
      "ai_keys_delete" => self.handle_ai_keys_delete(arguments).await,
      "ai_keys_test" => self.handle_ai_keys_test(arguments).await,
      "ai_keys_models_get" => self.handle_ai_keys_models_get(arguments).await,
      "subscription_list" => self.handle_subscription_list().await,
      "subscription_entries" => self.handle_subscription_entries(arguments).await,
      "subscription_save" => self.handle_subscription_save(arguments).await,
      "subscription_delete" => self.handle_subscription_delete(arguments).await,
      "subscription_refresh" => self.handle_subscription_refresh(arguments).await,
      "subscription_preview" => self.handle_subscription_preview(arguments).await,
      "dns_custom_get" => self.handle_dns_custom_get().await,
      "dns_custom_set" => self.handle_dns_custom_set(arguments).await,
      "dns_custom_import" => self.handle_dns_custom_import(arguments).await,
      "dns_custom_export" => self.handle_dns_custom_export(arguments).await,
      "dns_cache_status" => self.handle_dns_cache_status().await,
      "dns_refresh" => self.handle_dns_refresh().await,
      "extension_get" => self.handle_extension_get(arguments).await,
      "extension_add" => self.handle_extension_add(arguments).await,
      "extension_update" => self.handle_extension_update(arguments).await,
      "extension_update_group" => self.handle_extension_update_group(arguments).await,
      "extension_add_to_group" => self.handle_extension_add_to_group(arguments).await,
      "extension_remove_from_group" => self.handle_extension_remove_from_group(arguments).await,
      "extension_group_for_profile" => self.handle_extension_group_for_profile(arguments).await,
      "vpn_get" => self.handle_vpn_get(arguments).await,
      "vpn_update" => self.handle_vpn_update(arguments).await,
      "vpn_validate" => self.handle_vpn_validate(arguments).await,
      "vpn_batch_import" => self.handle_vpn_batch_import(arguments).await,
      "vpn_list_active" => self.handle_vpn_list_active().await,
      "vpn_create_manual" => self.handle_vpn_create_manual(arguments).await,
      "sync_settings_get" => self.handle_sync_settings_get(arguments).await,
      "sync_status" => self.handle_sync_status().await,
      "sync_set_proxy_enabled" => self.handle_sync_set_proxy_enabled(arguments).await,
      "sync_set_group_enabled" => self.handle_sync_set_group_enabled(arguments).await,
      "sync_set_vpn_enabled" => self.handle_sync_set_vpn_enabled(arguments).await,
      "sync_set_extension_enabled" => self.handle_sync_set_extension_enabled(arguments).await,
      "sync_set_extension_group_enabled" => {
        self
          .handle_sync_set_extension_group_enabled(arguments)
          .await
      }
      "sync_request_profile" => self.handle_sync_request_profile(arguments).await,
      "sync_enable_all" => self.handle_sync_enable_all().await,
      "e2e_has_password" => self.handle_e2e_has_password().await,
      "e2e_set_password" => self.handle_e2e_set_password(arguments).await,
      "e2e_delete_password" => self.handle_e2e_delete_password().await,
      "browser_versions" => self.handle_browser_versions(arguments).await,
      "browser_download_check" => self.handle_browser_download_check(arguments).await,
      "browser_missing_binaries" => self.handle_browser_missing_binaries().await,
      "traffic_get_all" => self.handle_traffic_get_all().await,
      "traffic_get_profile" => self.handle_traffic_get_profile(arguments).await,
      "traffic_clear_profile" => self.handle_traffic_clear_profile(arguments).await,
      "traffic_clear_all" => self.handle_traffic_clear_all().await,
      "default_browser_check" => self.handle_default_browser_check().await,
      "logs_read" => self.handle_logs_read().await,
      // Browser interaction tools
      "navigate" => self.handle_navigate(arguments).await,
      "screenshot" => self.handle_screenshot(arguments).await,
      "evaluate_javascript" => self.handle_evaluate_javascript(arguments).await,
      "click_element" => self.handle_click_element(arguments).await,
      "type_text" => self.handle_type_text(arguments).await,
      "get_page_content" => self.handle_get_page_content(arguments).await,
      "get_page_info" => self.handle_get_page_info(arguments).await,
      "get_interactive_elements" => self.handle_get_interactive_elements(arguments).await,
      "click_by_index" => self.handle_click_by_index(arguments).await,
      "type_by_index" => self.handle_type_by_index(arguments).await,
      "drag" => self.handle_drag(arguments).await,
      "scroll" => self.handle_scroll(arguments).await,
      "press_key" => self.handle_press_key(arguments).await,
      "hover" => self.handle_hover(arguments).await,
      "set_download_dir" => self.handle_set_download_dir(arguments).await,
      "wait_for_download" => self.handle_wait_for_download(arguments).await,
      "get_downloads" => self.handle_get_downloads(arguments).await,
      "list_tabs" => self.handle_list_tabs(arguments).await,
      "new_tab" => self.handle_new_tab(arguments).await,
      "switch_tab" => self.handle_switch_tab(arguments).await,
      "close_tab" => self.handle_close_tab(arguments).await,
      "wait_for_text" => self.handle_wait_for_text(arguments).await,
      "wait_for_url" => self.handle_wait_for_url(arguments).await,
      "select_option" => self.handle_select_option(arguments).await,
      "find_text" => self.handle_find_text(arguments).await,
      "get_cookies" => self.handle_get_cookies(arguments).await,
      "extract_table" => self.handle_extract_table(arguments).await,
      "extract_article" => self.handle_extract_article(arguments).await,
      _ => Err(McpError {
        code: -32602,
        message: format!("Unknown tool: {tool_name}"),
      }),
    }
  }

  async fn handle_list_profiles(&self) -> Result<serde_json::Value, McpError> {
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    // Filter to only Chromium profiles
    let filtered: Vec<&BrowserProfile> = profiles
      .iter()
      .filter(|p| p.browser == "chromium")
      .collect();

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&filtered).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    // Check if it's a Chromium profile
    if profile.browser != "chromium" {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium profiles".to_string(),
      });
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&profile).unwrap_or_default()
      }]
    }))
  }

  async fn handle_run_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let url = arguments.get("url").and_then(|v| v.as_str());
    let headless = arguments
      .get("headless")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    // Get the profile
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    // Check if it's a Chromium profile
    if profile.browser != "chromium" {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium profiles".to_string(),
      });
    }

    // Team lock check
    crate::team_lock::acquire_team_lock_if_needed(profile)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;

    // Get app handle to launch
    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    // Launch a fresh instance, honoring the requested headless mode. The CDP
    // port is self-allocated and discovered later via get_cdp_port_for_profile.
    // Pool members fail over to the next live member on launch failure.
    crate::proxy_pool::launch_browser_profile_with_pool_failover(
      app_handle.clone(),
      profile.clone(),
      url.map(|s| s.to_string()),
      None,
      headless,
      true,
    )
    .await
    .map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to launch browser: {e}"),
    })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Browser profile '{}' launched successfully", profile.name)
      }]
    }))
  }

  async fn handle_kill_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    // Get the profile
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    // Check if it's a Chromium profile
    if profile.browser != "chromium" {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium profiles".to_string(),
      });
    }

    // Get app handle to kill
    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    // Kill the browser
    crate::browser_runner::BrowserRunner::instance()
      .kill_browser_process(app_handle.clone(), profile)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to kill browser: {e}"),
      })?;

    crate::team_lock::release_team_lock_if_needed(profile).await;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Browser profile '{}' stopped successfully", profile.name)
      }]
    }))
  }

  async fn handle_batch_run_profiles(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_ids: Vec<String> = arguments
      .get("profile_ids")
      .and_then(|v| v.as_array())
      .map(|a| {
        a.iter()
          .filter_map(|v| v.as_str().map(|s| s.to_string()))
          .collect()
      })
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_ids array".to_string(),
      })?;

    let url = arguments.get("url").and_then(|v| v.as_str());
    let headless = arguments
      .get("headless")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    // Clone the app handle and release the lock before the launch loop so we
    // never hold the inner mutex across the per-profile awaits.
    let app_handle = {
      let inner = self.inner.lock().await;
      inner
        .app_handle
        .as_ref()
        .ok_or_else(|| McpError {
          code: -32000,
          message: "MCP server not properly initialized".to_string(),
        })?
        .clone()
    };

    let lines: Vec<String> = futures_util::stream::iter(profile_ids.clone())
      .map(|profile_id| {
        let profiles = profiles.clone();
        let app_handle = app_handle.clone();
        let url = url.map(|s| s.to_string());
        async move {
          let Some(profile) = profiles.iter().find(|p| p.id.to_string() == *profile_id) else {
            return format!("{profile_id}: not found");
          };
          if profile.browser != "chromium" {
            return format!("{profile_id}: unsupported browser (MCP supports Chromium)");
          }
          if let Err(e) = crate::team_lock::acquire_team_lock_if_needed(profile).await {
            return format!("{profile_id}: {e}");
          }
          match crate::proxy_pool::launch_browser_profile_with_pool_failover(
            app_handle,
            profile.clone(),
            url,
            None,
            headless,
            true,
          )
          .await
          {
            Ok(_) => format!("{}: launched", profile.name),
            Err(e) => format!("{}: launch failed: {e}", profile.name),
          }
        }
      })
      .buffered(profile_ids.len().max(1))
      .collect()
      .await;

    let launched = lines
      .iter()
      .filter(|line| line.ends_with(": launched"))
      .count();
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Launched {}/{} profile(s):\n{}", launched, profile_ids.len(), lines.join("\n"))
      }]
    }))
  }

  async fn handle_batch_stop_profiles(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_ids: Vec<String> = arguments
      .get("profile_ids")
      .and_then(|v| v.as_array())
      .map(|a| {
        a.iter()
          .filter_map(|v| v.as_str().map(|s| s.to_string()))
          .collect()
      })
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_ids array".to_string(),
      })?;

    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let app_handle = {
      let inner = self.inner.lock().await;
      inner
        .app_handle
        .as_ref()
        .ok_or_else(|| McpError {
          code: -32000,
          message: "MCP server not properly initialized".to_string(),
        })?
        .clone()
    };

    let mut stopped = 0usize;
    let mut lines: Vec<String> = Vec::with_capacity(profile_ids.len());
    for profile_id in &profile_ids {
      let Some(profile) = profiles.iter().find(|p| p.id.to_string() == *profile_id) else {
        lines.push(format!("{profile_id}: not found"));
        continue;
      };
      match crate::browser_runner::BrowserRunner::instance()
        .kill_browser_process(app_handle.clone(), profile)
        .await
      {
        Ok(_) => {
          crate::team_lock::release_team_lock_if_needed(profile).await;
          stopped += 1;
          lines.push(format!("{}: stopped", profile.name));
        }
        Err(e) => lines.push(format!("{}: stop failed: {e}", profile.name)),
      }
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Stopped {}/{} profile(s):\n{}", stopped, profile_ids.len(), lines.join("\n"))
      }]
    }))
  }

  async fn handle_create_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;
    let browser = arguments
      .get("browser")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing browser".to_string(),
      })?;

    if browser != "chromium" {
      return Err(McpError {
        code: -32602,
        message: "browser must be 'Chromium'".to_string(),
      });
    }

    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    if proxy_id.is_some() && vpn_id.is_some() {
      return Err(McpError {
        code: -32602,
        message: "Cannot set both proxy_id and vpn_id".to_string(),
      });
    }
    let launch_hook = arguments
      .get("launch_hook")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    let tags: Option<Vec<String>> = arguments.get("tags").and_then(|v| {
      v.as_array().map(|arr| {
        arr
          .iter()
          .filter_map(|item| item.as_str().map(|s| s.to_string()))
          .collect()
      })
    });
    let dns_blocklist = arguments
      .get("dns_blocklist")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let ephemeral = arguments
      .get("ephemeral")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    let clear_on_close = arguments
      .get("clear_on_close")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    if ephemeral && clear_on_close {
      return Err(McpError {
        code: -32602,
        message: "clear_on_close is unavailable for ephemeral profiles".to_string(),
      });
    }

    // Pick the latest downloaded version for this browser
    let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
    let versions = registry.get_downloaded_versions(browser);
    let version = versions.first().ok_or_else(|| McpError {
      code: -32000,
      message: format!("No downloaded version found for {browser}. Download it first."),
    })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;
    let app_handle_owned = app_handle.clone();
    drop(inner);

    crate::validate_profile_network(proxy_id.as_deref(), vpn_id.as_deref())
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to create profile: {e}"),
      })?;

    let mut profile = ProfileManager::instance()
      .create_profile_with_group(
        &app_handle_owned,
        name,
        browser,
        version,
        "stable",
        proxy_id,
        vpn_id,
        None,
        group_id,
        ephemeral,
        dns_blocklist,
        launch_hook,
      )
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to create profile: {e}"),
      })?;
    let profile_id_str = profile.id.to_string();

    if let Some(tags) = tags {
      ProfileManager::instance()
        .update_profile_tags(&app_handle_owned, &profile_id_str, tags.clone())
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update tags: {e}"),
        })?;
      profile.tags = tags;
      if let Ok(profiles) = ProfileManager::instance().list_profiles() {
        let _ = crate::tag_manager::TAG_MANAGER
          .lock()
          .map(|manager| manager.rebuild_from_profiles(&profiles));
      }
    }
    if let Some(note) = arguments.get("note").and_then(|v| v.as_str()) {
      let normalized = if note.is_empty() {
        None
      } else {
        Some(note.to_string())
      };
      ProfileManager::instance()
        .update_profile_note(&app_handle_owned, &profile_id_str, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update note: {e}"),
        })?;
    }
    if let Some(color) = arguments.get("window_color").and_then(|v| v.as_str()) {
      let normalized = if color.is_empty() {
        None
      } else {
        Some(color.to_string())
      };
      ProfileManager::instance()
        .update_profile_window_color(&app_handle_owned, &profile_id_str, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update window color: {e}"),
        })?;
    }
    if let Some(dir) = arguments.get("download_dir").and_then(|v| v.as_str()) {
      let normalized = if dir.is_empty() {
        None
      } else {
        Some(dir.to_string())
      };
      ProfileManager::instance()
        .update_profile_download_dir(&app_handle_owned, &profile_id_str, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update download dir: {e}"),
        })?;
    }
    if let Some(allow) = arguments
      .get("allow_agent_downloads")
      .and_then(|v| v.as_bool())
    {
      ProfileManager::instance()
        .update_profile_allow_agent_downloads(&app_handle_owned, &profile_id_str, allow)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update agent downloads: {e}"),
        })?;
    }
    if let Some(approve) = arguments
      .get("agent_auto_approve")
      .and_then(|v| v.as_bool())
    {
      ProfileManager::instance()
        .update_profile_agent_auto_approve(&app_handle_owned, &profile_id_str, approve)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update agent auto-approve: {e}"),
        })?;
    }
    if arguments.get("agent_key_id").is_some() || arguments.get("agent_id").is_some() {
      let key_id = arguments
        .get("agent_key_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
      let agent_id = arguments
        .get("agent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
      ProfileManager::instance()
        .update_profile_agent_pair(&app_handle_owned, &profile_id_str, key_id, agent_id)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update agent pair: {e}"),
        })?;
    }
    if clear_on_close {
      ProfileManager::instance()
        .update_profile_clear_on_close(&app_handle_owned, &profile_id_str, true)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update clear-on-close: {e}"),
        })?;
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Profile '{}' created (id: {})", profile.name, profile.id)
      }]
    }))
  }

  async fn handle_update_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;
    let pm = ProfileManager::instance();

    if let Some(new_name) = arguments.get("name").and_then(|v| v.as_str()) {
      pm.rename_profile(app_handle, profile_id, new_name)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to rename profile: {e}"),
        })?;
    }

    if let Some(proxy_id) = arguments.get("proxy_id").and_then(|v| v.as_str()) {
      let pid = if proxy_id.is_empty() {
        None
      } else {
        Some(proxy_id.to_string())
      };
      pm.update_profile_proxy(app_handle.clone(), profile_id, pid)
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update proxy: {e}"),
        })?;
    }

    if let Some(launch_hook) = arguments.get("launch_hook").and_then(|v| v.as_str()) {
      let normalized = if launch_hook.is_empty() {
        None
      } else {
        Some(launch_hook.to_string())
      };
      pm.update_profile_launch_hook(app_handle, profile_id, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update launch hook: {e}"),
        })?;
    }

    if let Some(group_id) = arguments.get("group_id").and_then(|v| v.as_str()) {
      let gid = if group_id.is_empty() {
        None
      } else {
        Some(group_id.to_string())
      };
      pm.assign_profiles_to_group(app_handle, vec![profile_id.to_string()], gid)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update group: {e}"),
        })?;
    }

    if let Some(tags) = arguments.get("tags").and_then(|v| v.as_array()) {
      let tag_list: Vec<String> = tags
        .iter()
        .filter_map(|item| item.as_str().map(|s| s.to_string()))
        .collect();
      pm.update_profile_tags(app_handle, profile_id, tag_list)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update tags: {e}"),
        })?;
      if let Ok(profiles) = pm.list_profiles() {
        let _ = crate::tag_manager::TAG_MANAGER
          .lock()
          .map(|manager| manager.rebuild_from_profiles(&profiles));
      }
    }

    if let Some(ext_group_id) = arguments.get("extension_group_id").and_then(|v| v.as_str()) {
      let eid = if ext_group_id.is_empty() {
        None
      } else {
        Some(ext_group_id.to_string())
      };
      pm.update_profile_extension_group(profile_id, eid)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update extension group: {e}"),
        })?;
    }

    if let Some(rules) = arguments
      .get("proxy_bypass_rules")
      .and_then(|v| v.as_array())
    {
      let rule_list: Vec<String> = rules
        .iter()
        .filter_map(|item| item.as_str().map(|s| s.to_string()))
        .collect();
      pm.update_profile_proxy_bypass_rules(app_handle, profile_id, rule_list)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update proxy bypass rules: {e}"),
        })?;
    }

    if let Some(clear_on_close) = arguments.get("clear_on_close").and_then(|v| v.as_bool()) {
      pm.update_profile_clear_on_close(app_handle, profile_id, clear_on_close)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update clear-on-close: {e}"),
        })?;
    }

    if let Some(vpn_id) = arguments.get("vpn_id").and_then(|v| v.as_str()) {
      let vid = if vpn_id.is_empty() {
        None
      } else {
        Some(vpn_id.to_string())
      };
      pm.update_profile_vpn(app_handle.clone(), profile_id, vid)
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update vpn: {e}"),
        })?;
    }
    if let Some(version) = arguments.get("version").and_then(|v| v.as_str()) {
      pm.update_profile_version(app_handle, profile_id, version)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update version: {e}"),
        })?;
    }
    if let Some(note) = arguments.get("note").and_then(|v| v.as_str()) {
      let normalized = if note.is_empty() {
        None
      } else {
        Some(note.to_string())
      };
      pm.update_profile_note(app_handle, profile_id, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update note: {e}"),
        })?;
    }
    if let Some(color) = arguments.get("window_color").and_then(|v| v.as_str()) {
      let normalized = if color.is_empty() {
        None
      } else {
        Some(color.to_string())
      };
      pm.update_profile_window_color(app_handle, profile_id, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update window color: {e}"),
        })?;
    }
    if let Some(dir) = arguments.get("download_dir").and_then(|v| v.as_str()) {
      let normalized = if dir.is_empty() {
        None
      } else {
        Some(dir.to_string())
      };
      pm.update_profile_download_dir(app_handle, profile_id, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update download dir: {e}"),
        })?;
    }
    if let Some(allow) = arguments
      .get("allow_agent_downloads")
      .and_then(|v| v.as_bool())
    {
      pm.update_profile_allow_agent_downloads(app_handle, profile_id, allow)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update agent downloads: {e}"),
        })?;
    }
    if let Some(approve) = arguments
      .get("agent_auto_approve")
      .and_then(|v| v.as_bool())
    {
      pm.update_profile_agent_auto_approve(app_handle, profile_id, approve)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update agent auto-approve: {e}"),
        })?;
    }
    if arguments.get("agent_key_id").is_some() || arguments.get("agent_id").is_some() {
      let key_id = arguments
        .get("agent_key_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
      let agent_id = arguments
        .get("agent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
      pm.update_profile_agent_pair(app_handle, profile_id, key_id, agent_id)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update agent pair: {e}"),
        })?;
    }
    if let Some(mode) = arguments.get("sync_mode").and_then(|v| v.as_str()) {
      let inner = self.inner.lock().await;
      let sync_handle = inner
        .app_handle
        .as_ref()
        .ok_or_else(|| McpError {
          code: -32000,
          message: "MCP server not properly initialized".to_string(),
        })?
        .clone();
      drop(inner);
      crate::sync::set_profile_sync_mode(sync_handle, profile_id.to_string(), mode.to_string())
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update sync mode: {e}"),
        })?;
    }
    if let Some(level) = arguments.get("dns_blocklist").and_then(|v| v.as_str()) {
      let normalized = if level.is_empty() || level == "none" {
        None
      } else {
        Some(level.to_string())
      };
      pm.update_profile_dns_blocklist(profile_id, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update dns blocklist: {e}"),
        })?;
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Profile '{profile_id}' updated successfully")
      }]
    }))
  }

  async fn handle_delete_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    ProfileManager::instance()
      .delete_profile(app_handle, profile_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete profile: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Profile '{profile_id}' deleted successfully")
      }]
    }))
  }

  async fn handle_list_tags(&self) -> Result<serde_json::Value, McpError> {
    let tags = crate::tag_manager::TAG_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to access tag manager: {e}"),
      })?
      .get_all_tags()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to get tags: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&tags).unwrap_or_default()
      }]
    }))
  }

  async fn handle_list_proxies(&self) -> Result<serde_json::Value, McpError> {
    let proxies = PROXY_MANAGER.get_stored_proxies();

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&proxies).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_profile_status(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    // Get the profile
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    // Check if it's a Chromium profile
    if profile.browser != "chromium" {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium profiles".to_string(),
      });
    }

    let is_running = profile.process_id.is_some();

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::json!({
          "profile_id": profile_id,
          "is_running": is_running
        }).to_string()
      }]
    }))
  }

  // Group management handlers
  async fn handle_list_groups(&self) -> Result<serde_json::Value, McpError> {
    let groups = GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .get_all_groups()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list groups: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&groups).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;

    let groups = GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .get_all_groups()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list groups: {e}"),
      })?;

    let group = groups
      .iter()
      .find(|g| g.id == group_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Group not found: {group_id}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&group).unwrap_or_default()
      }]
    }))
  }

  async fn handle_create_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let group = GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .create_group(app_handle, name.to_string())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to create group: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Group '{}' created successfully with ID: {}", group.name, group.id)
      }]
    }))
  }

  async fn handle_update_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;

    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let group = GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .update_group(app_handle, group_id.to_string(), name.to_string())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update group: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Group '{}' updated successfully", group.name)
      }]
    }))
  }

  async fn handle_delete_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .delete_group(app_handle, group_id.to_string())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete group: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Group '{}' deleted successfully", group_id)
      }]
    }))
  }

  async fn handle_assign_profiles_to_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_ids: Vec<String> = arguments
      .get("profile_ids")
      .and_then(|v| v.as_array())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_ids".to_string(),
      })?
      .iter()
      .filter_map(|v| v.as_str().map(|s| s.to_string()))
      .collect();

    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    ProfileManager::instance()
      .assign_profiles_to_group(app_handle, profile_ids.clone(), group_id.clone())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to assign profiles to group: {e}"),
      })?;

    let group_name = group_id.as_deref().unwrap_or("default");
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("{} profile(s) assigned to group '{}'", profile_ids.len(), group_name)
      }]
    }))
  }

  // Full proxy management handlers
  async fn handle_get_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_id".to_string(),
      })?;

    let proxies = PROXY_MANAGER.get_stored_proxies();
    let proxy = proxies
      .iter()
      .find(|p| p.id == proxy_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Proxy not found: {proxy_id}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&proxy).unwrap_or_default()
      }]
    }))
  }

  async fn handle_create_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let proxy_type = arguments
      .get("proxy_type")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_type".to_string(),
      })?;

    // The tool schema declares an enum, but JSON-Schema enums are advisory only;
    // enforce it here so a bad value can't produce a non-functional proxy.
    if !matches!(proxy_type, "http" | "https" | "socks4" | "socks5") {
      return Err(McpError {
        code: -32602,
        message: "proxy_type must be one of: http, https, socks4, socks5".to_string(),
      });
    }

    let host = arguments
      .get("host")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing host".to_string(),
      })?;

    let port = arguments
      .get("port")
      .and_then(|v| v.as_u64())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing port".to_string(),
      })? as u16;

    let username = arguments
      .get("username")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let password = arguments
      .get("password")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let proxy_settings = ProxySettings {
      proxy_type: proxy_type.to_string(),
      host: host.to_string(),
      port,
      username,
      password,
    };

    let proxy = PROXY_MANAGER
      .create_stored_proxy(app_handle, name.to_string(), proxy_settings)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to create proxy: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Proxy '{}' created successfully with ID: {}", proxy.name, proxy.id)
      }]
    }))
  }

  async fn handle_update_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_id".to_string(),
      })?;

    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    // Build proxy_settings if any settings fields are provided
    let has_settings = arguments.get("proxy_type").is_some()
      || arguments.get("host").is_some()
      || arguments.get("port").is_some();

    let proxy_settings = if has_settings {
      // Get existing proxy to use as defaults
      let proxies = PROXY_MANAGER.get_stored_proxies();
      let existing = proxies
        .iter()
        .find(|p| p.id == proxy_id)
        .ok_or_else(|| McpError {
          code: -32000,
          message: format!("Proxy not found: {proxy_id}"),
        })?;

      let proxy_type = arguments
        .get("proxy_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| existing.proxy_settings.proxy_type.clone());

      let host = arguments
        .get("host")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| existing.proxy_settings.host.clone());

      let port = arguments
        .get("port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16)
        .unwrap_or(existing.proxy_settings.port);

      let username = arguments
        .get("username")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| existing.proxy_settings.username.clone());

      let password = arguments
        .get("password")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| existing.proxy_settings.password.clone());

      Some(ProxySettings {
        proxy_type,
        host,
        port,
        username,
        password,
      })
    } else {
      None
    };

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let proxy = PROXY_MANAGER
      .update_stored_proxy(app_handle, proxy_id, name, proxy_settings)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update proxy: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Proxy '{}' updated successfully", proxy.name)
      }]
    }))
  }

  async fn handle_delete_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_id".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    PROXY_MANAGER
      .delete_stored_proxy(app_handle, proxy_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete proxy: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Proxy '{}' deleted successfully", proxy_id)
      }]
    }))
  }

  async fn handle_export_proxies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing format".to_string(),
      })?;

    let content = match format {
      "json" => PROXY_MANAGER.export_proxies_json().map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to export proxies: {e}"),
      })?,
      "txt" => PROXY_MANAGER.export_proxies_txt(),
      _ => {
        return Err(McpError {
          code: -32602,
          message: format!("Invalid format '{}', must be 'json' or 'txt'", format),
        })
      }
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": content
      }]
    }))
  }

  async fn handle_import_proxies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let content = arguments
      .get("content")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing content".to_string(),
      })?;

    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing format".to_string(),
      })?;

    let name_prefix = arguments
      .get("name_prefix")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let result = match format {
      "json" => PROXY_MANAGER
        .import_proxies_json(app_handle, content)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to import proxies: {e}"),
        })?,
      "txt" => {
        use crate::proxy_manager::{ProxyManager, ProxyParseResult};

        let parse_results = ProxyManager::parse_txt_proxies(content);
        let parsed: Vec<_> = parse_results
          .into_iter()
          .filter_map(|r| {
            if let ProxyParseResult::Parsed(p) = r {
              Some(p)
            } else {
              None
            }
          })
          .collect();

        if parsed.is_empty() {
          return Err(McpError {
            code: -32000,
            message: "No valid proxies found in content".to_string(),
          });
        }

        PROXY_MANAGER
          .import_proxies_from_parsed(app_handle, parsed, name_prefix)
          .map_err(|e| McpError {
            code: -32000,
            message: format!("Failed to import proxies: {e}"),
          })?
      }
      _ => {
        return Err(McpError {
          code: -32602,
          message: format!("Invalid format '{}', must be 'json' or 'txt'", format),
        })
      }
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Import complete: {} imported, {} skipped, {} errors",
          result.imported_count,
          result.skipped_count,
          result.errors.len()
        )
      }]
    }))
  }

  // Profile import handlers
  async fn handle_detect_browser_profiles(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let importer = crate::profile_importer::ProfileImporter::instance();
    let profiles = match arguments.get("folder").and_then(|v| v.as_str()) {
      Some(folder) => importer.scan_folder(std::path::Path::new(folder)),
      None => importer.detect_existing_profiles(),
    }
    .map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to detect profiles: {e}"),
    })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&profiles).unwrap_or_else(|_| "[]".to_string())
      }]
    }))
  }

  async fn handle_import_browser_profiles(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let items: Vec<crate::profile_importer::ImportProfileItem> = arguments
      .get("items")
      .cloned()
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing items".to_string(),
      })
      .and_then(|v| {
        serde_json::from_value(v).map_err(|e| McpError {
          code: -32602,
          message: format!("Invalid items: {e}"),
        })
      })?;

    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let duplicate_strategy = arguments
      .get("duplicate_strategy")
      .cloned()
      .map(serde_json::from_value::<crate::profile_importer::DuplicateStrategy>)
      .transpose()
      .map_err(|e| McpError {
        code: -32602,
        message: format!("Invalid duplicate_strategy: {e}"),
      })?
      .unwrap_or_default();

    // Clone the handle instead of holding the inner lock across a potentially
    // multi-GB copy.
    let app_handle = {
      let inner = self.inner.lock().await;
      inner.app_handle.clone().ok_or_else(|| McpError {
        code: -32000,
        message: "MCP server not properly initialized".to_string(),
      })?
    };

    let result = crate::profile_importer::ProfileImporter::instance()
      .import_profiles(&app_handle, items, group_id, duplicate_strategy, None)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to import profiles: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Import complete: {} imported, {} skipped, {} failed\n{}",
          result.imported_count,
          result.skipped_count,
          result.failed_count,
          serde_json::to_string_pretty(&result.results).unwrap_or_default()
        )
      }]
    }))
  }

  // Cookie management handlers
  async fn handle_import_profile_cookies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let content = arguments
      .get("content")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing content".to_string(),
      })?;

    let app_handle = {
      let inner = self.inner.lock().await;
      inner
        .app_handle
        .as_ref()
        .ok_or_else(|| McpError {
          code: -32000,
          message: "MCP server not properly initialized".to_string(),
        })?
        .clone()
    };

    let result =
      crate::cookie_manager::CookieManager::import_cookies(&app_handle, profile_id, content)
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to import cookies: {e}"),
        })?;

    if let Some(scheduler) = crate::sync::get_global_scheduler() {
      let profile_manager = crate::profile::manager::ProfileManager::instance();
      if let Ok(profiles) = profile_manager.list_profiles() {
        if let Some(profile) = profiles.iter().find(|p| p.id.to_string() == profile_id) {
          if profile.is_sync_enabled() {
            let pid = profile_id.to_string();
            tauri::async_runtime::spawn(async move {
              scheduler.queue_profile_sync(pid).await;
            });
          }
        }
      }
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Import complete: {} imported, {} replaced, {} parse error(s)",
          result.cookies_imported,
          result.cookies_replaced,
          result.errors.len()
        )
      }]
    }))
  }

  // Proxy pool management handlers
  async fn handle_create_proxy_pool(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?
      .to_string();
    let proxy_ids: Vec<String> = arguments
      .get("proxy_ids")
      .and_then(|v| v.as_array())
      .map(|a| {
        a.iter()
          .filter_map(|v| v.as_str().map(|s| s.to_string()))
          .collect()
      })
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_ids array".to_string(),
      })?;
    let pool = crate::proxy_pool::PROXY_POOL_MANAGER
      .create_pool(name, proxy_ids)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to create proxy pool: {e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Proxy pool '{}' created with {} member(s): {}",
          pool.name,
          pool.proxy_ids.len(),
          pool.proxy_ids.join(", ")
        )
      }]
    }))
  }

  async fn handle_list_proxy_pools(&self) -> Result<serde_json::Value, McpError> {
    let pools = crate::proxy_pool::PROXY_POOL_MANAGER.list_pools();
    let text = if pools.is_empty() {
      "No proxy pools defined".to_string()
    } else {
      pools
        .iter()
        .map(|p| {
          format!(
            "{} (id: {}, {} member(s): {})",
            p.name,
            p.id,
            p.proxy_ids.len(),
            p.proxy_ids.join(", ")
          )
        })
        .collect::<Vec<_>>()
        .join("\n")
    };
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": text }]
    }))
  }

  async fn handle_update_proxy_pool(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let pool_id = arguments
      .get("pool_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing pool_id".to_string(),
      })?
      .to_string();
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?
      .to_string();
    let proxy_ids: Vec<String> = arguments
      .get("proxy_ids")
      .and_then(|v| v.as_array())
      .map(|a| {
        a.iter()
          .filter_map(|v| v.as_str().map(|s| s.to_string()))
          .collect()
      })
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_ids array".to_string(),
      })?;
    let pool = crate::proxy_pool::PROXY_POOL_MANAGER
      .update_pool(pool_id, name, proxy_ids)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update proxy pool: {e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Proxy pool '{}' updated with {} member(s)",
          pool.name,
          pool.proxy_ids.len()
        )
      }]
    }))
  }

  async fn handle_delete_proxy_pool(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let pool_id = arguments
      .get("pool_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing pool_id".to_string(),
      })?
      .to_string();
    crate::proxy_pool::PROXY_POOL_MANAGER
      .delete_pool(&pool_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete proxy pool: {e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Proxy pool {pool_id} deleted") }]
    }))
  }

  async fn handle_assign_profiles_to_pool(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let pool_id = arguments
      .get("pool_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing pool_id".to_string(),
      })?
      .to_string();
    let profile_ids: Vec<String> = arguments
      .get("profile_ids")
      .and_then(|v| v.as_array())
      .map(|a| {
        a.iter()
          .filter_map(|v| v.as_str().map(|s| s.to_string()))
          .collect()
      })
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_ids array".to_string(),
      })?;

    let app_handle = {
      let inner = self.inner.lock().await;
      inner
        .app_handle
        .as_ref()
        .ok_or_else(|| McpError {
          code: -32000,
          message: "MCP server not properly initialized".to_string(),
        })?
        .clone()
    };

    let results = crate::proxy_pool::PROXY_POOL_MANAGER
      .assign_profiles_to_pool(&app_handle, pool_id, profile_ids)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to assign profiles: {e}"),
      })?;

    let lines: Vec<String> = results
      .iter()
      .map(|r| {
        if r.ok {
          format!(
            "{}: assigned proxy {}",
            r.profile_id,
            r.proxy_id.clone().unwrap_or_default()
          )
        } else {
          format!(
            "{}: failed ({})",
            r.profile_id,
            r.error
              .clone()
              .unwrap_or_else(|| "unknown error".to_string())
          )
        }
      })
      .collect();
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": lines.join("\n") }]
    }))
  }

  async fn handle_rotate_profile_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?.to_string();

    let app_handle = {
      let inner = self.inner.lock().await;
      inner
        .app_handle
        .as_ref()
        .ok_or_else(|| McpError {
          code: -32000,
          message: "MCP server not properly initialized".to_string(),
        })?
        .clone()
    };

    let rotated = crate::proxy_pool::PROXY_POOL_MANAGER
      .rotate_profile_proxy(&app_handle, &profile_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to rotate profile proxy: {e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Profile {} rotated to proxy {} ({}://{}:{})",
          profile_id,
          rotated.id,
          rotated.proxy_settings.proxy_type,
          rotated.proxy_settings.host,
          rotated.proxy_settings.port
        )
      }]
    }))
  }

  async fn handle_llm_completion(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    if crate::llm_rate_limiter::check_llm_rate_limit().is_limited() {
      return Err(McpError {
        code: -32000,
        message: "LLM request quota exceeded; try again later".to_string(),
      });
    }

    let mut messages: Vec<crate::llm::ChatMessage> = arguments
      .get("messages")
      .cloned()
      .and_then(|v| serde_json::from_value(v).ok())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing or invalid messages".to_string(),
      })?;
    if let Some(lang) = arguments
      .get("responseLanguage")
      .and_then(|v| v.as_str())
      .filter(|s| !s.trim().is_empty())
    {
      messages.insert(0, crate::llm::ChatMessage::text("system", format!("Respond in {lang} (BCP-47). Tool names, arguments, and JSON stay English; only the human-readable reply localizes.")));
    }
    let request = crate::llm_completion::LlmCompletionRequest {
      key_id: arguments
        .get("key_id")
        .and_then(|v| v.as_str())
        .map(String::from),
      provider: arguments
        .get("provider")
        .and_then(|v| v.as_str())
        .map(String::from),
      model: arguments
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from),
      messages,
      tools: arguments
        .get("tools")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok()),
      max_retries: arguments
        .get("max_retries")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32),
    };

    let result = crate::llm_completion::run_llm_completion(request)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("LLM completion failed: {e}"),
      })?;

    let usage = result
      .usage
      .map(|u| {
        format!(
          " ({} prompt + {} completion tokens)",
          u.prompt_tokens, u.completion_tokens
        )
      })
      .unwrap_or_default();
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Provider: {}\nModel: {}\n\n{}{}",
          result.provider, result.model, result.reply, usage
        )
      }]
    }))
  }

  async fn handle_agent_chat(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let mut message = arguments
      .get("message")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing message".to_string(),
      })?
      .to_string();
    if let Some(lang) = arguments
      .get("responseLanguage")
      .and_then(|v| v.as_str())
      .filter(|s| !s.trim().is_empty())
    {
      message =
        format!("[Reply in {lang}. Tool names, arguments, and JSON stay English.]\n{message}");
    }
    let key_id = arguments
      .get("key_id")
      .and_then(|v| v.as_str())
      .map(String::from);
    let model = arguments
      .get("model")
      .and_then(|v| v.as_str())
      .map(String::from);
    let use_agent = arguments
      .get("use_agent")
      .and_then(|v| v.as_str())
      .map(String::from);
    let auto_approve = arguments
      .get("auto_approve")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    // Boxed to break the async recursion cycle (agent_chat -> run_tool_call ->
    // dispatch_tool_call -> handle_agent_chat). The in-app agent rejects
    // agent_chat via agent_tools(), so only external MCP clients reach here.
    let future = Box::pin(crate::agent_engine::agent_chat_inner_with_run(
      key_id,
      model,
      message,
      use_agent,
      None,
      auto_approve,
    ));
    let result = future.await.map_err(|e| McpError {
      code: -32000,
      message: format!("Agent chat failed: {e}"),
    })?;
    Ok(
      serde_json::json!({ "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap_or_default() }] }),
    )
  }

  async fn handle_agent_chat_confirm(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let card_ids: Vec<String> = arguments
      .get("card_ids")
      .cloned()
      .and_then(|v| serde_json::from_value(v).ok())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing or invalid card_ids".to_string(),
      })?;
    // Boxed: same recursion-break rationale as handle_agent_chat.
    let future = Box::pin(crate::agent_engine::agent_chat_confirm(card_ids));
    let result = future.await.map_err(|e| McpError {
      code: -32000,
      message: format!("Agent confirm failed: {e}"),
    })?;
    Ok(
      serde_json::json!({ "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap_or_default() }] }),
    )
  }

  async fn handle_agent_chat_decline(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let card_ids: Vec<String> = arguments
      .get("card_ids")
      .cloned()
      .and_then(|v| serde_json::from_value(v).ok())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing or invalid card_ids".to_string(),
      })?;
    let result = crate::agent_engine::agent_chat_decline(card_ids).map_err(|e| McpError {
      code: -32000,
      message: format!("Agent decline failed: {e}"),
    })?;
    Ok(
      serde_json::json!({ "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap_or_default() }] }),
    )
  }

  // VPN management handlers
  async fn handle_import_vpn(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let content = arguments
      .get("content")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing content".to_string(),
      })?;

    let filename = arguments
      .get("filename")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing filename".to_string(),
      })?;

    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let storage = crate::vpn::VPN_STORAGE.lock().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to lock VPN storage: {e}"),
    })?;

    let config = storage
      .import_config(content, filename, name)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to import VPN config: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "VPN '{}' ({}) imported successfully with ID: {}",
          config.name,
          config.vpn_type,
          config.id
        )
      }]
    }))
  }

  async fn handle_list_vpn_configs(&self) -> Result<serde_json::Value, McpError> {
    let storage = crate::vpn::VPN_STORAGE.lock().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to lock VPN storage: {e}"),
    })?;

    let configs = storage.list_configs().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to list VPN configs: {e}"),
    })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&configs).unwrap_or_default()
      }]
    }))
  }

  async fn handle_delete_vpn(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;

    // First disconnect if connected (stop VPN worker)
    let _ = crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(vpn_id).await;

    let storage = crate::vpn::VPN_STORAGE.lock().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to lock VPN storage: {e}"),
    })?;

    storage.delete_config(vpn_id).map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to delete VPN config: {e}"),
    })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("VPN '{}' deleted successfully", vpn_id)
      }]
    }))
  }

  async fn handle_connect_vpn(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;

    // Start VPN worker process
    crate::vpn_worker_runner::start_vpn_worker(vpn_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to connect VPN: {e}"),
      })?;

    // Update last_used timestamp
    {
      let storage = crate::vpn::VPN_STORAGE.lock().map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock VPN storage: {e}"),
      })?;
      let _ = storage.update_last_used(vpn_id);
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("VPN '{}' connected successfully", vpn_id)
      }]
    }))
  }

  async fn handle_disconnect_vpn(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;

    crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(vpn_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to disconnect VPN: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("VPN '{}' disconnected successfully", vpn_id)
      }]
    }))
  }

  async fn handle_get_vpn_status(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;

    let connected =
      if let Some(worker) = crate::vpn_worker_storage::find_vpn_worker_by_vpn_id(vpn_id) {
        worker
          .pid
          .map(crate::proxy_storage::is_process_running)
          .unwrap_or(false)
      } else {
        false
      };

    let status = crate::vpn::VpnStatus {
      connected,
      vpn_id: vpn_id.to_string(),
      connected_at: None,
      bytes_sent: None,
      bytes_received: None,
      last_handshake: None,
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&status).unwrap_or_default()
      }]
    }))
  }

  // Fingerprint management handlers
  async fn handle_get_profile_fingerprint(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    let fingerprint_info = match profile.browser.as_str() {
      "chromium" => {
        let config = profile
          .chromium_config
          .as_ref()
          .cloned()
          .unwrap_or_default();
        serde_json::json!({
          "browser": "chromium",
          "fingerprint": config.fingerprint,
          "os": config.os,
          "randomize_fingerprint_on_launch": config.randomize_fingerprint_on_launch,
          "screen_max_width": config.screen_max_width,
          "screen_max_height": config.screen_max_height,
          "screen_min_width": config.screen_min_width,
          "screen_min_height": config.screen_min_height,
        })
      }
      _ => {
        return Err(McpError {
          code: -32000,
          message: "MCP only supports Chromium profiles".to_string(),
        })
      }
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&fingerprint_info).unwrap_or_default()
      }]
    }))
  }

  async fn handle_update_profile_fingerprint(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let fingerprint = arguments.get("fingerprint").and_then(|v| v.as_str());
    let os = arguments.get("os").and_then(|v| v.as_str());
    let randomize = arguments
      .get("randomize_fingerprint_on_launch")
      .and_then(|v| v.as_bool());

    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    match profile.browser.as_str() {
      "chromium" => {
        let mut config = profile
          .chromium_config
          .as_ref()
          .cloned()
          .unwrap_or_default();
        if let Some(fp) = fingerprint {
          config.fingerprint = Some(fp.to_string());
        }
        if let Some(os_val) = os {
          config.os = Some(os_val.to_string());
        }
        if let Some(r) = randomize {
          config.randomize_fingerprint_on_launch = Some(r);
        }
        ProfileManager::instance()
          .update_chromium_config(app_handle.clone(), profile_id, config)
          .await
          .map_err(|e| McpError {
            code: -32000,
            message: format!("Failed to update Chromium config: {e}"),
          })?;
      }
      _ => {
        return Err(McpError {
          code: -32000,
          message: "MCP only supports Chromium profiles".to_string(),
        })
      }
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Fingerprint configuration updated for profile '{}'", profile.name)
      }]
    }))
  }

  async fn handle_update_profile_proxy_bypass_rules(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let rules: Vec<String> = arguments
      .get("rules")
      .and_then(|v| v.as_array())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing rules array".to_string(),
      })?
      .iter()
      .filter_map(|v| v.as_str().map(|s| s.to_string()))
      .collect();

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let profile = ProfileManager::instance()
      .update_profile_proxy_bypass_rules(app_handle, profile_id, rules.clone())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update proxy bypass rules: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Proxy bypass rules updated for profile '{}': {} rule(s) configured",
          profile.name,
          rules.len()
        )
      }]
    }))
  }

  async fn handle_update_profile_dns_blocklist(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let level = arguments
      .get("level")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing level".to_string(),
      })?;

    let dns_blocklist = if level == "none" {
      None
    } else {
      Some(level.to_string())
    };

    let profile = ProfileManager::instance()
      .update_profile_dns_blocklist(profile_id, dns_blocklist)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update DNS blocklist: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "DNS blocklist updated for profile '{}': {}",
          profile.name,
          level
        )
      }]
    }))
  }

  async fn handle_get_dns_blocklist_status(&self) -> Result<serde_json::Value, McpError> {
    let statuses = crate::dns_blocklist::BlocklistManager::get_cache_status();
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&statuses).unwrap_or_default()
      }]
    }))
  }

  async fn handle_list_extensions(&self) -> Result<serde_json::Value, McpError> {
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    let extensions = mgr.list_extensions().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to list extensions: {e}"),
    })?;
    Ok(serde_json::to_value(extensions).unwrap())
  }

  async fn handle_list_extension_groups(&self) -> Result<serde_json::Value, McpError> {
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    let groups = mgr.list_groups().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to list extension groups: {e}"),
    })?;
    Ok(serde_json::to_value(groups).unwrap())
  }

  async fn handle_create_extension_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing required parameter: name".to_string(),
      })?;
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    let group = mgr.create_group(name.to_string()).map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to create extension group: {e}"),
    })?;
    Ok(serde_json::to_value(group).unwrap())
  }

  async fn handle_delete_extension_mcp(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let extension_id = arguments
      .get("extension_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing required parameter: extension_id".to_string(),
      })?;
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    mgr
      .delete_extension_internal(extension_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete extension: {e}"),
      })?;
    Ok(serde_json::json!({"success": true}))
  }

  async fn handle_delete_extension_group_mcp(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing required parameter: group_id".to_string(),
      })?;
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    // For MCP, we don't have an app_handle, but we need one for sync deletion.
    // Use the delete_group_internal which skips sync remote deletion.
    mgr.delete_group_internal(group_id).map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to delete extension group: {e}"),
    })?;
    if let Err(e) = crate::events::emit_empty("extensions-changed") {
      log::error!("Failed to emit extensions-changed event: {e}");
    }
    Ok(serde_json::json!({"success": true}))
  }

  async fn handle_assign_extension_group_to_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing required parameter: profile_id".to_string(),
      })?;
    let extension_group_id = arguments
      .get("extension_group_id")
      .and_then(|v| v.as_str())
      .map(|s| {
        if s.is_empty() {
          None
        } else {
          Some(s.to_string())
        }
      })
      .unwrap_or(None);

    // Validate compatibility if assigning
    if let Some(ref gid) = extension_group_id {
      let profile_manager = ProfileManager::instance();
      let profiles = profile_manager.list_profiles().map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;
      let profile = profiles
        .iter()
        .find(|p| p.id.to_string() == profile_id)
        .ok_or_else(|| McpError {
          code: -32000,
          message: format!("Profile '{profile_id}' not found"),
        })?;
      let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
      mgr
        .validate_group_compatibility(gid, &profile.browser)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("{e}"),
        })?;
    }

    let profile_manager = ProfileManager::instance();
    let profile = profile_manager
      .update_profile_extension_group(profile_id, extension_group_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to assign extension group: {e}"),
      })?;
    Ok(serde_json::to_value(profile).unwrap())
  }

  async fn handle_get_team_locks(&self) -> Result<serde_json::Value, McpError> {
    let locks = crate::team_lock::TEAM_LOCK.get_locks().await;
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&locks).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_team_lock_status(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let lock_status = crate::team_lock::TEAM_LOCK
      .get_lock_status(profile_id)
      .await;
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&lock_status).unwrap_or_default()
      }]
    }))
  }

  // --- CDP utility methods for browser interaction ---

  async fn get_cdp_port_for_profile(&self, profile: &BrowserProfile) -> Result<u16, McpError> {
    Ok(CdpSession::new().get_cdp_port_for_profile(profile).await?)
  }

  async fn get_cdp_ws_url(&self, port: u16) -> Result<String, McpError> {
    Ok(CdpSession::new().get_cdp_ws_url(port).await?)
  }

  async fn send_cdp(
    &self,
    ws_url: &str,
    method: &str,
    params: serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    Ok(CdpSession::new().send_cdp(ws_url, method, params).await?)
  }

  async fn send_human_keystrokes(
    &self,
    ws_url: &str,
    text: &str,
    wpm: Option<f64>,
  ) -> Result<(), McpError> {
    Ok(
      CdpSession::new()
        .send_human_keystrokes(ws_url, text, wpm)
        .await?,
    )
  }

  /// Send a CDP command and wait for the page to finish loading.
  /// Uses a single WebSocket connection to: enable Page events, send the command,
  /// wait for the command response, then wait for `Page.loadEventFired`.
  async fn send_cdp_and_wait_for_load(
    &self,
    ws_url: &str,
    method: &str,
    params: serde_json::Value,
    timeout_secs: u64,
  ) -> Result<serde_json::Value, McpError> {
    Ok(
      CdpSession::new()
        .send_cdp_and_wait_for_load(ws_url, method, params, timeout_secs)
        .await?,
    )
  }

  fn get_running_profile(&self, profile_id: &str) -> Result<BrowserProfile, McpError> {
    Ok(CdpSession::new().get_running_profile(profile_id)?)
  }

  // --- Browser interaction handlers ---

  async fn handle_navigate(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let url = arguments
      .get("url")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing url".to_string(),
      })?;

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    self
      .send_cdp_and_wait_for_load(
        &ws_url,
        "Page.navigate",
        serde_json::json!({ "url": url }),
        30,
      )
      .await?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Navigated to {url}")
      }]
    }))
  }

  async fn handle_screenshot(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .unwrap_or("png");
    let quality = arguments.get("quality").and_then(|v| v.as_i64());
    let full_page = arguments
      .get("full_page")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let mut params = serde_json::json!({ "format": format });

    if let Some(q) = quality {
      params["quality"] = serde_json::json!(q);
    }

    if full_page {
      let layout = self
        .send_cdp(&ws_url, "Page.getLayoutMetrics", serde_json::json!({}))
        .await?;

      if let Some(content_size) = layout.get("contentSize") {
        params["clip"] = serde_json::json!({
          "x": 0,
          "y": 0,
          "width": content_size.get("width").and_then(|v| v.as_f64()).unwrap_or(1920.0),
          "height": content_size.get("height").and_then(|v| v.as_f64()).unwrap_or(1080.0),
          "scale": 1
        });
        params["captureBeyondViewport"] = serde_json::json!(true);
      }
    }

    let result = self
      .send_cdp(&ws_url, "Page.captureScreenshot", params)
      .await?;

    let data = result
      .get("data")
      .and_then(|v| v.as_str())
      .unwrap_or_default();

    Ok(serde_json::json!({
      "content": [{
        "type": "image",
        "data": data,
        "mimeType": format!("image/{format}")
      }]
    }))
  }

  async fn handle_evaluate_javascript(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let expression = arguments
      .get("expression")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing expression".to_string(),
      })?;
    let await_promise = arguments
      .get("await_promise")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    let wait_for_load = arguments
      .get("wait_for_load")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let cdp_params = serde_json::json!({
      "expression": expression,
      "returnByValue": true,
      "awaitPromise": await_promise,
    });

    let result = if wait_for_load {
      self
        .send_cdp_and_wait_for_load(&ws_url, "Runtime.evaluate", cdp_params, 30)
        .await?
    } else {
      self
        .send_cdp(&ws_url, "Runtime.evaluate", cdp_params)
        .await?
    };

    let value = if let Some(exception) = result.get("exceptionDetails") {
      let text = exception
        .get("text")
        .or_else(|| {
          exception
            .get("exception")
            .and_then(|e| e.get("description"))
        })
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error");
      serde_json::json!({ "error": text })
    } else if let Some(r) = result.get("result") {
      let val = r.get("value").cloned().unwrap_or(serde_json::json!(null));
      serde_json::json!({ "value": val, "type": r.get("type") })
    } else {
      serde_json::json!({ "value": null })
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&value).unwrap_or_default()
      }]
    }))
  }

  async fn handle_click_element(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let selector = arguments
      .get("selector")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing selector".to_string(),
      })?;

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let selector_escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
    let js = format!(
      r#"(() => {{
        const el = document.querySelector('{}');
        if (!el) throw new Error('Element not found: {}');
        el.scrollIntoView({{block: 'center'}});
        el.click();
        return true;
      }})()"#,
      selector_escaped, selector_escaped
    );

    // Use send_cdp_and_wait_for_load: if the click triggers navigation,
    // we wait for the new page to load. If not, the 10s timeout expires
    // and we return immediately.
    let result = self
      .send_cdp_and_wait_for_load(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({
          "expression": js,
          "returnByValue": true,
        }),
        10,
      )
      .await?;

    if let Some(exception) = result.get("exceptionDetails") {
      let msg = exception
        .get("exception")
        .and_then(|e| e.get("description"))
        .or_else(|| exception.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("Click failed");
      return Err(McpError {
        code: -32000,
        message: msg.to_string(),
      });
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Clicked element: {selector}")
      }]
    }))
  }

  async fn handle_type_text(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let selector = arguments
      .get("selector")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing selector".to_string(),
      })?;
    let text = arguments
      .get("text")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing text".to_string(),
      })?;
    let clear_first = arguments
      .get("clear_first")
      .and_then(|v| v.as_bool())
      .unwrap_or(true);
    let instant = arguments
      .get("instant")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    let wpm = arguments.get("wpm").and_then(|v| v.as_f64());

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let selector_escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
    let focus_js = if clear_first {
      format!(
        r#"(() => {{
          const el = document.querySelector('{}');
          if (!el) throw new Error('Element not found: {}');
          el.scrollIntoView({{block: 'center'}});
          el.focus();
          el.value = '';
          el.dispatchEvent(new Event('input', {{bubbles: true}}));
          return true;
        }})()"#,
        selector_escaped, selector_escaped
      )
    } else {
      format!(
        r#"(() => {{
          const el = document.querySelector('{}');
          if (!el) throw new Error('Element not found: {}');
          el.scrollIntoView({{block: 'center'}});
          el.focus();
          return true;
        }})()"#,
        selector_escaped, selector_escaped
      )
    };

    let focus_result = self
      .send_cdp(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({
          "expression": focus_js,
          "returnByValue": true,
        }),
      )
      .await?;

    if let Some(exception) = focus_result.get("exceptionDetails") {
      let msg = exception
        .get("exception")
        .and_then(|e| e.get("description"))
        .or_else(|| exception.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("Focus failed");
      return Err(McpError {
        code: -32000,
        message: msg.to_string(),
      });
    }

    if instant {
      self
        .send_cdp(
          &ws_url,
          "Input.insertText",
          serde_json::json!({ "text": text }),
        )
        .await?;
    } else {
      self.send_human_keystrokes(&ws_url, text, wpm).await?;
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Typed text into element: {selector}")
      }]
    }))
  }

  async fn handle_get_page_content(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .unwrap_or("text");
    let selector = arguments.get("selector").and_then(|v| v.as_str());
    let max_chars = arguments
      .get("max_chars")
      .and_then(|v| v.as_u64())
      .map(|n| n as usize)
      .unwrap_or(40_000);

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let js = if let Some(sel) = selector {
      let sel_escaped = sel.replace('\\', "\\\\").replace('\'', "\\'");
      if format == "html" {
        format!(
          r#"(() => {{
            const el = document.querySelector('{}');
            return el ? el.outerHTML : null;
          }})()"#,
          sel_escaped
        )
      } else {
        format!(
          r#"(() => {{
            const el = document.querySelector('{}');
            return el ? el.innerText : null;
          }})()"#,
          sel_escaped
        )
      }
    } else if format == "html" {
      "document.documentElement.outerHTML".to_string()
    } else {
      "document.body.innerText".to_string()
    };

    let result = self
      .send_cdp(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({
          "expression": js,
          "returnByValue": true,
        }),
      )
      .await?;

    let content = result
      .get("result")
      .and_then(|r| r.get("value"))
      .and_then(|v| v.as_str())
      .unwrap_or("");

    // Cap output so a 500 KB DOM dump doesn't blow out the agent's context.
    // Slice on character boundaries (chars().take().collect()) rather than
    // byte indices, since the latter would panic on multi-byte boundaries.
    let total_chars = content.chars().count();
    let (text, truncated) = if total_chars > max_chars {
      (content.chars().take(max_chars).collect::<String>(), true)
    } else {
      (content.to_string(), false)
    };

    let payload = if truncated {
      format!(
        "{text}\n\n[truncated: showing {max_chars} of {total_chars} chars — call with a larger max_chars or use get_interactive_elements for an indexed view]"
      )
    } else {
      text
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": payload
      }]
    }))
  }

  async fn handle_get_page_info(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let result = self
      .send_cdp(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({
          "expression": "JSON.stringify({url: location.href, title: document.title, readyState: document.readyState})",
          "returnByValue": true,
        }),
      )
      .await?;

    let info_str = result
      .get("result")
      .and_then(|r| r.get("value"))
      .and_then(|v| v.as_str())
      .unwrap_or("{}");

    let info: serde_json::Value = serde_json::from_str(info_str).unwrap_or(serde_json::json!({}));

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&info).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_interactive_elements(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let max_chars = arguments
      .get("max_chars")
      .and_then(|v| v.as_u64())
      .map(|n| n as usize)
      .unwrap_or(40_000);

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    // Walk the DOM for visible, non-disabled interactive elements, label them
    // with a zero-based index, and cache the live references on
    // `window.__duckling_interactive` so click_by_index / type_by_index can
    // resolve the index → Element without round-tripping a selector.
    let js = INTERACTIVE_ELEMENTS_JS.replace("__MAX_CHARS__", &max_chars.to_string());

    let result = self
      .send_cdp(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({
          "expression": js,
          "returnByValue": true,
        }),
      )
      .await?;

    if let Some(exception) = result.get("exceptionDetails") {
      let msg = exception
        .get("exception")
        .and_then(|e| e.get("description"))
        .or_else(|| exception.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("Enumeration failed");
      return Err(McpError {
        code: -32000,
        message: msg.to_string(),
      });
    }

    let payload_str = result
      .get("result")
      .and_then(|r| r.get("value"))
      .and_then(|v| v.as_str())
      .unwrap_or("{}");

    let payload: serde_json::Value =
      serde_json::from_str(payload_str).unwrap_or(serde_json::json!({}));
    let elements = payload
      .get("elements")
      .and_then(|v| v.as_str())
      .unwrap_or("");
    let count = payload.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    let truncated = payload
      .get("truncated")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    let header = if truncated {
      format!("{count} interactive elements (truncated at {max_chars} chars — re-call with a larger max_chars or scroll the page):")
    } else {
      format!("{count} interactive elements:")
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("{header}\n{elements}")
      }]
    }))
  }

  async fn handle_click_by_index(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let index = arguments
      .get("index")
      .and_then(|v| v.as_u64())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing index".to_string(),
      })?;

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let js = format!(
      r#"(() => {{
        const arr = window.__duckling_interactive;
        if (!arr || !arr[{index}]) throw new Error('No element at index {index}. Call get_interactive_elements first or after navigation.');
        const el = arr[{index}];
        el.scrollIntoView({{block: 'center'}});
        el.click();
        return true;
      }})()"#
    );

    let result = self
      .send_cdp_and_wait_for_load(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({
          "expression": js,
          "returnByValue": true,
        }),
        10,
      )
      .await?;

    if let Some(exception) = result.get("exceptionDetails") {
      let msg = exception
        .get("exception")
        .and_then(|e| e.get("description"))
        .or_else(|| exception.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("Click failed");
      return Err(McpError {
        code: -32000,
        message: msg.to_string(),
      });
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Clicked element at index {index}")
      }]
    }))
  }

  async fn handle_type_by_index(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let index = arguments
      .get("index")
      .and_then(|v| v.as_u64())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing index".to_string(),
      })?;
    let text = arguments
      .get("text")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing text".to_string(),
      })?;
    let clear_first = arguments
      .get("clear_first")
      .and_then(|v| v.as_bool())
      .unwrap_or(true);
    let instant = arguments
      .get("instant")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    let wpm = arguments.get("wpm").and_then(|v| v.as_f64());

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    // Mirrors handle_type_text's focus step but resolves the element via the
    // cached index instead of a CSS selector.
    let focus_js = if clear_first {
      format!(
        r#"(() => {{
          const arr = window.__duckling_interactive;
          if (!arr || !arr[{index}]) throw new Error('No element at index {index}. Call get_interactive_elements first or after navigation.');
          const el = arr[{index}];
          el.scrollIntoView({{block: 'center'}});
          el.focus();
          el.value = '';
          el.dispatchEvent(new Event('input', {{bubbles: true}}));
          return true;
        }})()"#
      )
    } else {
      format!(
        r#"(() => {{
          const arr = window.__duckling_interactive;
          if (!arr || !arr[{index}]) throw new Error('No element at index {index}. Call get_interactive_elements first or after navigation.');
          const el = arr[{index}];
          el.scrollIntoView({{block: 'center'}});
          el.focus();
          return true;
        }})()"#
      )
    };

    let focus_result = self
      .send_cdp(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({
          "expression": focus_js,
          "returnByValue": true,
        }),
      )
      .await?;

    if let Some(exception) = focus_result.get("exceptionDetails") {
      let msg = exception
        .get("exception")
        .and_then(|e| e.get("description"))
        .or_else(|| exception.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("Focus failed");
      return Err(McpError {
        code: -32000,
        message: msg.to_string(),
      });
    }

    if instant {
      self
        .send_cdp(
          &ws_url,
          "Input.insertText",
          serde_json::json!({ "text": text }),
        )
        .await?;
    } else {
      self.send_human_keystrokes(&ws_url, text, wpm).await?;
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Typed text into element at index {index}")
      }]
    }))
  }

  fn opt_selector(arguments: &serde_json::Value, key: &str) -> Option<String> {
    arguments
      .get(key)
      .and_then(|v| v.as_str())
      .map(|s| s.to_string())
  }

  fn opt_index(arguments: &serde_json::Value, key: &str) -> Option<u32> {
    arguments
      .get(key)
      .and_then(|v| v.as_u64())
      .map(|n| n as u32)
  }

  async fn handle_drag(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let session = crate::cdp_session::CdpSession::new();
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let (from_x, from_y) = session
      .element_point(
        &ws_url,
        Self::opt_selector(arguments, "from_selector").as_deref(),
        Self::opt_index(arguments, "from_index"),
      )
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    let (to_x, to_y) = match (
      arguments.get("to_x").and_then(|v| v.as_f64()),
      arguments.get("to_y").and_then(|v| v.as_f64()),
    ) {
      (Some(x), Some(y)) => (x, y),
      _ => session
        .element_point(
          &ws_url,
          Self::opt_selector(arguments, "to_selector").as_deref(),
          Self::opt_index(arguments, "to_index"),
        )
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: e.message,
        })?,
    };

    session
      .dispatch_mouse(&ws_url, "mousePressed", from_x, from_y, Some("left"))
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    for i in 1..=5 {
      let t = f64::from(i) / 5.0;
      session
        .dispatch_mouse(
          &ws_url,
          "mouseMoved",
          from_x + (to_x - from_x) * t,
          from_y + (to_y - from_y) * t,
          None,
        )
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: e.message,
        })?;
    }
    // Use the load-aware path: a drop that navigates still returns settled.
    let release = session
      .send_cdp(
        &ws_url,
        "Input.dispatchMouseEvent",
        serde_json::json!({
          "type": "mouseReleased", "x": to_x, "y": to_y,
          "button": "left", "clickCount": 1,
        }),
      )
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    let _ = release;

    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Dragged to ({to_x:.0}, {to_y:.0})") }]
    }))
  }

  async fn handle_scroll(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let direction = arguments
      .get("direction")
      .and_then(|v| v.as_str())
      .unwrap_or("down");
    let pixels = arguments
      .get("pixels")
      .and_then(|v| v.as_u64())
      .unwrap_or(500)
      .clamp(1, 10_000);
    let (dx, dy): (i64, i64) = match direction {
      "down" => (0, pixels as i64),
      "up" => (0, -(pixels as i64)),
      "right" => (pixels as i64, 0),
      "left" => (-(pixels as i64), 0),
      other => {
        return Err(McpError {
          code: -32602,
          message: format!("Unknown direction '{other}'. Use up, down, left, or right."),
        });
      }
    };

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let target = if let Some(index) = Self::opt_index(arguments, "index") {
      format!(
        r#"(window.__duckling_interactive && window.__duckling_interactive[{index}]) || (() => {{ throw new Error('No element at index {index}'); }})()"#
      )
    } else if let Some(selector) = Self::opt_selector(arguments, "selector") {
      let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
      format!(
        r#"document.querySelector('{escaped}') || (() => {{ throw new Error('Element not found: {escaped}'); }})()"#
      )
    } else {
      "window".to_string()
    };
    let js = format!(
      r#"(() => {{
        const el = {target};
        if (el === window) {{ window.scrollBy({dx}, {dy}); }}
        else {{ el.scrollBy({dx}, {dy}); }}
        return true;
      }})()"#
    );
    let result = self
      .send_cdp(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({ "expression": js, "returnByValue": true }),
      )
      .await?;
    if let Some(exception) = result.get("exceptionDetails") {
      let msg = exception
        .get("exception")
        .and_then(|e| e.get("description"))
        .or_else(|| exception.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("Scroll failed");
      return Err(McpError {
        code: -32000,
        message: msg.to_string(),
      });
    }
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Scrolled {direction} by {pixels}px") }]
    }))
  }

  async fn handle_press_key(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let key = arguments
      .get("key")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing key".to_string(),
      })?;
    let session = crate::cdp_session::CdpSession::new();
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    session
      .press_key(&ws_url, key)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Pressed {key}") }]
    }))
  }

  async fn handle_hover(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let session = crate::cdp_session::CdpSession::new();
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (x, y) = session
      .element_point(
        &ws_url,
        Self::opt_selector(arguments, "selector").as_deref(),
        Self::opt_index(arguments, "index"),
      )
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    session
      .dispatch_mouse(&ws_url, "mouseMoved", x, y, None)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Hovered ({x:.0}, {y:.0})") }]
    }))
  }

  fn download_profile(&self, profile_id: &str) -> Result<crate::profile::BrowserProfile, McpError> {
    let profiles = crate::profile::ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;
    profiles
      .into_iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32602,
        message: format!("Profile not found: {profile_id}"),
      })
  }

  async fn handle_set_download_dir(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let path = arguments.get("path").and_then(|v| v.as_str());
    let profile = self.download_profile(profile_id)?;
    let dir =
      crate::browser_downloads::resolve_download_dir(&profile, path).map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    // Apply immediately when the profile is running; otherwise the folder is
    // still resolved + created and takes effect on next launch/tool call.
    if profile.process_id.is_some() {
      let running = self.get_running_profile(profile_id)?;
      let cdp_port = self.get_cdp_port_for_profile(&running).await?;
      let ws_url = self.get_cdp_ws_url(cdp_port).await?;
      crate::cdp_session::CdpSession::new()
        .set_download_behavior(&ws_url, &dir)
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: e.message,
        })?;
    }
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Download folder: {}", dir.display()) }]
    }))
  }

  async fn handle_wait_for_download(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let timeout_ms = arguments
      .get("timeout_ms")
      .and_then(|v| v.as_u64())
      .unwrap_or(30_000)
      .clamp(1_000, 300_000);
    let profile = self.download_profile(profile_id)?;
    let dir =
      crate::browser_downloads::resolve_download_dir(&profile, None).map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    let known: std::collections::HashSet<String> = crate::browser_downloads::list_downloads(&dir)
      .iter()
      .map(|f| f.name.clone())
      .collect();
    let found = crate::browser_downloads::wait_for_new_downloads(
      &dir,
      &known,
      std::time::Duration::from_millis(timeout_ms),
    )
    .await;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string(&found).unwrap_or_default() }]
    }))
  }

  async fn handle_get_downloads(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let profile = self.download_profile(profile_id)?;
    let dir =
      crate::browser_downloads::resolve_download_dir(&profile, None).map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    let files = crate::browser_downloads::list_downloads(&dir);
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string(&files).unwrap_or_default() }]
    }))
  }

  fn require_profile_id(arguments: &serde_json::Value) -> Result<&str, McpError> {
    arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })
  }

  fn clamp_wait_timeout_ms(arguments: &serde_json::Value) -> u64 {
    arguments
      .get("timeout_ms")
      .and_then(|v| v.as_u64())
      .unwrap_or(15_000)
      .clamp(1, 120_000)
  }

  /// Evaluate an expression and unwrap the CDP result value (or surface the
  /// page-side exception as an MCP error).
  async fn evaluate_value(
    &self,
    ws_url: &str,
    expression: String,
  ) -> Result<serde_json::Value, McpError> {
    let result = self
      .send_cdp(
        ws_url,
        "Runtime.evaluate",
        serde_json::json!({ "expression": expression, "returnByValue": true }),
      )
      .await?;
    if let Some(exception) = result.get("exceptionDetails") {
      let text = exception
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("Page expression threw");
      return Err(McpError {
        code: -32000,
        message: format!("Page error: {text}"),
      });
    }
    Ok(
      result
        .pointer("/result/value")
        .cloned()
        .unwrap_or(serde_json::Value::Null),
    )
  }

  async fn handle_list_tabs(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let targets = crate::cdp_session::CdpSession::new()
      .list_targets(cdp_port)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&targets).unwrap_or_default() }]
    }))
  }

  async fn handle_new_tab(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let url = arguments.get("url").and_then(|v| v.as_str());
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let target = crate::cdp_session::CdpSession::new()
      .new_tab(cdp_port, url)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&target).unwrap_or_default() }]
    }))
  }

  async fn handle_switch_tab(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let target_id = arguments
      .get("target_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing target_id".to_string(),
      })?;
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    crate::cdp_session::CdpSession::new()
      .activate_target(cdp_port, target_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Switched to tab {target_id}") }]
    }))
  }

  async fn handle_close_tab(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let target_id = arguments
      .get("target_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing target_id".to_string(),
      })?;
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    crate::cdp_session::CdpSession::new()
      .close_target(cdp_port, target_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Closed tab {target_id}") }]
    }))
  }

  async fn handle_wait_for_text(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let text = arguments
      .get("text")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing text".to_string(),
      })?;
    let timeout_ms = Self::clamp_wait_timeout_ms(arguments);
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    crate::cdp_session::CdpSession::new()
      .wait_for_text(&ws_url, text, timeout_ms)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Page now contains {text:?}") }]
    }))
  }

  async fn handle_wait_for_url(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let contains = arguments
      .get("contains")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing contains".to_string(),
      })?;
    let timeout_ms = Self::clamp_wait_timeout_ms(arguments);
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    crate::cdp_session::CdpSession::new()
      .wait_for_url(&ws_url, contains, timeout_ms)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("URL now contains {contains:?}") }]
    }))
  }

  async fn handle_select_option(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let value = arguments
      .get("value")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing value".to_string(),
      })?;
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let expression = crate::cdp_session::CdpSession::select_option_expression(
      Self::opt_selector(arguments, "selector").as_deref(),
      Self::opt_index(arguments, "index"),
      value,
    )
    .map_err(|e| McpError {
      code: -32602,
      message: e.message,
    })?;
    let selected = self.evaluate_value(&ws_url, expression).await?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": format!("Selected option {selected}") }]
    }))
  }

  async fn handle_find_text(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let query = arguments
      .get("query")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing query".to_string(),
      })?;
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let found = self
      .evaluate_value(
        &ws_url,
        crate::cdp_session::CdpSession::find_text_expression(query),
      )
      .await?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&found).unwrap_or_default() }]
    }))
  }

  async fn handle_get_cookies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let result = self
      .send_cdp(&ws_url, "Network.getCookies", serde_json::json!({}))
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e.message,
      })?;
    let cookies = result
      .get("cookies")
      .cloned()
      .unwrap_or(serde_json::json!([]));
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&cookies).unwrap_or_default() }]
    }))
  }

  async fn handle_extract_table(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let selector = Self::opt_selector(arguments, "selector");
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let rows = self
      .evaluate_value(
        &ws_url,
        crate::cdp_session::CdpSession::extract_table_expression(selector.as_deref()),
      )
      .await?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&rows).unwrap_or_default() }]
    }))
  }

  async fn handle_extract_article(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let article = self
      .evaluate_value(
        &ws_url,
        crate::cdp_session::CdpSession::extract_article_expression(),
      )
      .await?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&article).unwrap_or_default() }]
    }))
  }

  // --- Synchronizer handlers ---

  async fn handle_start_sync_session(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let leader_id = arguments
      .get("leader_profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing leader_profile_id".to_string(),
      })?;
    let follower_ids: Vec<String> = arguments
      .get("follower_profile_ids")
      .and_then(|v| v.as_array())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing follower_profile_ids".to_string(),
      })?
      .iter()
      .filter_map(|v| v.as_str().map(|s| s.to_string()))
      .collect();

    let app = {
      let inner = self.inner.lock().await;
      inner.app_handle.clone().ok_or_else(|| McpError {
        code: -32000,
        message: "MCP server not properly initialized".to_string(),
      })?
    };

    let info = crate::synchronizer::SynchronizerManager::instance()
      .start_session(app, leader_id.to_string(), follower_ids)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&info).unwrap_or_default()
      }]
    }))
  }

  async fn handle_stop_sync_session(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let session_id = arguments
      .get("session_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing session_id".to_string(),
      })?;

    let app = {
      let inner = self.inner.lock().await;
      inner.app_handle.clone().ok_or_else(|| McpError {
        code: -32000,
        message: "MCP server not properly initialized".to_string(),
      })?
    };

    crate::synchronizer::SynchronizerManager::instance()
      .stop_session(app, session_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": "Sync session stopped"
      }]
    }))
  }

  async fn handle_get_sync_sessions(&self) -> Result<serde_json::Value, McpError> {
    let sessions = crate::synchronizer::SynchronizerManager::instance()
      .get_sessions()
      .await;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&sessions).unwrap_or_default()
      }]
    }))
  }

  async fn handle_remove_sync_follower(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let session_id = arguments
      .get("session_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing session_id".to_string(),
      })?;
    let follower_id = arguments
      .get("follower_profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing follower_profile_id".to_string(),
      })?;

    let app = {
      let inner = self.inner.lock().await;
      inner.app_handle.clone().ok_or_else(|| McpError {
        code: -32000,
        message: "MCP server not properly initialized".to_string(),
      })?
    };

    crate::synchronizer::SynchronizerManager::instance()
      .remove_follower(app, session_id, follower_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": "Follower removed from sync session"
      }]
    }))
  }

  async fn handle_clone_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let cloned = ProfileManager::instance()
      .clone_profile(profile_id, name)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to clone profile: {e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Profile cloned (id: {})", cloned.id)
      }]
    }))
  }

  async fn handle_get_app_settings(&self) -> Result<serde_json::Value, McpError> {
    let manager = SettingsManager::instance();
    let settings = manager.load_settings().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to load settings: {e}"),
    })?;
    // Presence-only: secret tokens never leave the backend. Best-effort —
    // false when the server runs without an app handle (e.g. unit tests).
    let (api_token_set, mcp_token_set) = match self.inner.lock().await.app_handle.clone() {
      Some(handle) => (
        manager
          .get_api_token(&handle)
          .await
          .ok()
          .flatten()
          .is_some(),
        manager
          .get_mcp_token(&handle)
          .await
          .ok()
          .flatten()
          .is_some(),
      ),
      None => (false, false),
    };
    let redacted = serde_json::json!({
      "theme": settings.theme,
      "custom_theme": settings.custom_theme,
      "language": settings.language,
      "api_enabled": settings.api_enabled,
      "api_port": settings.api_port,
      "api_token_set": api_token_set,
      "sync_server_url": settings.sync_server_url,
      "mcp_enabled": settings.mcp_enabled,
      "mcp_port": settings.mcp_port,
      "mcp_token_set": mcp_token_set,
      "set_as_default_browser": settings.set_as_default_browser,
      "disable_auto_updates": settings.disable_auto_updates,
      "keep_decrypted_profiles_in_ram": settings.keep_decrypted_profiles_in_ram,
      "keep_running_in_background": settings.keep_running_in_background,
      "llm_max_concurrency": settings.llm_max_concurrency,
      "llm_requests_per_hour": settings.llm_requests_per_hour,
      "max_concurrent_launches": settings.max_concurrent_launches,
      "automation_requests_per_hour": settings.automation_requests_per_hour,
      "onboarding_completed": settings.onboarding_completed,
    });
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&redacted).unwrap_or_default() }]
    }))
  }

  async fn handle_update_app_settings(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    const FORBIDDEN: &[&str] = &[
      "api_enabled",
      "api_port",
      "api_token",
      "mcp_enabled",
      "mcp_port",
      "mcp_token",
      "sync_server_url",
      "set_as_default_browser",
      "onboarding_completed",
      "window_resize_warning_dismissed",
    ];
    for key in FORBIDDEN {
      if arguments.get(*key).is_some() {
        return Err(McpError {
          code: -32602,
          message: format!("field '{key}' is not MCP-writable"),
        });
      }
    }
    let allowed = [
      "theme",
      "custom_theme",
      "language",
      "disable_auto_updates",
      "keep_decrypted_profiles_in_ram",
      "keep_running_in_background",
      "llm_max_concurrency",
      "llm_requests_per_hour",
      "max_concurrent_launches",
      "automation_requests_per_hour",
      "responseLanguage",
    ];
    let mut touched = false;
    for (k, _) in arguments
      .as_object()
      .map(|m| m.iter())
      .into_iter()
      .flatten()
    {
      if !allowed.contains(&k.as_str()) {
        return Err(McpError {
          code: -32602,
          message: format!("field '{k}' is not MCP-writable"),
        });
      }
      if k != "responseLanguage" {
        touched = true;
      }
    }
    if !touched {
      return Err(McpError {
        code: -32602,
        message: "No writable settings provided".to_string(),
      });
    }
    if let Some(theme) = arguments.get("theme").and_then(|v| v.as_str()) {
      if !matches!(theme, "light" | "dark" | "system" | "custom") {
        return Err(McpError {
          code: -32602,
          message: "theme must be light, dark, system or custom".to_string(),
        });
      }
      if theme == "custom" {
        let has_map = arguments
          .get("custom_theme")
          .and_then(|v| v.as_object())
          .map(|m| !m.is_empty())
          .unwrap_or(false);
        if !has_map {
          let current = SettingsManager::instance()
            .load_settings()
            .map_err(|e| McpError {
              code: -32000,
              message: format!("Failed to load settings: {e}"),
            })?;
          let current_has = current
            .custom_theme
            .as_ref()
            .map(|m| !m.is_empty())
            .unwrap_or(false);
          if !current_has {
            return Err(McpError {
              code: -32602,
              message: "theme 'custom' requires a non-empty custom_theme".to_string(),
            });
          }
        }
      }
    }
    if let Some(lang) = arguments.get("language").and_then(|v| v.as_str()) {
      const LANGS: &[&str] = &[
        "system", "en", "es", "pt", "fr", "zh", "ja", "ko", "ru", "tr", "vi",
      ];
      if !LANGS.contains(&lang) {
        return Err(McpError {
          code: -32602,
          message: "unsupported language".to_string(),
        });
      }
    }
    for key in ["llm_max_concurrency", "max_concurrent_launches"] {
      if let Some(v) = arguments.get(key).and_then(|v| v.as_u64()) {
        if v == 0 || v > 64 {
          return Err(McpError {
            code: -32602,
            message: format!("{key} must be 1-64"),
          });
        }
      }
    }
    let manager = SettingsManager::instance();
    let mut settings = manager.load_settings().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to load settings: {e}"),
    })?;
    if let Some(theme) = arguments.get("theme").and_then(|v| v.as_str()) {
      settings.theme = theme.to_string();
    }
    if let Some(map) = arguments.get("custom_theme").and_then(|v| v.as_object()) {
      if map.len() > 100 {
        return Err(McpError {
          code: -32602,
          message: "custom_theme too large (max 100 entries)".to_string(),
        });
      }
      let mut out = std::collections::HashMap::new();
      for (k, v) in map {
        let s = v.as_str().ok_or_else(|| McpError {
          code: -32602,
          message: "custom_theme values must be strings".to_string(),
        })?;
        out.insert(k.clone(), s.to_string());
      }
      settings.custom_theme = Some(out);
    }
    if let Some(lang) = arguments.get("language").and_then(|v| v.as_str()) {
      settings.language = if lang == "system" {
        None
      } else {
        Some(lang.to_string())
      };
    }
    if let Some(v) = arguments
      .get("disable_auto_updates")
      .and_then(|v| v.as_bool())
    {
      settings.disable_auto_updates = v;
    }
    if let Some(v) = arguments
      .get("keep_decrypted_profiles_in_ram")
      .and_then(|v| v.as_bool())
    {
      settings.keep_decrypted_profiles_in_ram = v;
    }
    if let Some(v) = arguments
      .get("keep_running_in_background")
      .and_then(|v| v.as_bool())
    {
      settings.keep_running_in_background = v;
    }
    if let Some(v) = arguments
      .get("llm_max_concurrency")
      .and_then(|v| v.as_u64())
    {
      settings.llm_max_concurrency = v as usize;
    }
    if let Some(v) = arguments
      .get("llm_requests_per_hour")
      .and_then(|v| v.as_u64())
    {
      settings.llm_requests_per_hour = v;
    }
    if let Some(v) = arguments
      .get("max_concurrent_launches")
      .and_then(|v| v.as_u64())
    {
      settings.max_concurrent_launches = v as usize;
    }
    if let Some(v) = arguments
      .get("automation_requests_per_hour")
      .and_then(|v| v.as_u64())
    {
      settings.automation_requests_per_hour = v;
    }
    manager.save_settings(&settings).map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to save settings: {e}"),
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Application settings updated" }]
    }))
  }

  async fn handle_get_table_sorting(&self) -> Result<serde_json::Value, McpError> {
    let sorting = SettingsManager::instance()
      .load_table_sorting()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to load table sorting: {e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&sorting).unwrap_or_default() }]
    }))
  }

  async fn handle_update_table_sorting(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let column = arguments
      .get("column")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing column".to_string(),
      })?;
    let direction = arguments
      .get("direction")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing direction".to_string(),
      })?;
    if !matches!(column, "name" | "browser" | "status") {
      return Err(McpError {
        code: -32602,
        message: "column must be name, browser or status".to_string(),
      });
    }
    if !matches!(direction, "asc" | "desc") {
      return Err(McpError {
        code: -32602,
        message: "direction must be asc or desc".to_string(),
      });
    }
    SettingsManager::instance()
      .save_table_sorting(&crate::settings_manager::TableSortingSettings {
        column: column.to_string(),
        direction: direction.to_string(),
      })
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to save table sorting: {e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Table sorting updated" }]
    }))
  }

  async fn handle_scheduler_list(&self) -> Result<serde_json::Value, McpError> {
    let tasks = crate::scheduler::SchedulerStore::instance().list_tasks();
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&tasks).unwrap_or_default() }]
    }))
  }

  async fn handle_scheduler_get(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let task_id = arguments
      .get("task_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing task_id".to_string(),
      })?;
    let task = crate::scheduler::SchedulerStore::instance()
      .get_task(task_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: "TASK_NOT_FOUND".to_string(),
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&task).unwrap_or_default() }]
    }))
  }

  async fn handle_scheduler_save(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let task_value = arguments.get("task").ok_or_else(|| McpError {
      code: -32602,
      message: "Missing task".to_string(),
    })?;
    let task: crate::scheduler::TaskDefinition = serde_json::from_value(task_value.clone())
      .map_err(|e| McpError {
        code: -32602,
        message: format!("Invalid task: {e}"),
      })?;
    let saved = crate::scheduler::SchedulerStore::instance()
      .save_task(&task)
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&saved).unwrap_or_default() }]
    }))
  }

  async fn handle_scheduler_delete(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let task_id = arguments
      .get("task_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing task_id".to_string(),
      })?;
    crate::scheduler::SchedulerStore::instance()
      .delete_task(task_id)
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Scheduled task deleted" }]
    }))
  }

  async fn handle_scheduler_set_enabled(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let task_id = arguments
      .get("task_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing task_id".to_string(),
      })?;
    let enabled = arguments
      .get("enabled")
      .and_then(|v| v.as_bool())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing enabled".to_string(),
      })?;
    let saved = crate::scheduler::SchedulerStore::instance()
      .set_enabled(task_id, enabled)
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&saved).unwrap_or_default() }]
    }))
  }

  async fn handle_scheduler_run_now(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let task_id = arguments
      .get("task_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing task_id".to_string(),
      })?
      .to_string();
    // Boxed to break the async recursion cycle (run_now -> agent run_tool_call
    // -> dispatch_tool_call -> handle_scheduler_run_now), same as agent_chat.
    let result = Box::pin(crate::scheduler::JobRunner::instance().run_now(&task_id))
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]
    }))
  }

  async fn handle_ai_keys_list(&self) -> Result<serde_json::Value, McpError> {
    let keys = crate::ai_keys::list_keys().map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&keys).unwrap_or_default() }]
    }))
  }

  async fn handle_ai_keys_save(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let provider = arguments
      .get("provider")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing provider".to_string(),
      })?;
    let model = arguments
      .get("model")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing model".to_string(),
      })?;
    let key = arguments
      .get("key")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing key".to_string(),
      })?;
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .unwrap_or("")
      .to_string();
    let endpoint = arguments
      .get("endpoint")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let saved = crate::ai_keys::save_key(provider, &name, model, key, endpoint.as_deref())
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&saved).unwrap_or_default() }]
    }))
  }

  async fn handle_ai_keys_delete(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let key_id = arguments
      .get("key_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing key_id".to_string(),
      })?;
    crate::ai_keys::delete_key(key_id).map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "AI key deleted" }]
    }))
  }

  async fn handle_ai_keys_test(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    if crate::llm_rate_limiter::check_llm_rate_limit().is_limited() {
      return Err(McpError {
        code: -32000,
        message: "LLM request quota exceeded; try again later".to_string(),
      });
    }
    let provider = arguments
      .get("provider")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing provider".to_string(),
      })?
      .to_string();
    let model = arguments
      .get("model")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing model".to_string(),
      })?
      .to_string();
    let key = arguments
      .get("key")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let id = arguments
      .get("key_id")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let endpoint = arguments
      .get("endpoint")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let result = crate::ai_keys::ai_keys_test(provider, model, key, id, endpoint)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]
    }))
  }

  async fn handle_ai_keys_models_get(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    if crate::llm_rate_limiter::check_llm_rate_limit().is_limited() {
      return Err(McpError {
        code: -32000,
        message: "LLM request quota exceeded; try again later".to_string(),
      });
    }
    let provider = arguments
      .get("provider")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing provider".to_string(),
      })?
      .to_string();
    let key = arguments
      .get("key")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let id = arguments
      .get("key_id")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let endpoint = arguments
      .get("endpoint")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let result = crate::ai_keys::ai_keys_models(provider, key, id, endpoint)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]
    }))
  }

  async fn require_app_handle(&self) -> Result<AppHandle, McpError> {
    self
      .inner
      .lock()
      .await
      .app_handle
      .clone()
      .ok_or_else(|| McpError {
        code: -32000,
        message: "MCP server not properly initialized".to_string(),
      })
  }

  fn redact_vpn_config(config: &crate::vpn::VpnConfig) -> serde_json::Value {
    serde_json::json!({
      "id": config.id,
      "name": config.name,
      "vpn_type": config.vpn_type.to_string(),
      "created_at": config.created_at,
      "last_used": config.last_used,
      "sync_enabled": config.sync_enabled,
      "last_sync": config.last_sync,
      "updated_at": config.updated_at,
      "config_data_set": !config.config_data.is_empty(),
      "config_data_len": config.config_data.len(),
    })
  }

  async fn handle_subscription_list(&self) -> Result<serde_json::Value, McpError> {
    let subs = crate::subscription_manager::subscriptions_list().map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&subs).unwrap_or_default() }]
    }))
  }

  async fn handle_subscription_entries(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let subscription_id = arguments
      .get("subscription_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing subscription_id".to_string(),
      })?;
    let entries = crate::subscription_manager::subscription_entries(subscription_id.to_string())
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&entries).unwrap_or_default() }]
    }))
  }

  async fn handle_subscription_save(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;
    let url = arguments
      .get("url")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing url".to_string(),
      })?;
    if url.trim().is_empty() {
      return Err(McpError {
        code: -32602,
        message: "url must not be empty".to_string(),
      });
    }
    let id = arguments
      .get("id")
      .and_then(|v| v.as_str())
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    let refresh_hours = arguments
      .get("refresh_hours")
      .and_then(|v| v.as_u64())
      .unwrap_or(24);
    if refresh_hours == 0 {
      return Err(McpError {
        code: -32602,
        message: "refresh_hours must be >= 1".to_string(),
      });
    }
    let use_proxy_id = arguments
      .get("use_proxy_id")
      .and_then(|v| v.as_str())
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    let auto_check = arguments
      .get("auto_check")
      .and_then(|v| v.as_bool())
      .unwrap_or(true);
    let auto_prune = arguments
      .get("auto_prune")
      .and_then(|v| v.as_bool())
      .unwrap_or(true);
    let saved = crate::subscription_manager::subscription_save(
      id,
      name.to_string(),
      url.to_string(),
      refresh_hours,
      use_proxy_id,
      auto_check,
      auto_prune,
    )
    .map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&saved).unwrap_or_default() }]
    }))
  }

  async fn handle_subscription_delete(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let id = arguments
      .get("id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing id".to_string(),
      })?;
    let delete_entries = arguments
      .get("delete_entries")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    let app_handle = self.require_app_handle().await?;
    crate::subscription_manager::subscription_delete(app_handle, id.to_string(), delete_entries)
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Subscription deleted" }]
    }))
  }

  async fn handle_subscription_refresh(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let id = arguments
      .get("id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing id".to_string(),
      })?;
    let app_handle = self.require_app_handle().await?;
    let result = crate::subscription_manager::subscription_refresh(app_handle, id.to_string())
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]
    }))
  }

  async fn handle_subscription_preview(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let url = arguments
      .get("url")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing url".to_string(),
      })?;
    let use_proxy_id = arguments
      .get("use_proxy_id")
      .and_then(|v| v.as_str())
      .filter(|s| !s.is_empty())
      .map(|s| s.to_string());
    let result = crate::subscription_manager::subscription_preview(url.to_string(), use_proxy_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]
    }))
  }

  async fn handle_dns_custom_get(&self) -> Result<serde_json::Value, McpError> {
    let config = crate::dns_blocklist::get_custom_dns_config()
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&config).unwrap_or_default() }]
    }))
  }

  async fn handle_dns_custom_set(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let str_list = |key: &str| -> Result<Vec<String>, McpError> {
      arguments
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| McpError {
          code: -32602,
          message: format!("Missing {key}"),
        })?
        .iter()
        .map(|v| {
          v.as_str().map(|s| s.to_string()).ok_or_else(|| McpError {
            code: -32602,
            message: format!("{key} entries must be strings"),
          })
        })
        .collect()
    };
    let sources = str_list("sources")?;
    let block_domains = str_list("block_domains")?;
    let allow_domains = str_list("allow_domains")?;
    let allowlist_mode = arguments
      .get("allowlist_mode")
      .and_then(|v| v.as_bool())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing allowlist_mode".to_string(),
      })?;
    let saved = crate::dns_blocklist::set_custom_dns_config(
      sources,
      block_domains,
      allow_domains,
      allowlist_mode,
    )
    .await
    .map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&saved).unwrap_or_default() }]
    }))
  }

  async fn handle_dns_custom_import(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let content = arguments
      .get("content")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing content".to_string(),
      })?;
    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing format".to_string(),
      })?;
    let imported =
      crate::dns_blocklist::import_custom_dns_rules(content.to_string(), format.to_string())
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: e,
        })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&imported).unwrap_or_default() }]
    }))
  }

  async fn handle_dns_custom_export(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing format".to_string(),
      })?;
    let exported = crate::dns_blocklist::export_custom_dns_rules(format.to_string())
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": exported }]
    }))
  }

  async fn handle_dns_cache_status(&self) -> Result<serde_json::Value, McpError> {
    let status = crate::dns_blocklist::get_dns_blocklist_cache_status()
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&status).unwrap_or_default() }]
    }))
  }

  async fn handle_dns_refresh(&self) -> Result<serde_json::Value, McpError> {
    crate::dns_blocklist::refresh_dns_blocklists()
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "DNS blocklists refreshed" }]
    }))
  }

  async fn handle_extension_get(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let extension_id = arguments
      .get("extension_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing extension_id".to_string(),
      })?;
    let ext = crate::extension_manager::EXTENSION_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Extension store unavailable: {e}"),
      })?
      .get_extension(extension_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("{e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&ext).unwrap_or_default() }]
    }))
  }

  async fn handle_extension_add(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    use base64::Engine as _;
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;
    let file_name = arguments
      .get("file_name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing file_name".to_string(),
      })?;
    let encoded = arguments
      .get("file_data_base64")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing file_data_base64".to_string(),
      })?;
    let file_data = base64::engine::general_purpose::STANDARD
      .decode(encoded)
      .map_err(|e| McpError {
        code: -32602,
        message: format!("Invalid base64 file data: {e}"),
      })?;
    if file_data.len() > 32 * 1024 * 1024 {
      return Err(McpError {
        code: -32602,
        message: "Extension file too large (max 32 MB)".to_string(),
      });
    }
    let ext =
      crate::extension_manager::add_extension(name.to_string(), file_name.to_string(), file_data)
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: e,
        })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&ext).unwrap_or_default() }]
    }))
  }

  async fn handle_extension_update(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let extension_id = arguments
      .get("extension_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing extension_id".to_string(),
      })?;
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;
    let updated = crate::extension_manager::update_extension(
      extension_id.to_string(),
      Some(name.to_string()),
      None,
      None,
    )
    .await
    .map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&updated).unwrap_or_default() }]
    }))
  }

  async fn handle_extension_update_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let extension_ids: Option<Vec<String>> = arguments.get("extension_ids").and_then(|v| {
      v.as_array().map(|arr| {
        arr
          .iter()
          .filter_map(|item| item.as_str().map(|s| s.to_string()))
          .collect()
      })
    });
    if name.is_none() && extension_ids.is_none() {
      return Err(McpError {
        code: -32602,
        message: "Provide name and/or extension_ids".to_string(),
      });
    }
    let updated =
      crate::extension_manager::update_extension_group(group_id.to_string(), name, extension_ids)
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: e,
        })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&updated).unwrap_or_default() }]
    }))
  }

  async fn handle_extension_add_to_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;
    let extension_id = arguments
      .get("extension_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing extension_id".to_string(),
      })?;
    let updated = crate::extension_manager::add_extension_to_group(
      group_id.to_string(),
      extension_id.to_string(),
    )
    .await
    .map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&updated).unwrap_or_default() }]
    }))
  }

  async fn handle_extension_remove_from_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;
    let extension_id = arguments
      .get("extension_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing extension_id".to_string(),
      })?;
    let updated = crate::extension_manager::remove_extension_from_group(
      group_id.to_string(),
      extension_id.to_string(),
    )
    .await
    .map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&updated).unwrap_or_default() }]
    }))
  }

  async fn handle_extension_group_for_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?.to_string();
    let group = crate::extension_manager::get_extension_group_for_profile(profile_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&group).unwrap_or_default() }]
    }))
  }

  async fn handle_vpn_get(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;
    let config = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("VPN storage unavailable: {e}"),
      })?
      .load_config(vpn_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("{e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&Self::redact_vpn_config(&config)).unwrap_or_default() }]
    }))
  }

  async fn handle_vpn_update(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;
    if name.trim().is_empty() {
      return Err(McpError {
        code: -32602,
        message: "name must not be empty".to_string(),
      });
    }
    let updated = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("VPN storage unavailable: {e}"),
      })?
      .update_config_name(vpn_id, name.trim())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("{e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&Self::redact_vpn_config(&updated)).unwrap_or_default() }]
    }))
  }

  async fn handle_vpn_validate(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;
    let result = crate::check_vpn_validity_core(vpn_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]
    }))
  }

  async fn handle_vpn_batch_import(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let content = arguments
      .get("content")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing content".to_string(),
      })?;
    let results = crate::import_vpn_config_batch(content.to_string())
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&results).unwrap_or_default() }]
    }))
  }

  async fn handle_vpn_list_active(&self) -> Result<serde_json::Value, McpError> {
    use crate::proxy_storage::is_process_running;
    let workers = crate::vpn_worker_storage::list_vpn_worker_configs();
    let active: Vec<crate::vpn::VpnStatus> = workers
      .into_iter()
      .filter(|w| w.pid.map(is_process_running).unwrap_or(false))
      .map(|w| crate::vpn::VpnStatus {
        connected: true,
        vpn_id: w.vpn_id,
        connected_at: None,
        bytes_sent: None,
        bytes_received: None,
        last_handshake: None,
      })
      .collect();
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&active).unwrap_or_default() }]
    }))
  }

  async fn handle_vpn_create_manual(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;
    if name.trim().is_empty() {
      return Err(McpError {
        code: -32602,
        message: "name must not be empty".to_string(),
      });
    }
    let vpn_type = arguments
      .get("vpn_type")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_type".to_string(),
      })?;
    let vpn_type = match vpn_type.to_lowercase().as_str() {
      "wireguard" => crate::vpn::VpnType::WireGuard,
      "vless" => crate::vpn::VpnType::Vless,
      _ => {
        return Err(McpError {
          code: -32602,
          message: "vpn_type must be wireguard or vless".to_string(),
        });
      }
    };
    let config_data = arguments
      .get("config_data")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing config_data".to_string(),
      })?;
    if config_data.trim().is_empty() {
      return Err(McpError {
        code: -32602,
        message: "config_data must not be empty".to_string(),
      });
    }
    let created = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("VPN storage unavailable: {e}"),
      })?
      .create_config_manual(name.trim(), vpn_type, config_data)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("{e}"),
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&Self::redact_vpn_config(&created)).unwrap_or_default() }]
    }))
  }

  async fn handle_sync_settings_get(
    &self,
    _arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let app_handle = self.require_app_handle().await?;
    let settings = crate::settings_manager::get_sync_settings(app_handle)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    let redacted = serde_json::json!({
      "sync_server_url": settings.sync_server_url,
      "sync_token_set": settings.sync_token.as_ref().map(|t| !t.is_empty()).unwrap_or(false),
    });
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&redacted).unwrap_or_default() }]
    }))
  }

  async fn handle_sync_status(&self) -> Result<serde_json::Value, McpError> {
    let configured = crate::sync::is_sync_configured();
    let counts = crate::sync::get_unsynced_entity_counts().map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&serde_json::json!({
        "configured": configured,
        "unsynced": counts,
      })).unwrap_or_default() }]
    }))
  }

  async fn handle_sync_set_proxy_enabled(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_id".to_string(),
      })?;
    let enabled = arguments
      .get("enabled")
      .and_then(|v| v.as_bool())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing enabled".to_string(),
      })?;
    let app_handle = self.require_app_handle().await?;
    crate::sync::set_proxy_sync_enabled(app_handle, proxy_id.to_string(), enabled)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Proxy sync setting updated" }]
    }))
  }

  async fn handle_sync_set_group_enabled(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;
    let enabled = arguments
      .get("enabled")
      .and_then(|v| v.as_bool())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing enabled".to_string(),
      })?;
    let app_handle = self.require_app_handle().await?;
    crate::sync::set_group_sync_enabled(app_handle, group_id.to_string(), enabled)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Group sync setting updated" }]
    }))
  }

  async fn handle_sync_set_vpn_enabled(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;
    let enabled = arguments
      .get("enabled")
      .and_then(|v| v.as_bool())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing enabled".to_string(),
      })?;
    let app_handle = self.require_app_handle().await?;
    crate::sync::set_vpn_sync_enabled(app_handle, vpn_id.to_string(), enabled)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "VPN sync setting updated" }]
    }))
  }

  async fn handle_sync_set_extension_enabled(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let extension_id = arguments
      .get("extension_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing extension_id".to_string(),
      })?;
    let enabled = arguments
      .get("enabled")
      .and_then(|v| v.as_bool())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing enabled".to_string(),
      })?;
    let app_handle = self.require_app_handle().await?;
    crate::sync::set_extension_sync_enabled(app_handle, extension_id.to_string(), enabled)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Extension sync setting updated" }]
    }))
  }

  async fn handle_sync_set_extension_group_enabled(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let extension_group_id = arguments
      .get("extension_group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing extension_group_id".to_string(),
      })?;
    let enabled = arguments
      .get("enabled")
      .and_then(|v| v.as_bool())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing enabled".to_string(),
      })?;
    let app_handle = self.require_app_handle().await?;
    crate::sync::set_extension_group_sync_enabled(
      app_handle,
      extension_group_id.to_string(),
      enabled,
    )
    .await
    .map_err(|e| McpError {
      code: -32000,
      message: e,
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Extension group sync setting updated" }]
    }))
  }

  async fn handle_sync_request_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?.to_string();
    let app_handle = self.require_app_handle().await?;
    crate::sync::request_profile_sync(app_handle, profile_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Profile sync queued" }]
    }))
  }

  async fn handle_sync_enable_all(&self) -> Result<serde_json::Value, McpError> {
    let app_handle = self.require_app_handle().await?;
    crate::sync::enable_sync_for_all_entities(app_handle)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Sync enabled for all metadata entities" }]
    }))
  }

  async fn handle_e2e_has_password(&self) -> Result<serde_json::Value, McpError> {
    let has = crate::sync::check_has_e2e_password();
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&serde_json::json!({ "has_password": has })).unwrap_or_default() }]
    }))
  }

  async fn handle_e2e_set_password(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let password = arguments
      .get("password")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing password".to_string(),
      })?;
    if password.is_empty() {
      return Err(McpError {
        code: -32602,
        message: "password must not be empty".to_string(),
      });
    }
    crate::sync::set_e2e_password(password.to_string())
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Sync encryption password set" }]
    }))
  }

  async fn handle_e2e_delete_password(&self) -> Result<serde_json::Value, McpError> {
    crate::sync::delete_e2e_password()
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Sync encryption password deleted" }]
    }))
  }

  async fn handle_browser_versions(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let browser = arguments
      .get("browser")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing browser".to_string(),
      })?;
    let versions =
      crate::downloaded_browsers_registry::get_downloaded_browser_versions(browser.to_string())
        .map_err(|e| McpError {
          code: -32000,
          message: e,
        })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&versions).unwrap_or_default() }]
    }))
  }

  async fn handle_browser_download_check(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let browser = arguments
      .get("browser")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing browser".to_string(),
      })?;
    let version = arguments
      .get("version")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing version".to_string(),
      })?;
    let downloaded = crate::downloaded_browsers_registry::is_browser_downloaded(
      browser.to_string(),
      version.to_string(),
    );
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&serde_json::json!({ "downloaded": downloaded })).unwrap_or_default() }]
    }))
  }

  async fn handle_browser_missing_binaries(&self) -> Result<serde_json::Value, McpError> {
    let missing = crate::downloaded_browsers_registry::check_missing_binaries()
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&missing).unwrap_or_default() }]
    }))
  }

  async fn handle_traffic_get_all(&self) -> Result<serde_json::Value, McpError> {
    let snapshots = crate::traffic_stats::get_all_traffic_snapshots_realtime();
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&snapshots).unwrap_or_default() }]
    }))
  }

  async fn handle_traffic_get_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?;
    let snapshot = crate::traffic_stats::get_traffic_snapshot_for_profile(profile_id);
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&snapshot).unwrap_or_default() }]
    }))
  }

  async fn handle_traffic_clear_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = Self::require_profile_id(arguments)?.to_string();
    if crate::traffic_stats::load_traffic_stats_by_profile(&profile_id).is_none() {
      return Ok(serde_json::json!({
        "content": [{ "type": "text", "text": "No traffic history for profile" }]
      }));
    }
    crate::traffic_stats::delete_traffic_stats(&profile_id);
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "Profile traffic history erased" }]
    }))
  }

  async fn handle_traffic_clear_all(&self) -> Result<serde_json::Value, McpError> {
    crate::traffic_stats::clear_all_traffic_stats().map_err(|e| McpError {
      code: -32000,
      message: format!("{e}"),
    })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": "All traffic history erased" }]
    }))
  }

  async fn handle_default_browser_check(&self) -> Result<serde_json::Value, McpError> {
    let is_default = crate::default_browser::is_default_browser()
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": serde_json::to_string_pretty(&serde_json::json!({ "is_default": is_default })).unwrap_or_default() }]
    }))
  }

  async fn handle_logs_read(&self) -> Result<serde_json::Value, McpError> {
    let app_handle = self.require_app_handle().await?;
    let logs = crate::settings_manager::read_log_files(app_handle)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(serde_json::json!({
      "content": [{ "type": "text", "text": logs }]
    }))
  }
}

lazy_static::lazy_static! {
  static ref MCP_SERVER: McpServer = McpServer::new();
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_mcp_tools_count() {
    let server = McpServer::new();
    let tools = server.get_tools();

    // Should have at least 104 tools (41 base + 17 settings/scheduler/AI + 46 management tools)
    assert!(tools.len() >= 104);

    // Check tool names
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    // Profile tools
    assert!(tool_names.contains(&"list_profiles"));
    assert!(tool_names.contains(&"get_profile"));
    assert!(tool_names.contains(&"run_profile"));
    assert!(tool_names.contains(&"kill_profile"));
    assert!(tool_names.contains(&"get_profile_status"));
    // Profile import tools
    assert!(tool_names.contains(&"detect_browser_profiles"));
    assert!(tool_names.contains(&"import_browser_profiles"));
    // Group tools
    assert!(tool_names.contains(&"list_groups"));
    assert!(tool_names.contains(&"get_group"));
    assert!(tool_names.contains(&"create_group"));
    assert!(tool_names.contains(&"update_group"));
    assert!(tool_names.contains(&"delete_group"));
    assert!(tool_names.contains(&"assign_profiles_to_group"));
    // Proxy tools
    assert!(tool_names.contains(&"list_proxies"));
    assert!(tool_names.contains(&"get_proxy"));
    assert!(tool_names.contains(&"create_proxy"));
    assert!(tool_names.contains(&"update_proxy"));
    assert!(tool_names.contains(&"delete_proxy"));
    // Proxy import/export tools
    assert!(tool_names.contains(&"export_proxies"));
    assert!(tool_names.contains(&"import_proxies"));
    // Proxy pool tools
    assert!(tool_names.contains(&"create_proxy_pool"));
    assert!(tool_names.contains(&"list_proxy_pools"));
    assert!(tool_names.contains(&"update_proxy_pool"));
    assert!(tool_names.contains(&"delete_proxy_pool"));
    assert!(tool_names.contains(&"assign_profiles_to_pool"));
    assert!(tool_names.contains(&"rotate_profile_proxy"));
    // LLM tools
    assert!(tool_names.contains(&"llm_completion"));
    // Agent tools
    assert!(tool_names.contains(&"agent_chat"));
    assert!(tool_names.contains(&"agent_chat_confirm"));
    assert!(tool_names.contains(&"agent_chat_decline"));
    // VPN tools
    assert!(tool_names.contains(&"import_vpn"));
    assert!(tool_names.contains(&"list_vpn_configs"));
    assert!(tool_names.contains(&"delete_vpn"));
    assert!(tool_names.contains(&"connect_vpn"));
    assert!(tool_names.contains(&"disconnect_vpn"));
    assert!(tool_names.contains(&"get_vpn_status"));
    // Fingerprint tools
    assert!(tool_names.contains(&"get_profile_fingerprint"));
    assert!(tool_names.contains(&"update_profile_fingerprint"));
    assert!(tool_names.contains(&"update_profile_proxy_bypass_rules"));
    // Extension tools
    assert!(tool_names.contains(&"list_extensions"));
    assert!(tool_names.contains(&"list_extension_groups"));
    assert!(tool_names.contains(&"create_extension_group"));
    assert!(tool_names.contains(&"delete_extension"));
    assert!(tool_names.contains(&"delete_extension_group"));
    assert!(tool_names.contains(&"assign_extension_group_to_profile"));
    // Cookie tools
    assert!(tool_names.contains(&"import_profile_cookies"));
    assert!(tool_names.contains(&"clone_profile"));
    assert!(tool_names.contains(&"get_app_settings"));
    assert!(tool_names.contains(&"update_app_settings"));
    assert!(tool_names.contains(&"get_table_sorting"));
    assert!(tool_names.contains(&"update_table_sorting"));
    assert!(tool_names.contains(&"scheduler_list"));
    assert!(tool_names.contains(&"scheduler_get"));
    assert!(tool_names.contains(&"scheduler_save"));
    assert!(tool_names.contains(&"scheduler_delete"));
    assert!(tool_names.contains(&"scheduler_set_enabled"));
    assert!(tool_names.contains(&"scheduler_run_now"));
    assert!(tool_names.contains(&"ai_keys_list"));
    assert!(tool_names.contains(&"ai_keys_save"));
    assert!(tool_names.contains(&"ai_keys_delete"));
    assert!(tool_names.contains(&"ai_keys_test"));
    assert!(tool_names.contains(&"ai_keys_models_get"));
    // Subscription tools
    assert!(tool_names.contains(&"subscription_list"));
    assert!(tool_names.contains(&"subscription_entries"));
    assert!(tool_names.contains(&"subscription_save"));
    assert!(tool_names.contains(&"subscription_delete"));
    assert!(tool_names.contains(&"subscription_refresh"));
    assert!(tool_names.contains(&"subscription_preview"));
    // DNS tools
    assert!(tool_names.contains(&"dns_custom_get"));
    assert!(tool_names.contains(&"dns_custom_set"));
    assert!(tool_names.contains(&"dns_custom_import"));
    assert!(tool_names.contains(&"dns_custom_export"));
    assert!(tool_names.contains(&"dns_cache_status"));
    assert!(tool_names.contains(&"dns_refresh"));
    // Extension tools
    assert!(tool_names.contains(&"extension_get"));
    assert!(tool_names.contains(&"extension_add"));
    assert!(tool_names.contains(&"extension_update"));
    assert!(tool_names.contains(&"extension_update_group"));
    assert!(tool_names.contains(&"extension_add_to_group"));
    assert!(tool_names.contains(&"extension_remove_from_group"));
    assert!(tool_names.contains(&"extension_group_for_profile"));
    // VPN tools
    assert!(tool_names.contains(&"vpn_get"));
    assert!(tool_names.contains(&"vpn_update"));
    assert!(tool_names.contains(&"vpn_validate"));
    assert!(tool_names.contains(&"vpn_batch_import"));
    assert!(tool_names.contains(&"vpn_list_active"));
    assert!(tool_names.contains(&"vpn_create_manual"));
    // Sync tools
    assert!(tool_names.contains(&"sync_settings_get"));
    assert!(tool_names.contains(&"sync_status"));
    assert!(tool_names.contains(&"sync_set_proxy_enabled"));
    assert!(tool_names.contains(&"sync_set_group_enabled"));
    assert!(tool_names.contains(&"sync_set_vpn_enabled"));
    assert!(tool_names.contains(&"sync_set_extension_enabled"));
    assert!(tool_names.contains(&"sync_set_extension_group_enabled"));
    assert!(tool_names.contains(&"sync_request_profile"));
    assert!(tool_names.contains(&"sync_enable_all"));
    // E2E encryption tools
    assert!(tool_names.contains(&"e2e_has_password"));
    assert!(tool_names.contains(&"e2e_set_password"));
    assert!(tool_names.contains(&"e2e_delete_password"));
    // Maintenance tools
    assert!(tool_names.contains(&"browser_versions"));
    assert!(tool_names.contains(&"browser_download_check"));
    assert!(tool_names.contains(&"browser_missing_binaries"));
    assert!(tool_names.contains(&"traffic_get_all"));
    assert!(tool_names.contains(&"traffic_get_profile"));
    assert!(tool_names.contains(&"traffic_clear_profile"));
    assert!(tool_names.contains(&"traffic_clear_all"));
    assert!(tool_names.contains(&"default_browser_check"));
    assert!(tool_names.contains(&"logs_read"));
    // Team lock tools
    assert!(tool_names.contains(&"get_team_locks"));
    assert!(tool_names.contains(&"get_team_lock_status"));
    // Synchronizer tools
    assert!(tool_names.contains(&"start_sync_session"));
    assert!(tool_names.contains(&"stop_sync_session"));
    assert!(tool_names.contains(&"get_sync_sessions"));
    assert!(tool_names.contains(&"remove_sync_follower"));
    // Browser interaction tools
    assert!(tool_names.contains(&"navigate"));
    assert!(tool_names.contains(&"screenshot"));
    assert!(tool_names.contains(&"evaluate_javascript"));
    assert!(tool_names.contains(&"click_element"));
    assert!(tool_names.contains(&"type_text"));
    assert!(tool_names.contains(&"drag"));
    assert!(tool_names.contains(&"scroll"));
    assert!(tool_names.contains(&"press_key"));
    assert!(tool_names.contains(&"hover"));
    assert!(tool_names.contains(&"set_download_dir"));
    assert!(tool_names.contains(&"wait_for_download"));
    assert!(tool_names.contains(&"get_downloads"));
    assert!(tool_names.contains(&"list_tabs"));
    assert!(tool_names.contains(&"new_tab"));
    assert!(tool_names.contains(&"switch_tab"));
    assert!(tool_names.contains(&"close_tab"));
    assert!(tool_names.contains(&"wait_for_text"));
    assert!(tool_names.contains(&"wait_for_url"));
    assert!(tool_names.contains(&"select_option"));
    assert!(tool_names.contains(&"find_text"));
    assert!(tool_names.contains(&"get_cookies"));
    assert!(tool_names.contains(&"extract_table"));
    assert!(tool_names.contains(&"extract_article"));
    assert!(tool_names.contains(&"get_page_content"));
    assert!(tool_names.contains(&"get_page_info"));
  }

  #[test]
  fn test_mcp_server_initial_state() {
    let server = McpServer::new();
    assert!(!server.is_running());
  }

  #[test]
  fn browser_tools_come_from_shared_catalog() {
    let tools = McpServer::new().get_tools();
    let catalog = crate::browser_tools::browser_tools();
    assert!(!catalog.is_empty());
    for entry in &catalog {
      let found = tools
        .iter()
        .find(|t| t.name == entry.name)
        .unwrap_or_else(|| panic!("catalog tool {} missing from MCP", entry.name));
      assert_eq!(found.description, entry.description);
      assert_eq!(found.input_schema, entry.input_schema);
    }
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), tools.len(), "duplicate MCP tool names");
  }

  #[test]
  fn rate_limit_only_classifies_browser_automation_tools() {
    let request = |method: &str, name: Option<&str>| McpRequest {
      jsonrpc: "2.0".to_string(),
      id: Some(serde_json::json!(1)),
      method: method.to_string(),
      params: name.map(|name| serde_json::json!({ "name": name, "arguments": {} })),
    };

    for name in [
      "run_profile",
      "kill_profile",
      "batch_run_profiles",
      "batch_stop_profiles",
      "start_sync_session",
      "scheduler_run_now",
      "navigate",
      "screenshot",
      "evaluate_javascript",
      "click_element",
      "type_text",
      "get_page_content",
      "get_page_info",
      "get_interactive_elements",
      "click_by_index",
      "type_by_index",
      "drag",
      "scroll",
      "press_key",
      "hover",
      "set_download_dir",
      "wait_for_download",
      "get_downloads",
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
        McpServer::is_automation_tool_call(&request("tools/call", Some(name))),
        "automation tool was not limited: {name}"
      );
    }

    assert!(!McpServer::is_automation_tool_call(&request(
      "tools/call",
      Some("list_profiles")
    )));
    for name in [
      "get_app_settings",
      "update_app_settings",
      "get_table_sorting",
      "update_table_sorting",
      "scheduler_list",
      "scheduler_save",
      "ai_keys_list",
      "ai_keys_save",
      "clone_profile",
    ] {
      assert!(
        !McpServer::is_automation_tool_call(&request("tools/call", Some(name))),
        "settings tool must not consume automation quota: {name}"
      );
    }
    assert!(!McpServer::is_automation_tool_call(&request(
      "tools/list",
      None
    )));
  }

  async fn call(tool: &str, args: serde_json::Value) -> Result<serde_json::Value, McpError> {
    McpServer::new().dispatch_tool_call(tool, &args).await
  }

  #[tokio::test]
  async fn mcp_rejects_forbidden_app_settings() {
    for field in [
      "api_enabled",
      "api_port",
      "mcp_enabled",
      "mcp_port",
      "sync_server_url",
      "set_as_default_browser",
      "onboarding_completed",
    ] {
      let err = call("update_app_settings", serde_json::json!({ field: true }))
        .await
        .expect_err("forbidden field must be rejected");
      assert!(err.message.contains("not MCP-writable"), "{field}: {err}");
    }
  }

  #[tokio::test]
  async fn mcp_validates_app_settings_values() {
    let err = call(
      "update_app_settings",
      serde_json::json!({ "theme": "neon" }),
    )
    .await
    .expect_err("bad theme must be rejected");
    assert!(err.message.contains("theme"), "{err}");
    let err = call(
      "update_app_settings",
      serde_json::json!({ "language": "xx" }),
    )
    .await
    .expect_err("bad language must be rejected");
    assert!(err.message.contains("language"), "{err}");
    let err = call(
      "update_app_settings",
      serde_json::json!({ "llm_max_concurrency": 0 }),
    )
    .await
    .expect_err("zero concurrency must be rejected");
    assert!(err.message.contains("1-64"), "{err}");
    let err = call(
      "update_app_settings",
      serde_json::json!({ "max_concurrent_launches": 65 }),
    )
    .await
    .expect_err("over-limit launches must be rejected");
    assert!(err.message.contains("1-64"), "{err}");
    let err = call("update_app_settings", serde_json::json!({}))
      .await
      .expect_err("empty update must be rejected");
    assert!(err.message.contains("No writable"), "{err}");
  }

  #[tokio::test]
  async fn mcp_validates_profile_network_rules() {
    let err = call(
      "create_profile",
      serde_json::json!({ "name": "x", "browser": "chromium", "proxy_id": "p", "vpn_id": "v" }),
    )
    .await
    .expect_err("proxy+vpn must be rejected");
    assert!(err.message.contains("proxy_id and vpn_id"), "{err}");
    let err = call(
      "create_profile",
      serde_json::json!({ "name": "x", "browser": "chromium", "ephemeral": true, "clear_on_close": true }),
    )
    .await
    .expect_err("ephemeral clear_on_close must be rejected");
    assert!(err.message.contains("ephemeral"), "{err}");
  }

  #[tokio::test]
  async fn mcp_validates_subscription_save() {
    let err = call(
      "subscription_save",
      serde_json::json!({ "url": "https://example.com/sub" }),
    )
    .await
    .expect_err("missing name must be rejected");
    assert!(err.message.contains("Missing name"), "{err}");
    let err = call(
      "subscription_save",
      serde_json::json!({ "name": "s", "url": "https://example.com/sub", "refresh_hours": 0 }),
    )
    .await
    .expect_err("zero refresh_hours must be rejected");
    assert!(err.message.contains("refresh_hours"), "{err}");
  }

  #[tokio::test]
  async fn mcp_validates_dns_extension_vpn_e2e_inputs() {
    let err = call("dns_custom_set", serde_json::json!({}))
      .await
      .expect_err("missing dns fields must be rejected");
    assert!(err.message.contains("Missing"), "{err}");
    let err = call(
      "extension_add",
      serde_json::json!({ "name": "e", "file_name": "e.zip", "file_data_base64": "!!!" }),
    )
    .await
    .expect_err("bad base64 must be rejected");
    assert!(err.message.contains("base64"), "{err}");
    let err = call(
      "extension_update_group",
      serde_json::json!({ "group_id": "g" }),
    )
    .await
    .expect_err("empty group update must be rejected");
    assert!(err.message.contains("name and/or"), "{err}");
    let err = call(
      "vpn_create_manual",
      serde_json::json!({ "name": "v", "vpn_type": "pptp", "config_data": "x" }),
    )
    .await
    .expect_err("bad vpn_type must be rejected");
    assert!(err.message.contains("wireguard or vless"), "{err}");
    let err = call("e2e_set_password", serde_json::json!({ "password": "" }))
      .await
      .expect_err("empty password must be rejected");
    assert!(err.message.contains("must not be empty"), "{err}");
    let err = call(
      "vpn_update",
      serde_json::json!({ "vpn_id": "v", "name": "  " }),
    )
    .await
    .expect_err("blank vpn name must be rejected");
    assert!(err.message.contains("must not be empty"), "{err}");
  }

  #[tokio::test]
  async fn mcp_unknown_ids_do_not_leak_secrets() {
    let err = call("vpn_get", serde_json::json!({ "vpn_id": "nope" }))
      .await
      .expect_err("unknown vpn must error");
    assert!(!err.message.contains("config_data"), "{err}");
    let err = call("scheduler_get", serde_json::json!({ "task_id": "nope" }))
      .await
      .expect_err("unknown task must error");
    assert!(err.message.contains("TASK_NOT_FOUND"), "{err}");
  }
}
